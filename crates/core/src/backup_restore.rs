//! Restoring the key backup's room keys after recovery with the recovery key.
//!
//! The client downloads a backup key only after a decryption fails
//! (`BackupDownloadStrategy::AfterDecryptionFailure`, `account.rs`), one key
//! at a time and in the background. After recovery on a new device, old
//! history then stays undecryptable until each message has failed once. So,
//! when recovery succeeds, the security loop (`session.rs`) downloads the
//! backup's keys for each joined room that may be encrypted, with
//! `Backups::download_room_keys_for_room`:
//!
//! * At most [`MAX_RESTORE_ROOMS`] rooms, the most recently active first.
//! * One room per step, each step with its own lease and at most
//!   [`ROOM_RESTORE_TIMEOUT`], the longest one sync request may take. A lock
//!   waits for one room at most, and so does a security command.
//! * Recovery does not wait for the room list. For [`RESTORE_SCAN_WINDOW`]
//!   after recovery, the joined rooms are read again every
//!   [`RESTORE_SCAN_INTERVAL`], so rooms that sync later (Sliding Sync loads
//!   the list in batches) are added at the end of the queue.
//! * Each room is tried once. [`MAX_RESTORE_FAILURES`] failures in a row end
//!   the restore (the homeserver is likely unreachable). A lock or a new
//!   session ends it too; nothing is retried later. Keys not restored then
//!   still come one at a time, after a decryption fails, as before.
//!
//! The SDK stores the imported keys in the crypto store, which sends them on
//! `room_keys_received_stream`. The sync loop takes that stream after each
//! round and marks the subscribed rooms changed, so an open room reloads.
//!
//! Memory: the SDK reads one room's backup response whole
//! (`HttpClient::send`, `response.bytes()`), with the client's request
//! timeout but no size limit. Persephone cannot set one through the SDK.
//! The homeserver of the signed-in account sets that size; this is an
//! accepted risk, the same as for other SDK responses.

use matrix_sdk::{
    Client, EncryptionState,
    ruma::{MilliSecondsSinceUnixEpoch, OwnedRoomId, RoomId},
};
use std::{
    collections::{HashSet, VecDeque},
    time::{Duration, Instant},
};

/// Most rooms one restore downloads keys for: the room list's limit
/// (`views::MAX_ROOMS`).
pub const MAX_RESTORE_ROOMS: usize = crate::views::MAX_ROOMS;
/// Longest one room's download and import may take: the client's request
/// timeout (`account.rs`), so a restore step holds its lease no longer than
/// a sync request does.
pub const ROOM_RESTORE_TIMEOUT: Duration = Duration::from_secs(30);
/// Failed rooms in a row that end a restore.
pub const MAX_RESTORE_FAILURES: u8 = 3;
/// How long after recovery rooms that synced since are added to the restore.
pub const RESTORE_SCAN_WINDOW: Duration = Duration::from_secs(10 * 60);
/// Time between two reads of the joined rooms in that window: the sync poll.
pub const RESTORE_SCAN_INTERVAL: Duration = Duration::from_secs(5);

/// The rooms a restore has still to do, in order.
#[derive(Debug, Default)]
pub struct Restore {
    rooms: VecDeque<OwnedRoomId>,
    /// Every room this restore has queued, so that each is tried once. At
    /// most [`MAX_RESTORE_ROOMS`].
    queued: HashSet<OwnedRoomId>,
    failures: u8,
    /// While the scan window is open: the next scan and the window's end.
    scan: Option<(Instant, Instant)>,
}

impl Restore {
    /// A restore of `rooms`, in this order, that adds no rooms later; rooms
    /// after the first [`MAX_RESTORE_ROOMS`] are left out.
    pub fn new(rooms: impl IntoIterator<Item = OwnedRoomId>) -> Self {
        let mut restore = Self::default();
        restore.extend(rooms);
        restore
    }

