//! Messages the user sends, held in the core until the homeserver accepts
//! them (ADR-0005; development plan, "Send text and reconcile history").
//!
//! * Each message gets a transaction ID when it is queued. A retry sends the
//!   same ID again, so a message whose delivery is unknown cannot arrive
//!   twice: the homeserver answers a repeated ID with the first event.
//! * One sending thread takes messages in order. A later message in a room
//!   waits while an earlier one there is sending, failed or unknown, so a
//!   room's messages never arrive out of order; other rooms carry on.
//! * At most [`MAX_UNSENT`] messages are not yet sent; a sent message is kept
//!   (up to [`MAX_KEPT`] in all, oldest sent dropped first) so the page can
//!   match it to its event in the timeline.
//! * All bodies together take at most [`MAX_BODY_BUDGET`] bytes as JSON, so a
//!   room's list always fits one reply; old sent messages make room first,
//!   and a message that does not fit beside the unsent ones is refused.
//! * Memory only: nothing is kept after a lock or restart.

use matrix_sdk::ruma::{OwnedTransactionId, TransactionId};
use persephone_contracts::{PendingMessage, PendingState, SendFailure};
use std::collections::{BTreeSet, VecDeque};
use zeroize::Zeroizing;

/// Most messages queued, sending, failed or unknown at once.
pub const MAX_UNSENT: usize = 16;
/// Most messages kept, sent ones included.
pub const MAX_KEPT: usize = 32;
/// Most bytes all kept bodies take as JSON strings. With [`MAX_KEPT`]
/// entries' other fields (under 1 KiB each) this stays within one bounded
/// view (`views::MAX_PAGE_BYTES`, 112 KiB).
pub const MAX_BODY_BUDGET: usize = 80 * 1024;

/// How a send ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Sent(String),
    Failed(SendFailure),
    /// The request may have reached the homeserver.
    Unknown,
}

struct Entry {
    local_id: u32,
    room_id: String,
    body: Zeroizing<String>,
    /// The body's size as a JSON string.
    size: usize,
    transaction_id: OwnedTransactionId,
    state: PendingState,
    event_id: Option<String>,
    failure: Option<SendFailure>,
}

impl Entry {
    fn view(&self) -> PendingMessage {
        PendingMessage {
            local_id: self.local_id,
            room_id: self.room_id.clone(),
            body: self.body.to_string(),
            state: self.state,
            event_id: self.event_id.clone(),
            failure: self.failure,
        }
    }
}

/// A message to send now.
pub struct Job {
    pub local_id: u32,
    pub room_id: String,
    pub body: Zeroizing<String>,
    pub transaction_id: OwnedTransactionId,
}

#[derive(Default)]
pub struct Outbox {
    entries: VecDeque<Entry>,
    next_id: u32,
}

