//! Persephone core (ADR-0004).
//!
//! The core is the application's entry process. It will own sessions, the
//! Matrix SDK, stores, keys and the media broker. The UI process holds none of
//! these; it sends one bounded request frame per call, and the core answers
//! with one bounded reply frame. The core also pushes change notifications for
//! rooms the UI subscribed to.
//!
//! The UI is treated as untrusted. Every request is read with the budgeted
//! frame reader, so an oversized or malformed header is refused before its body
//! is allocated. Anything a correct UI never sends (a bad frame, an unknown
//! command or field) ends the session instead of being answered.
//!
//! Notifications carry no content and coalesce: a room that changes many times
//! before the UI reads has one pending notification. Pending state is
//! therefore bounded by the number of subscriptions, whatever the UI's speed.

pub mod store;
pub mod vault;
pub mod views;

use persephone_contracts::{AppStatus, StoreState};
use persephone_core_protocol::{
    Call, CoreMessage, ErrorCode, Event, MAX_ROOM_ID_BYTES, MAX_SUBSCRIPTIONS, Outcome,
    ReceiveError, Reply, Request, receive, send,
};
use persephone_media_protocol::ResourceBudgets;
use serde::Serialize;
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    sync::{Condvar, Mutex, MutexGuard},
    time::Duration,
};

pub const MILESTONE: &str = "P1";

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
    closed: bool,
}

/// Core state shared by the request loop and the notification writer. One
/// `Core` serves one UI connection; once [`serve`] returns it stays closed.
pub struct Core {
    store: StoreState,
    views: Mutex<views::Fixtures>,
    notifications: Mutex<Notifications>,
    changed: Condvar,
}

impl Default for Core {
    fn default() -> Self {
        Self::new(views::Fixtures::default())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned lock means a panic mid-update; the core stops either way.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn ok<T: Serialize>(value: &T) -> Outcome {
    serde_json::to_value(value).map_or(Outcome::Error(ErrorCode::Internal), Outcome::Ok)
}

impl Core {
    pub fn new(views: views::Fixtures) -> Self {
        Self {
            store: StoreState::NotConfigured,
            views: Mutex::new(views),
            notifications: Mutex::new(Notifications::default()),
            changed: Condvar::new(),
        }
    }

    /// Records the local store's state for `app_status`.
    pub fn with_store(mut self, store: StoreState) -> Self {
        self.store = store;
        self
    }

    /// Answers one request.
    pub fn handle(&self, request: &Request) -> Reply {
        let outcome = match &request.call {
            Call::AppStatus {} => ok(&AppStatus {
                version: env!("CARGO_PKG_VERSION").into(),
                milestone: MILESTONE.into(),
                store: self.store,
            }),
            Call::RoomList {} => ok(&lock(&self.views).room_list()),
            Call::TimelinePage { room_id, cursor } => {
                match lock(&self.views).timeline_page(room_id, cursor.as_deref()) {
                    Ok(page) => ok(&page),
                    Err(code) => Outcome::Error(code),
                }
            }
            Call::Subscribe { room_id } => self.subscribe(room_id),
            Call::Unsubscribe { room_id } => {
                let mut notifications = lock(&self.notifications);
                notifications.subscribed.remove(room_id);
                notifications.pending.remove(room_id);
                Outcome::Ok(serde_json::Value::Null)
            }
        };
        Reply {
            id: request.id,
            outcome,
        }
    }

    fn subscribe(&self, room_id: &str) -> Outcome {
        if room_id.len() > MAX_ROOM_ID_BYTES || !room_id.starts_with('!') {
            return Outcome::Error(ErrorCode::InvalidArgument);
        }
        if !lock(&self.views).has_room(room_id) {
            return Outcome::Error(ErrorCode::NotFound);
        }
        let mut notifications = lock(&self.notifications);
        if !notifications.subscribed.contains(room_id)
            && notifications.subscribed.len() >= MAX_SUBSCRIPTIONS
        {
            return Outcome::Error(ErrorCode::LimitExceeded);
        }
        notifications.subscribed.insert(room_id.to_owned());
        Outcome::Ok(serde_json::Value::Null)
    }

    /// Adds a message to a room and notifies a subscribed UI. Stands in for
    /// incoming Matrix events until the SDK is integrated.
    pub fn post_message(&self, room_id: &str, sender: &str, body: String) -> bool {
        if !lock(&self.views).append_message(room_id, sender, body) {
            return false;
        }
        let mut notifications = lock(&self.notifications);
        if notifications.subscribed.contains(room_id) {
            notifications.pending.insert(room_id.to_owned());
            self.changed.notify_all();
        }
        true
    }

    /// Number of rooms with a pending notification (for tests).
    pub fn pending_notifications(&self) -> usize {
        lock(&self.notifications).pending.len()
    }

    /// Waits for pending notifications and takes them all, or returns `None`
    /// once the connection is closed.
    fn take_events(&self) -> Option<Vec<Event>> {
        let mut notifications = lock(&self.notifications);
        while notifications.pending.is_empty() && !notifications.closed {
            notifications = self
                .changed
                .wait(notifications)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        if notifications.closed {
            return None;
        }
        let rooms = std::mem::take(&mut notifications.pending);
        Some(
            rooms
                .into_iter()
                .map(|room_id| Event::RoomChanged { room_id })
                .collect(),
        )
    }

    /// Waits for `period`, returning `false` early once serving has ended.
    pub fn wait_running(&self, period: Duration) -> bool {
        let notifications = lock(&self.notifications);
        let (notifications, _) = self
            .changed
            .wait_timeout_while(notifications, period, |n| !n.closed)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        !notifications.closed
    }

    fn close(&self) {
        lock(&self.notifications).closed = true;
        self.changed.notify_all();
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
        let end = loop {
            match receive::<_, Request>(&mut input, &budgets) {
                Ok(request) => {
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