    /// A restore that starts at `now` with no rooms. Its first step reads
    /// the joined rooms, and later steps read them again until
    /// [`RESTORE_SCAN_WINDOW`] has passed.
    pub fn start(now: Instant) -> Self {
        Self {
            scan: Some((now, now + RESTORE_SCAN_WINDOW)),
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rooms.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rooms.len()
    }

    /// Whether a step has work at `now`: a queued room or a scan.
    pub fn wants_step(&self, now: Instant) -> bool {
        !self.rooms.is_empty() || self.scan_due(now)
    }

    /// Whether the joined rooms are to be read again at `now`.
    pub fn scan_due(&self, now: Instant) -> bool {
        self.queued.len() < MAX_RESTORE_ROOMS
            && self
                .scan
                .is_some_and(|(next, end)| next <= now && now < end)
    }

    /// Adds the scanned `rooms` that this restore has not queued yet, in
    /// this order, at the end of the queue. The next scan is due
    /// [`RESTORE_SCAN_INTERVAL`] after `now`.
    pub fn add_scanned(&mut self, rooms: impl IntoIterator<Item = OwnedRoomId>, now: Instant) {
        if let Some((next, _)) = &mut self.scan {
            *next = now + RESTORE_SCAN_INTERVAL;
        }
        self.extend(rooms);
    }

    fn extend(&mut self, rooms: impl IntoIterator<Item = OwnedRoomId>) {
        for room in rooms {
            if self.queued.len() >= MAX_RESTORE_ROOMS {
                break;
            }
            if self.queued.insert(room.clone()) {
                self.rooms.push_back(room);
            }
        }
    }

    /// The next room to restore; it leaves the queue.
    pub fn take_next(&mut self) -> Option<OwnedRoomId> {
        self.rooms.pop_front()
    }

    /// Records the outcome of one room. After [`MAX_RESTORE_FAILURES`]
    /// failures in a row, the restore ends.
    pub fn record(&mut self, restored: bool) {
        if restored {
            self.failures = 0;
            return;
        }
        self.failures = self.failures.saturating_add(1);
        if self.failures >= MAX_RESTORE_FAILURES {
            self.stop();
        }
    }

    /// Ends the restore: the remaining rooms are not tried, and no rooms are
    /// added.
    pub fn stop(&mut self) {
        self.rooms.clear();
        self.scan = None;
    }
}

/// The order of a restore: rooms that may be encrypted (encrypted, or not
/// known yet), the most recent latest event first, then those without one;
/// equal ones by room ID. At most [`MAX_RESTORE_ROOMS`].
pub fn order(
    rooms: impl IntoIterator<
        Item = (
            OwnedRoomId,
            EncryptionState,
            Option<MilliSecondsSinceUnixEpoch>,
        ),
    >,
) -> Vec<OwnedRoomId> {
    let mut rooms: Vec<_> = rooms
        .into_iter()
        .filter(|(_, encryption, _)| !matches!(encryption, EncryptionState::NotEncrypted))
        .map(|(room, _, latest)| (room, latest))
        .collect();
    rooms.sort_by(|(a, a_latest), (b, b_latest)| b_latest.cmp(a_latest).then_with(|| a.cmp(b)));
    rooms
        .into_iter()
        .map(|(room, _)| room)
        .take(MAX_RESTORE_ROOMS)
        .collect()
}

/// The joined rooms of the client, in restore order.
pub fn joined_rooms(client: &Client) -> Vec<OwnedRoomId> {
    order(client.joined_rooms().into_iter().map(|room| {
        (
            room.room_id().to_owned(),
            room.encryption_state(),
            room.latest_event_timestamp(),
        )
    }))
}

/// Downloads and imports the backup's keys for one room. `false` when that
/// failed or took longer than [`ROOM_RESTORE_TIMEOUT`]. Without a usable
/// backup key the SDK does nothing and reports success.
pub async fn restore_room(client: &Client, room: &RoomId) -> bool {
    matches!(
        tokio::time::timeout(
            ROOM_RESTORE_TIMEOUT,
            client
                .encryption()
                .backups()
                .download_room_keys_for_room(room),
        )
        .await,
        Ok(Ok(()))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::{UInt, owned_room_id};

    fn at(millis: u32) -> Option<MilliSecondsSinceUnixEpoch> {
        Some(MilliSecondsSinceUnixEpoch(UInt::from(millis)))
    }

    /// GIVEN more rooms than the limit WHEN a restore starts THEN it holds
    /// only the first `MAX_RESTORE_ROOMS`, in the given order.
    #[test]
    fn given_too_many_rooms_then_the_restore_keeps_the_first_ones() {
        let rooms = (0..MAX_RESTORE_ROOMS + 5)
            .map(|n| OwnedRoomId::try_from(format!("!r{n:04}:s")).unwrap());
        let mut restore = Restore::new(rooms);
        assert_eq!(restore.len(), MAX_RESTORE_ROOMS);
        assert_eq!(restore.take_next().unwrap().as_str(), "!r0000:s");
        let mut last = None;
        while let Some(room) = restore.take_next() {
            last = Some(room);
        }
        assert_eq!(
            last.unwrap().as_str(),
            format!("!r{:04}:s", MAX_RESTORE_ROOMS - 1)
        );
        assert!(restore.is_empty());
    }

    /// GIVEN a restore WHEN rooms fail `MAX_RESTORE_FAILURES` times in a row
    /// THEN it ends; WHEN a success comes between failures THEN it goes on.
    #[test]
    fn given_failures_in_a_row_then_the_restore_ends() {
        let rooms = (0..10).map(|n| OwnedRoomId::try_from(format!("!r{n}:s")).unwrap());
        let mut restore = Restore::new(rooms);
        for _ in 0..MAX_RESTORE_FAILURES - 1 {
            restore.take_next();
            restore.record(false);
        }
        restore.take_next();
        restore.record(true);
        assert!(!restore.is_empty(), "a success resets the count");
        for _ in 0..MAX_RESTORE_FAILURES - 1 {
            restore.take_next();
            restore.record(false);
            assert!(!restore.is_empty());
        }
        restore.take_next();
        restore.record(false);
        assert!(restore.is_empty(), "too many failures in a row");
        assert_eq!(restore.take_next(), None);
    }

    /// GIVEN a restore started at recovery WHEN the joined rooms are read
    /// again in the scan window THEN only rooms not queued before are added,
    /// at the end; scans come once per interval and stop at the window's end
    /// or when the restore stops.
    #[test]
    fn given_rooms_that_sync_later_then_a_scan_adds_each_room_once() {
        let now = Instant::now();
        let mut restore = Restore::start(now);
        assert!(restore.is_empty());
        assert!(restore.wants_step(now), "the first scan is due at once");
        restore.add_scanned([owned_room_id!("!a:s"), owned_room_id!("!b:s")], now);
        assert!(!restore.scan_due(now), "one scan per interval");
        assert_eq!(restore.take_next().unwrap().as_str(), "!a:s");

        let later = now + RESTORE_SCAN_INTERVAL;
        assert!(restore.scan_due(later));
        restore.add_scanned(
            [
                owned_room_id!("!a:s"),
                owned_room_id!("!c:s"),
                owned_room_id!("!b:s"),
            ],
            later,
        );
        let queue: Vec<_> = std::iter::from_fn(|| restore.take_next()).collect();
        assert_eq!(queue, [owned_room_id!("!b:s"), owned_room_id!("!c:s")]);

        assert!(
            !restore.wants_step(now + RESTORE_SCAN_WINDOW),
            "window ended"
        );
        restore.stop();
        assert!(!restore.scan_due(later + RESTORE_SCAN_INTERVAL), "stopped");
        assert!(!Restore::new([owned_room_id!("!a:s")]).scan_due(now));
    }

    /// GIVEN scans that find more rooms than the limit in total WHEN they
    /// are added THEN the restore queues `MAX_RESTORE_ROOMS` rooms at most
    /// and scans no more.
    #[test]
    fn given_scans_past_the_limit_then_the_restore_is_bounded() {
        let now = Instant::now();
        let mut restore = Restore::start(now);
        let room = |n: usize| OwnedRoomId::try_from(format!("!r{n:04}:s")).unwrap();
        restore.add_scanned((0..MAX_RESTORE_ROOMS - 1).map(room), now);
        while restore.take_next().is_some() {}
        let later = now + RESTORE_SCAN_INTERVAL;
        restore.add_scanned((0..MAX_RESTORE_ROOMS + 5).map(room), later);
        assert_eq!(restore.len(), 1, "only one place was left");
        assert!(!restore.scan_due(later + RESTORE_SCAN_INTERVAL));
    }

    /// GIVEN joined rooms WHEN they are ordered THEN plain rooms are left
    /// out, rooms of unknown encryption stay, the newest activity is first,
    /// rooms without a latest event are last, and equal ones go by room ID.
    #[test]
    fn given_joined_rooms_then_encrypted_ones_are_ordered_by_activity() {
        let order = order([
            (owned_room_id!("!old:s"), EncryptionState::Encrypted, at(10)),
            (
                owned_room_id!("!plain:s"),
                EncryptionState::NotEncrypted,
                at(99),
            ),
            (owned_room_id!("!none:s"), EncryptionState::Encrypted, None),
            (owned_room_id!("!new:s"), EncryptionState::Unknown, at(50)),
            (owned_room_id!("!b:s"), EncryptionState::Encrypted, at(10)),
        ]);
        let order: Vec<_> = order.iter().map(|room| room.as_str()).collect();
        assert_eq!(order, ["!new:s", "!b:s", "!old:s", "!none:s"]);
    }

    /// GIVEN more candidate rooms than the limit WHEN they are ordered THEN
    /// only `MAX_RESTORE_ROOMS` are returned.
    #[test]
    fn given_too_many_candidates_then_the_order_is_bounded() {
        let order = order((0..MAX_RESTORE_ROOMS * 2).map(|n| {
            (
                OwnedRoomId::try_from(format!("!r{n}:s")).unwrap(),
                EncryptionState::Encrypted,
                None,
            )
        }));
        assert_eq!(order.len(), MAX_RESTORE_ROOMS);
    }
}
