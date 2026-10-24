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
//! Views come from one of four sources: synthetic fixtures when no profile was
//! given; nothing while the session is locked; an empty list while signed out;
//! and the signed-in Matrix client otherwise.
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
pub mod contacts;
pub mod credentials;
pub mod homeserver;
pub mod identities;
pub mod matrix_views;
pub mod media;
pub mod outbox;
pub mod passphrase;
pub mod pick;
pub mod security;
pub mod sending;
pub mod session;
pub mod store;
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
    sync::{Arc, Condvar, Mutex, MutexGuard},
    time::Duration,
};

pub const MILESTONE: &str = "P1";

/// Longest a `lock_session` call waits for admitted work before replying
/// `locking`; the UI may call again to resume the wait. Covers one sync round.
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(15);

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
        }
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

    fn outcome(&self, call: &Call) -> Outcome {
        match call {
            Call::AppStatus {} => ok(&AppStatus {
                version: env!("CARGO_PKG_VERSION").into(),
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
            Call::SecurityStatus {} => ok(&self.session.security_status()),
            Call::SetUpEncryption {} => {
                ok(&self.session.set_up_encryption(self.credentials.clone()))
            }
            Call::RecoverWithKey {} => ok(&self.session.recover_with_key(self.credentials.clone())),
            Call::SendMessage { room_id, body } => self.send_message(room_id, body),
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
            Call::SetUpRecovery {} => ok(&self.session.set_up_recovery(self.credentials.clone())),
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
            Call::ImageBand { .. } => Outcome::Error(ErrorCode::Unavailable),
            Call::ReleaseImage { handle } => {
                refused(self.media.release(self.media_generation(), handle))
            }
            Call::DiscardImage { handle } => {
                refused(self.media.discard(self.media_generation(), handle))
            }
            Call::SendImage { room_id, handle } => self.send_image(room_id, handle),
        }
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
        let encoded = match self.media.http() {
            Ok(http) => {
                let encoded = lease.stores().block_on(media::fetch(
                    &client,
                    &http,
                    room_id,
                    event_id,
                    &self.budgets,
                    &current,
                ));
                let _context = lease.stores().runtime().enter();
                drop(http);
                encoded
            }
            Err(code) => Err(code),
        };
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

    fn send_message(&self, room_id: &str, body: &str) -> Outcome {
        if !valid_room_id(room_id) || !valid_body(body) {
            return Outcome::Error(ErrorCode::InvalidArgument);
        }
        match self
            .session
            .send_message(room_id, zeroize::Zeroizing::new(body.to_owned()))
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

    /// Waits for pending notifications and takes them all, or returns `None`
    /// once the connection is closed.
    fn take_events(&self) -> Option<Vec<Event>> {
        let mut notifications = lock(&self.hub.notifications);
        while notifications.pending.is_empty()
            && !notifications.session_changed
            && !notifications.room_list_changed
            && !notifications.security_changed
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
        let rooms = std::mem::take(&mut notifications.pending);
        Some(
            session
                .into_iter()
                .chain(list)
                .chain(security)
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
                            let spawned = std::thread::Builder::new()
                                .name("core-long-call".into())
                                .stack_size(LONG_CALL_STACK_BYTES)
                                .spawn_scoped(scope, move || {
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
        end
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
