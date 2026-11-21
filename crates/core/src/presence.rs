//! This account's presence and its contacts' presence (ADR-0057).
//!
//! * Writes. Every sync request carries a `set_presence` value, and the SDK
//!   sends `online` unless told otherwise. A value of `offline` leaves the
//!   presence as it is: "the client is not marked as being online" (Matrix
//!   client-server API, `GET /sync`). [`Writer`] picks the value of each
//!   request and of each explicit `PUT /presence/{userId}/status`. At most
//!   one request each [`MIN_WRITE_GAP`] changes the presence, and a sync
//!   request does so at most each [`SYNC_CARRY_GAP`]. A sync round that
//!   returns early, for example because another client of this account
//!   changed the presence, therefore does not set it again at once, so two
//!   clients cannot flood the homeserver by setting it in turn.
//! * Reads. Classic sync delivers `m.presence` events; Sliding Sync has no
//!   presence extension. [`Book`] keeps the presence of at most
//!   [`MAX_CONTACTS`] direct-message contacts. With Sliding Sync the sync
//!   loop asks `GET /presence/{userId}/status` for at most
//!   [`LOOKUPS_PER_ROUND`] of them in a round, each at most once in
//!   [`LOOKUP_AGE`], or once in [`WANTED_AGE`] when the page opens their room.

