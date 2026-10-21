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

pub mod account;
pub mod credentials;
pub mod homeserver;
pub mod matrix_views;
pub mod session;
pub mod store;
pub mod vault;
pub mod views;

use credentials::CredentialSource;
use homeserver::Homeserver;
use matrix_views::MatrixViews;
use persephone_contracts::{AccountStatus, AppStatus, RoomList, StoreState};
use persephone_core_protocol::{
    Call, CoreMessage, ErrorCode, Event, MAX_HOMESERVER_URL_BYTES, MAX_ROOM_ID_BYTES,
    MAX_SUBSCRIPTIONS, Outcome, ReceiveError, Reply, Request, receive, send,
};
use persephone_media_protocol::ResourceBudgets;
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
        let mut notifications = lock(&self.notifications);
        let mut any = false;
        if list && !rooms.is_empty() && !notifications.room_list_changed {
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
    fixtures: Mutex<views::Fixtures>,
    matrix: MatrixViews,
    hub: Arc<Hub>,
}

impl Default for Core {
    fn default() -> Self {
        Self::new(views::Fixtures::default())
    }
}

fn ok<T: Serialize>(value: &T) -> Outcome {
    serde_json::to_value(value).map_or(Outcome::Error(ErrorCode::Internal), Outcome::Ok)
}

fn empty_rooms() -> RoomList {
    RoomList {
        rooms: Vec::new(),
        truncated: false,
    }
}

impl Core {
    pub fn new(fixtures: views::Fixtures) -> Self {
        Self {
            session: Session::not_configured(),
            credentials: Arc::from(credentials::platform()),
            fixtures: Mutex::new(fixtures),
            matrix: MatrixViews::default(),
            hub: Arc::default(),
        }
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

    /// Answers one request.
    pub fn handle(&self, request: &Request) -> Reply {
        let outcome = match &request.call {
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
                let (state, _drain) = self
                    .session
                    .lock(LOCK_TIMEOUT, || self.hub.announce_session_change());
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
            Call::SignIn { homeserver } => self.sign_in(homeserver),
        };
        Reply {
            id: request.id,
            outcome,
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

    fn subscribe(&self, room_id: &str) -> Outcome {
        if room_id.len() > MAX_ROOM_ID_BYTES || !room_id.starts_with('!') {
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
        let rooms = std::mem::take(&mut notifications.pending);
        Some(
            session
                .into_iter()
                .chain(list)
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
    let budgets = ResourceBudgets::default();
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
                    let reply = CoreMessage::Reply(core.handle(&request));
                    if send(&mut *lock(&output), &reply).is_err() {
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
}