impl Outbox {
    /// Queues a message, or `None` when [`MAX_UNSENT`] are already unsent or
    /// the body does not fit [`MAX_BODY_BUDGET`] beside the unsent ones.
    pub fn queue(&mut self, room_id: &str, body: Zeroizing<String>) -> Option<PendingMessage> {
        let unsent: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|entry| entry.state != PendingState::Sent)
            .collect();
        let size = json_size(&body);
        let unsent_size: usize = unsent.iter().map(|entry| entry.size).sum();
        if unsent.len() >= MAX_UNSENT || unsent_size + size > MAX_BODY_BUDGET {
            return None;
        }
        // Old sent messages make room, by count and by size.
        while self.entries.len() >= MAX_KEPT
            || self.entries.iter().map(|entry| entry.size).sum::<usize>() + size > MAX_BODY_BUDGET
        {
            let oldest_sent = self
                .entries
                .iter()
                .position(|entry| entry.state == PendingState::Sent)?;
            self.entries.remove(oldest_sent);
        }
        self.next_id = self.next_id.wrapping_add(1);
        let entry = Entry {
            local_id: self.next_id,
            room_id: room_id.to_owned(),
            body,
            size,
            transaction_id: TransactionId::new(),
            state: PendingState::Queued,
            event_id: None,
            failure: None,
        };
        let view = entry.view();
        self.entries.push_back(entry);
        Some(view)
    }

    /// The next message to send, marked as sending: the first queued one in a
    /// room where nothing earlier is still unresolved.
    pub fn next_job(&mut self) -> Option<Job> {
        let mut blocked = BTreeSet::new();
        for entry in self.entries.iter_mut() {
            match entry.state {
                PendingState::Sent => {}
                PendingState::Queued if !blocked.contains(&entry.room_id) => {
                    entry.state = PendingState::Sending;
                    return Some(Job {
                        local_id: entry.local_id,
                        room_id: entry.room_id.clone(),
                        body: entry.body.clone(),
                        transaction_id: entry.transaction_id.clone(),
                    });
                }
                _ => {
                    blocked.insert(entry.room_id.clone());
                }
            }
        }
        None
    }

    /// Puts a message taken for sending back in the queue (no lease was free).
    pub fn requeue(&mut self, local_id: u32) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.local_id == local_id && entry.state == PendingState::Sending)
        {
            entry.state = PendingState::Queued;
        }
    }

    /// Records how a send ended; returns the room to tell the page about.
    pub fn finish(&mut self, local_id: u32, outcome: SendOutcome) -> Option<String> {
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| entry.local_id == local_id && entry.state == PendingState::Sending)?;
        match outcome {
            SendOutcome::Sent(event_id) => {
                entry.state = PendingState::Sent;
                entry.event_id = Some(event_id);
                entry.failure = None;
            }
            SendOutcome::Failed(failure) => {
                entry.state = PendingState::Failed;
                entry.failure = Some(failure);
            }
            SendOutcome::Unknown => {
                entry.state = PendingState::Unknown;
                entry.failure = None;
            }
        }
        Some(entry.room_id.clone())
    }

    /// Queues a failed or unknown message again, with the same transaction
    /// ID. Returns its room, or `None` if it cannot be retried.
    pub fn retry(&mut self, local_id: u32) -> Option<String> {
        let entry = self.entries.iter_mut().find(|entry| {
            entry.local_id == local_id
                && matches!(entry.state, PendingState::Failed | PendingState::Unknown)
        })?;
        entry.state = PendingState::Queued;
        entry.failure = None;
        Some(entry.room_id.clone())
    }

    /// Drops a message that is not being sent. Returns its room.
    pub fn discard(&mut self, local_id: u32) -> Option<String> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.local_id == local_id && entry.state != PendingState::Sending)?;
        self.entries.remove(index).map(|entry| entry.room_id)
    }

    /// The room's messages, oldest first.
    pub fn for_room(&self, room_id: &str) -> Vec<PendingMessage> {
        self.entries
            .iter()
            .filter(|entry| entry.room_id == room_id)
            .map(Entry::view)
            .collect()
    }

    /// The transaction ID of a message (for tests).
    pub fn transaction_id(&self, local_id: u32) -> Option<OwnedTransactionId> {
        self.entries
            .iter()
            .find(|entry| entry.local_id == local_id)
            .map(|entry| entry.transaction_id.clone())
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// The size of `body` as a JSON string, escapes and quotes included.
fn json_size(body: &str) -> usize {
    serde_json::to_string(body).map_or(usize::MAX / 2, |json| json.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(text: &str) -> Zeroizing<String> {
        Zeroizing::new(text.to_owned())
    }

    /// GIVEN messages in two rooms WHEN the first in room A fails THEN the
    /// later one in A waits while room B carries on; a retry keeps the same
    /// transaction ID and clears the way.
    #[test]
    fn given_a_failed_message_then_later_ones_in_its_room_wait_and_a_retry_keeps_its_id() {
        let mut outbox = Outbox::default();
        let a1 = outbox.queue("!a:s", body("a1")).unwrap().local_id;
        let a2 = outbox.queue("!a:s", body("a2")).unwrap().local_id;
        let b1 = outbox.queue("!b:s", body("b1")).unwrap().local_id;
        let first = outbox.next_job().unwrap();
        assert_eq!(first.local_id, a1);
        let id = first.transaction_id.clone();
        outbox.finish(a1, SendOutcome::Unknown);
        let next = outbox.next_job().unwrap();
        assert_eq!(next.local_id, b1, "a2 waits behind the unknown a1");
        outbox.finish(b1, SendOutcome::Sent("$b1".into()));
        assert!(outbox.next_job().is_none(), "nothing else may go yet");

        assert_eq!(outbox.retry(a1).as_deref(), Some("!a:s"));
        let again = outbox.next_job().unwrap();
        assert_eq!((again.local_id, again.transaction_id), (a1, id), "same ID");
        outbox.finish(a1, SendOutcome::Sent("$a1".into()));
        assert_eq!(outbox.next_job().unwrap().local_id, a2);
    }

    /// GIVEN states WHEN retrying or discarding THEN only failed and unknown
    /// messages retry, a message being sent cannot be discarded, and an
    /// unknown ID changes nothing.
    #[test]
    fn given_each_state_then_retry_and_discard_apply_only_where_safe() {
        let mut outbox = Outbox::default();
        let id = outbox.queue("!a:s", body("x")).unwrap().local_id;
        assert_eq!(outbox.retry(id), None, "queued: nothing to retry");
        outbox.next_job();
        assert_eq!(outbox.discard(id), None, "sending: not discarded");
        outbox.finish(id, SendOutcome::Failed(SendFailure::DeviceNotVerified));
        let view = &outbox.for_room("!a:s")[0];
        assert_eq!(
            (view.state, view.failure),
            (PendingState::Failed, Some(SendFailure::DeviceNotVerified))
        );
        assert_eq!(outbox.discard(id).as_deref(), Some("!a:s"));
        assert!(outbox.for_room("!a:s").is_empty());
        assert_eq!(outbox.retry(999), None);
        assert_eq!(outbox.discard(999), None);
        assert_eq!(outbox.finish(999, SendOutcome::Unknown), None);
    }

    /// GIVEN the limits WHEN more messages are queued THEN at most
    /// `MAX_UNSENT` wait, and sent ones make room oldest first within
    /// `MAX_KEPT`.
    #[test]
    fn given_the_limits_then_unsent_messages_are_capped_and_old_sent_ones_make_room() {
        let mut outbox = Outbox::default();
        for n in 0..MAX_UNSENT {
            assert!(outbox.queue(&format!("!r{n}:s"), body("x")).is_some());
        }
        assert!(outbox.queue("!r:s", body("x")).is_none(), "unsent capped");
        for _ in 0..MAX_UNSENT {
            let job = outbox.next_job().unwrap();
            outbox.finish(
                job.local_id,
                SendOutcome::Sent(format!("${}", job.local_id)),
            );
        }
        for n in 0..MAX_UNSENT {
            assert!(outbox.queue(&format!("!s{n}:s"), body("y")).is_some());
        }
        // Full: one more drops the oldest sent message.
        let job = outbox.next_job().unwrap();
        outbox.finish(job.local_id, SendOutcome::Sent("$late".into()));
        assert!(outbox.queue("!t:s", body("z")).is_some());
        assert!(outbox.for_room("!r0:s").is_empty(), "oldest sent dropped");
        assert_eq!(outbox.for_room("!r1:s").len(), 1);
    }

    /// GIVEN large bodies WHEN they are queued THEN all kept bodies stay
    /// within `MAX_BODY_BUDGET` as JSON: unsent ones that do not fit are
    /// refused, and sent ones make room oldest first. Escaped characters
    /// count at their escaped size.
    #[test]
    fn given_large_bodies_then_the_kept_bodies_stay_within_the_byte_budget() {
        let large = "\"".repeat(8 * 1024); // 16 KiB once escaped
        let total = |outbox: &Outbox| {
            outbox
                .for_room("!a:s")
                .iter()
                .map(|message| json_size(&message.body))
                .sum::<usize>()
        };
        let mut outbox = Outbox::default();
        let mut fitted = 0;
        while outbox.queue("!a:s", body(&large)).is_some() {
            fitted += 1;
        }
        assert_eq!(fitted, MAX_BODY_BUDGET / json_size(&large));
        assert!(total(&outbox) <= MAX_BODY_BUDGET);
        assert!(fitted < MAX_UNSENT, "the budget, not the count, refused");

        for _ in 0..fitted {
            let job = outbox.next_job().unwrap();
            outbox.finish(
                job.local_id,
                SendOutcome::Sent(format!("${}", job.local_id)),
            );
        }
        let first = outbox.for_room("!a:s")[0].local_id;
        assert!(
            outbox.queue("!a:s", body(&large)).is_some(),
            "sent ones make room"
        );
        assert!(total(&outbox) <= MAX_BODY_BUDGET);
        assert!(
            outbox
                .for_room("!a:s")
                .iter()
                .all(|message| message.local_id != first),
            "oldest sent dropped"
        );
    }
}