use matrix_sdk::ruma::{OwnedUserId, UserId, presence::PresenceState};
use persephone_contracts::PresenceStatus;
use std::{
    collections::VecDeque,
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

/// The shortest time between two requests that change this account's
/// presence.
pub const MIN_WRITE_GAP: Duration = Duration::from_secs(1);
/// The shortest time between two sync requests that carry this account's
/// presence. It stays below the 30 seconds after which Synapse takes a user
/// that stopped syncing as offline, with room for one 5-second poll.
pub const SYNC_CARRY_GAP: Duration = Duration::from_secs(20);
/// Most contacts whose presence is kept.
pub const MAX_CONTACTS: usize = 32;
/// Most presence lookups in one sync round.
pub const LOOKUPS_PER_ROUND: usize = 2;
/// How long a looked-up presence is used before it is asked again.
pub const LOOKUP_AGE: Duration = Duration::from_secs(5 * 60);
/// How old a presence may be before opening the contact's room asks again.
pub const WANTED_AGE: Duration = Duration::from_secs(60);
/// The longest wait for one lookup.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// The Matrix state of a status.
pub fn state(status: PresenceStatus) -> PresenceState {
    match status {
        PresenceStatus::Online => PresenceState::Online,
        PresenceStatus::Unavailable => PresenceState::Unavailable,
        PresenceStatus::Offline => PresenceState::Offline,
    }
}

/// The status of a received Matrix state; `None` for one this build does
/// not know.
pub fn status(state: &PresenceState) -> Option<PresenceStatus> {
    match state {
        PresenceState::Online => Some(PresenceStatus::Online),
        PresenceState::Unavailable => Some(PresenceStatus::Unavailable),
        PresenceState::Offline => Some(PresenceStatus::Offline),
        _ => None,
    }
}

/// When this account's presence was last changed and what still waits to be
/// sent. Owned by the session; used by the sync loop.
#[derive(Debug, Default)]
pub struct Writer {
    /// The account's settings: whether it shares presence, and which.
    share: bool,
    status: PresenceStatus,
    /// An explicit change waiting for its `PUT`.
    pending: Option<PresenceStatus>,
    last_write: Option<Instant>,
    last_carry: Option<Instant>,
}

impl Writer {
    /// The writer of a session that starts. Sync requests send an online or
    /// away status; an offline one, which no sync request can send, waits
    /// for its `PUT`.
    pub fn new(share: bool, status: PresenceStatus) -> Self {
        Self {
            share,
            status,
            pending: (share && status == PresenceStatus::Offline).then_some(status),
            ..Self::default()
        }
    }

    /// Takes the account's settings. A change of the shared presence waits
    /// for its `PUT`; turning sharing off drops any waiting one and sends
    /// nothing, so the presence stays as the homeserver has it.
    pub fn configure(&mut self, share: bool, status: PresenceStatus) {
        let changed = share && (!self.share || status != self.status);
        self.share = share;
        self.status = status;
        self.pending = if !share {
            None
        } else if changed {
            Some(status)
        } else {
            self.pending
        };
    }

    fn may_write(&self, now: Instant) -> bool {
        self.last_write
            .is_none_or(|last| now.saturating_duration_since(last) >= MIN_WRITE_GAP)
    }

    /// The waiting change to `PUT` now, or `None`: nothing waits, or the
    /// last write was too recent. Taking it counts as a write.
    pub fn take_put(&mut self, now: Instant) -> Option<PresenceState> {
        if self.pending.is_none() || !self.may_write(now) {
            return None;
        }
        self.last_write = Some(now);
        self.pending.take().map(state)
    }

    /// Puts back a change whose `PUT` failed, unless a newer one waits or
    /// sharing was turned off since.
    pub fn retry(&mut self, failed: PresenceState) {
        if self.share && self.pending.is_none() && state(self.status) == failed {
            self.pending = Some(self.status);
        }
    }

    /// The `set_presence` value of the next sync request. Only `online` and
    /// `unavailable` change the presence, and so count as a write: an
    /// `offline` sync request leaves it as it is, and an `offline` status is
    /// sent by its `PUT` alone.
    pub fn for_sync(&mut self, now: Instant) -> PresenceState {
        let carries = self.share
            && self.status != PresenceStatus::Offline
            && self.may_write(now)
            && self
                .last_carry
                .is_none_or(|last| now.saturating_duration_since(last) >= SYNC_CARRY_GAP);
        if !carries {
            return PresenceState::Offline;
        }
        self.last_write = Some(now);
        self.last_carry = Some(now);
        state(self.status)
    }
}

#[derive(Debug)]
struct Entry {
    user: OwnedUserId,
    status: Option<PresenceStatus>,
    /// When the presence was last received or asked; `None` never.
    checked: Option<Instant>,
    /// The page opened the contact's room.
    wanted: bool,
}

/// The presence of the account's direct-message contacts, at most
/// [`MAX_CONTACTS`]. A lock clears it with the rest of the session.
#[derive(Default)]
pub struct Book {
    entries: Mutex<VecDeque<Entry>>,
}

impl Book {
    fn entries(&self) -> MutexGuard<'_, VecDeque<Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Keeps only `contacts`, the first [`MAX_CONTACTS`] of them, and adds
    /// those not known yet.
    pub fn track<'a>(&self, contacts: impl IntoIterator<Item = &'a UserId>) {
        let mut entries = self.entries();
        let mut kept = VecDeque::new();
        for user in contacts {
            if kept.len() == MAX_CONTACTS {
                break;
            }
            if kept.iter().any(|entry: &Entry| entry.user == user) {
                continue;
            }
            let entry = match entries.iter().position(|entry| entry.user == user) {
                Some(index) => entries.remove(index).expect("found above"),
                None => Entry {
                    user: user.to_owned(),
                    status: None,
                    checked: None,
                    wanted: false,
                },
            };
            kept.push_back(entry);
        }
        *entries = kept;
    }

    /// Records a received presence of a tracked contact; others are ignored.
    /// `true` when the shown presence changed.
    pub fn note(&self, user: &UserId, status: Option<PresenceStatus>, now: Instant) -> bool {
        let mut entries = self.entries();
        let Some(entry) = entries.iter_mut().find(|entry| entry.user == user) else {
            return false;
        };
        let changed = entry.status != status;
        entry.status = status;
        entry.checked = Some(now);
        entry.wanted = false;
        changed
    }

    /// The contact's presence, if known.
    pub fn get(&self, user: &UserId) -> Option<PresenceStatus> {
        self.entries()
            .iter()
            .find(|entry| entry.user == user)
            .and_then(|entry| entry.status)
    }

    /// Asks for the contact's presence again in the next lookups, unless it
    /// is younger than [`WANTED_AGE`].
    pub fn want(&self, user: &UserId, now: Instant) {
        if let Some(entry) = self.entries().iter_mut().find(|entry| entry.user == user) {
            entry.wanted = entry
                .checked
                .is_none_or(|checked| now.saturating_duration_since(checked) >= WANTED_AGE);
        }
    }

    /// At most [`LOOKUPS_PER_ROUND`] contacts to ask for: wanted ones first,
    /// then those never asked, then the oldest past [`LOOKUP_AGE`]. Each is
    /// marked as asked now, so a failed lookup waits its turn again.
    pub fn due(&self, now: Instant) -> Vec<OwnedUserId> {
        let mut entries = self.entries();
        let mut due: Vec<&mut Entry> = entries
            .iter_mut()
            .filter(|entry| {
                entry.wanted
                    || entry
                        .checked
                        .is_none_or(|checked| now.saturating_duration_since(checked) >= LOOKUP_AGE)
            })
            .collect();
        due.sort_by_key(|entry| (!entry.wanted, entry.checked));
        due.into_iter()
            .take(LOOKUPS_PER_ROUND)
            .map(|entry| {
                entry.checked = Some(now);
                entry.wanted = false;
                entry.user.clone()
            })
            .collect()
    }

    pub fn clear(&self) {
        self.entries().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(name: &str) -> OwnedUserId {
        OwnedUserId::try_from(format!("@{name}:server.invalid")).unwrap()
    }

    /// GIVEN sharing is on WHEN sync rounds follow each other quickly THEN
    /// only one request each 20 seconds carries the presence, and the others
    /// carry `offline`, which leaves it as it is.
    #[test]
    fn given_quick_sync_rounds_then_presence_is_carried_once_in_twenty_seconds() {
        let start = Instant::now();
        let mut writer = Writer::new(true, PresenceStatus::Online);
        assert_eq!(writer.for_sync(start), PresenceState::Online);
        let mut carried = 0;
        for step in 1..200u64 {
            let at = start + Duration::from_millis(step * 100);
            if writer.for_sync(at) != PresenceState::Offline {
                carried += 1;
            }
        }
        assert_eq!(carried, 0, "no carry within 20 s of the first");
        assert_eq!(
            writer.for_sync(start + SYNC_CARRY_GAP),
            PresenceState::Online
        );
    }

    /// GIVEN many changes in a short time WHEN they are sent THEN at most one
    /// request each second changes the presence, the newest change wins, and
    /// a sync request does not carry it in the same second.
    #[test]
    fn given_many_changes_then_at_most_one_write_each_second_and_the_newest_wins() {
        let start = Instant::now();
        let mut writer = Writer::new(true, PresenceStatus::Online);
        assert_eq!(writer.for_sync(start), PresenceState::Online);
        writer.configure(true, PresenceStatus::Unavailable);
        assert_eq!(writer.take_put(start + Duration::from_millis(500)), None);
        writer.configure(true, PresenceStatus::Offline);
        writer.configure(true, PresenceStatus::Online);
        writer.configure(true, PresenceStatus::Unavailable);
        let second = start + MIN_WRITE_GAP;
        assert_eq!(writer.take_put(second), Some(PresenceState::Unavailable));
        assert_eq!(writer.take_put(second), None, "nothing waits");
        writer.configure(true, PresenceStatus::Online);
        let soon = second + Duration::from_millis(999);
        assert_eq!(writer.take_put(soon), None);
        assert_eq!(writer.for_sync(soon), PresenceState::Offline);
        assert_eq!(
            writer.take_put(second + MIN_WRITE_GAP),
            Some(PresenceState::Online)
        );
    }

    /// GIVEN sharing is off WHEN the settings change or sync runs THEN no
    /// request changes the presence.
    #[test]
    fn given_sharing_is_off_then_nothing_changes_the_presence() {
        let start = Instant::now();
        let mut writer = Writer::new(true, PresenceStatus::Online);
        writer.configure(true, PresenceStatus::Unavailable);
        writer.configure(false, PresenceStatus::Unavailable);
        for step in 0..100u64 {
            let at = start + Duration::from_secs(step);
            assert_eq!(writer.take_put(at), None);
            assert_eq!(writer.for_sync(at), PresenceState::Offline);
        }
        writer.retry(PresenceState::Unavailable);
        assert_eq!(writer.take_put(start + Duration::from_secs(200)), None);
    }

    /// GIVEN the offline status WHEN chosen THEN one `PUT` sends it, and sync
    /// requests never carry a presence.
    #[test]
    fn given_the_offline_status_then_only_its_put_sends_it() {
        let start = Instant::now();
        let mut writer = Writer::new(true, PresenceStatus::Online);
        writer.configure(true, PresenceStatus::Offline);
        assert_eq!(writer.take_put(start), Some(PresenceState::Offline));
        for step in 1..100u64 {
            let at = start + Duration::from_secs(step);
            assert_eq!(writer.for_sync(at), PresenceState::Offline);
            assert_eq!(writer.take_put(at), None);
        }
    }

    /// GIVEN a failed `PUT` WHEN retried THEN it is sent again, unless a
    /// newer change replaced it.
    #[test]
    fn given_a_failed_put_then_it_waits_again_unless_replaced() {
        let start = Instant::now();
        let mut writer = Writer::new(true, PresenceStatus::Online);
        writer.configure(true, PresenceStatus::Unavailable);
        let failed = writer.take_put(start).unwrap();
        writer.retry(failed);
        assert_eq!(writer.take_put(start), None, "within a second");
        assert_eq!(
            writer.take_put(start + MIN_WRITE_GAP),
            Some(PresenceState::Unavailable)
        );
        writer.configure(true, PresenceStatus::Online);
        writer.retry(PresenceState::Unavailable);
        assert_eq!(
            writer.take_put(start + 2 * MIN_WRITE_GAP),
            Some(PresenceState::Online)
        );
    }

    /// GIVEN more contacts than kept WHEN tracked THEN only the first 32 are
    /// kept, a received presence of another user is ignored, and a contact
    /// that stays keeps its presence.
    #[test]
    fn given_many_contacts_then_the_book_stays_bounded() {
        let book = Book::default();
        let now = Instant::now();
        let users: Vec<OwnedUserId> = (0..40).map(|n| user(&format!("u{n}"))).collect();
        book.track(users.iter().map(|user| &**user));
        assert!(book.note(&users[0], Some(PresenceStatus::Online), now));
        assert!(!book.note(&users[39], Some(PresenceStatus::Online), now));
        assert_eq!(book.get(&users[39]), None);
        book.track(users[..2].iter().rev().map(|user| &**user));
        assert_eq!(book.get(&users[0]), Some(PresenceStatus::Online));
        assert_eq!(book.entries().len(), 2);
        assert!(!book.note(&users[0], Some(PresenceStatus::Online), now));
    }

    /// GIVEN tracked contacts WHEN lookups are due THEN at most two are asked
    /// a round, wanted ones first, and none is asked again before five
    /// minutes, or one minute once its room is opened.
    #[test]
    fn given_contacts_then_lookups_are_bounded_and_spaced() {
        let book = Book::default();
        let start = Instant::now();
        let users: Vec<OwnedUserId> = (0..3).map(|n| user(&format!("u{n}"))).collect();
        book.track(users.iter().map(|user| &**user));
        assert_eq!(book.due(start), users[..2].to_vec());
        assert_eq!(book.due(start), users[2..].to_vec());
        assert!(book.due(start + Duration::from_secs(1)).is_empty());
        book.want(&users[1], start + Duration::from_secs(30));
        assert!(
            book.due(start + Duration::from_secs(30)).is_empty(),
            "too young"
        );
        let later = start + WANTED_AGE;
        book.want(&users[1], later);
        assert_eq!(book.due(later), vec![users[1].clone()]);
        let all = start + WANTED_AGE + LOOKUP_AGE;
        assert_eq!(book.due(all).len(), LOOKUPS_PER_ROUND);
    }
}
