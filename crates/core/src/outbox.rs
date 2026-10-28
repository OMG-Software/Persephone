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
//! * All kept messages together take at most [`MAX_KEPT_BYTES`] as JSON, so a
//!   room's list always fits one reply; old sent messages make room first,
//!   and a message that does not fit beside the unsent ones is refused.
//! * At most [`MAX_UNSENT_IMAGES`] image is not yet sent (queued, sending,
//!   failed or unknown): its stripped bytes stay here for a retry, and are
//!   dropped once it is sent or discarded. Its entry otherwise behaves like
//!   a text message, in the same order.
//! * Memory only: nothing is kept after a lock or restart.

use crate::media::ImageUpload;
use matrix_sdk::ruma::{OwnedEventId, OwnedTransactionId, TransactionId};
use persephone_contracts::{PendingKind, PendingMessage, PendingState, SendFailure, UnixMillis};
use std::{
    collections::{BTreeSet, VecDeque},
    sync::Arc,
};
use zeroize::Zeroizing;

/// Most messages queued, sending, failed or unknown at once.
pub const MAX_UNSENT: usize = 16;
/// Most messages kept, sent ones included.
pub const MAX_KEPT: usize = 32;
/// Most images not yet sent at once: each holds up to 20 MiB.
pub const MAX_UNSENT_IMAGES: usize = 1;
/// Most bytes all kept messages take as JSON (see [`entry_size`]). This
/// leaves room for the list's brackets and commas within one bounded view
/// (`views::MAX_PAGE_BYTES`, 112 KiB).
pub const MAX_KEPT_BYTES: usize = 100 * 1024;
/// The most an event ID takes as JSON. The SDK accepts up to 255 bytes and
/// checks little more, so every byte may be a control character, which JSON
/// writes as `\u00XX` (6 bytes); 2 more for the quotes.
const MAX_EVENT_ID_JSON: usize = 255 * 6 + 2;
/// What a message takes as JSON besides its body and room ID: the event ID,
/// and under 176 bytes for the field names, the local ID, the kind, the
/// state, the failure, the queue time and the list's comma.
const ENTRY_OVERHEAD: usize = MAX_EVENT_ID_JSON + 176;

/// How a send ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Sent(String),
    Failed(SendFailure),
    /// The request may have reached the homeserver.
    Unknown,
}

/// The event a text message refers to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Relation {
    /// A reply to this event (`m.in_reply_to`).
    Reply(OwnedEventId),
}

/// What a message sends.
#[derive(Clone)]
pub enum Content {
    Text(Zeroizing<String>, Option<Relation>),
    /// The stripped image; dropped from the entry once sent.
    Image(Arc<ImageUpload>),
}

struct Entry {
    local_id: u32,
    room_id: String,
    kind: PendingKind,
    body: Zeroizing<String>,
    relation: Option<Relation>,
    image: Option<Arc<ImageUpload>>,
    /// What the message takes as JSON, from [`entry_size`].
    size: usize,
    transaction_id: OwnedTransactionId,
    state: PendingState,
    event_id: Option<String>,
    failure: Option<SendFailure>,
    queued_at: UnixMillis,
}

impl Entry {
    fn view(&self) -> PendingMessage {
        PendingMessage {
            local_id: self.local_id,
            room_id: self.room_id.clone(),
            kind: self.kind,
            body: self.body.to_string(),
            state: self.state,
            event_id: self.event_id.clone(),
            failure: self.failure,
            queued_at: self.queued_at,
        }
    }
}

/// A message to send now.
pub struct Job {
    pub local_id: u32,
    pub room_id: String,
    pub content: Content,
    pub transaction_id: OwnedTransactionId,
}

#[derive(Default)]
pub struct Outbox {
    entries: VecDeque<Entry>,
    next_id: u32,
}

impl Outbox {
    /// Queues a message, or `None` when [`MAX_UNSENT`] are already unsent or
    /// the message does not fit [`MAX_KEPT_BYTES`] beside the unsent ones.
    pub fn queue(
        &mut self,
        room_id: &str,
        body: Zeroizing<String>,
        relation: Option<Relation>,
    ) -> Option<PendingMessage> {
        self.push(room_id, Content::Text(body, relation))
    }

    /// Queues an image, or `None` when [`MAX_UNSENT_IMAGES`] images are
    /// already unsent or the limits of [`Outbox::queue`] refuse it.
    pub fn queue_image(
        &mut self,
        room_id: &str,
        image: Arc<ImageUpload>,
    ) -> Option<PendingMessage> {
        let unsent_images = self
            .entries
            .iter()
            .filter(|entry| entry.image.is_some() && entry.state != PendingState::Sent)
            .count();
        if unsent_images >= MAX_UNSENT_IMAGES {
            return None;
        }
        self.push(room_id, Content::Image(image))
    }

    fn push(&mut self, room_id: &str, content: Content) -> Option<PendingMessage> {
        let (kind, body, relation, image) = match content {
            Content::Text(body, relation) => (PendingKind::Text, body, relation, None),
            Content::Image(image) => (PendingKind::Image, Zeroizing::default(), None, Some(image)),
        };
        let unsent: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|entry| entry.state != PendingState::Sent)
            .collect();
        let size = entry_size(room_id, &body);
        let unsent_size: usize = unsent.iter().map(|entry| entry.size).sum();
        if unsent.len() >= MAX_UNSENT || unsent_size.saturating_add(size) > MAX_KEPT_BYTES {
            return None;
        }
        // Old sent messages make room, by count and by size.
        while self.entries.len() >= MAX_KEPT
            || self
                .entries
                .iter()
                .map(|entry| entry.size)
                .sum::<usize>()
                .saturating_add(size)
                > MAX_KEPT_BYTES
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
            kind,
            body,
            relation,
            image,
            size,
            transaction_id: TransactionId::new(),
            state: PendingState::Queued,
            event_id: None,
            failure: None,
            queued_at: crate::views::now_millis(),
        };
        let view = entry.view();
        self.entries.push_back(entry);
        Some(view)
    }

    /// Where the next message to send is: the first queued one in a room
    /// where nothing earlier is still unresolved.
    fn next_index(&self) -> Option<usize> {
        let mut blocked = BTreeSet::new();
        for (index, entry) in self.entries.iter().enumerate() {
            match entry.state {
                PendingState::Sent => {}
                PendingState::Queued if !blocked.contains(entry.room_id.as_str()) => {
                    return Some(index);
                }
                _ => {
                    blocked.insert(entry.room_id.as_str());
                }
            }
        }
        None
    }

    /// Whether a message is ready to send. Changes nothing, so the sender can
    /// ask before it takes a lease.
    pub fn has_job(&self) -> bool {
        self.next_index().is_some()
    }

    /// The next message to send, marked as sending.
    pub fn next_job(&mut self) -> Option<Job> {
        let index = self.next_index()?;
        let entry = &mut self.entries[index];
        entry.state = PendingState::Sending;
        let content = match &entry.image {
            Some(image) => Content::Image(image.clone()),
            None => Content::Text(entry.body.clone(), entry.relation.clone()),
        };
        Some(Job {
            local_id: entry.local_id,
            room_id: entry.room_id.clone(),
            content,
            transaction_id: entry.transaction_id.clone(),
        })
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
                // Nothing will send it again.
                entry.image = None;
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

    /// Whether a message still holds its image bytes (for tests).
    pub fn holds_image(&self, local_id: u32) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.local_id == local_id && entry.image.is_some())
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// The size of `text` as a JSON string, escapes and quotes included.
fn json_size(text: &str) -> usize {
    serde_json::to_string(text).map_or(usize::MAX / 2, |json| json.len())
}

/// The most a message can take in a pending-list reply.
fn entry_size(room_id: &str, body: &str) -> usize {
    json_size(body)
        .saturating_add(json_size(room_id))
        .saturating_add(ENTRY_OVERHEAD)
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
        let a1 = outbox.queue("!a:s", body("a1"), None).unwrap().local_id;
        let a2 = outbox.queue("!a:s", body("a2"), None).unwrap().local_id;
        let b1 = outbox.queue("!b:s", body("b1"), None).unwrap().local_id;
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

    /// GIVEN a reply WHEN it is sent, fails and is sent again THEN each job
    /// carries the event it replies to.
    #[test]
    fn given_a_reply_then_each_send_carries_its_relation() {
        let mut outbox = Outbox::default();
        let reply = Relation::Reply(OwnedEventId::try_from("$original").unwrap());
        let id = outbox
            .queue("!a:s", body("yes"), Some(reply.clone()))
            .unwrap()
            .local_id;
        let relation = |job: Job| match job.content {
            Content::Text(_, relation) => relation,
            Content::Image(_) => panic!("text"),
        };
        assert_eq!(relation(outbox.next_job().unwrap()), Some(reply.clone()));
        outbox.finish(id, SendOutcome::Unknown);
        outbox.retry(id);
        assert_eq!(relation(outbox.next_job().unwrap()), Some(reply));
    }

    fn image(budgets: &Arc<persephone_media_protocol::ResourceBudgets>) -> Arc<ImageUpload> {
        Arc::new(ImageUpload::for_tests(budgets, b"\x89PNG\r\n\x1a\nimage"))
    }

    /// GIVEN an unsent image WHEN another image is queued THEN it is refused
    /// whether the first is queued, sending, failed or unknown, while text
    /// still queues; once the first is sent or discarded another fits.
    #[test]
    fn given_an_unsent_image_then_a_second_image_waits_its_turn() {
        let budgets = Arc::new(persephone_media_protocol::ResourceBudgets::default());
        let mut outbox = Outbox::default();
        let first = outbox.queue_image("!a:s", image(&budgets)).unwrap();
        assert_eq!((first.kind, first.body.as_str()), (PendingKind::Image, ""));
        assert!(
            outbox.queue_image("!b:s", image(&budgets)).is_none(),
            "queued"
        );
        assert_eq!(
            outbox.queue("!a:s", body("text"), None).unwrap().kind,
            PendingKind::Text
        );
        let job = outbox.next_job().unwrap();
        assert!(matches!(job.content, Content::Image(_)));
        assert!(
            outbox.queue_image("!b:s", image(&budgets)).is_none(),
            "sending"
        );
        outbox.finish(job.local_id, SendOutcome::Unknown);
        assert!(
            outbox.queue_image("!b:s", image(&budgets)).is_none(),
            "unknown"
        );
        outbox.retry(job.local_id);
        let job = outbox.next_job().unwrap();
        outbox.finish(job.local_id, SendOutcome::Failed(SendFailure::Rejected));
        assert!(
            outbox.queue_image("!b:s", image(&budgets)).is_none(),
            "failed"
        );
        outbox.discard(job.local_id);
        assert!(outbox.queue_image("!b:s", image(&budgets)).is_some());
    }

    /// GIVEN an image whose delivery is unknown WHEN it is retried THEN the
    /// same transaction ID and the same bytes go again; once sent, its bytes
    /// are dropped while its entry stays to match the timeline.
    #[test]
    fn given_an_image_retry_then_it_keeps_its_id_and_drops_its_bytes_once_sent() {
        let budgets = Arc::new(persephone_media_protocol::ResourceBudgets::default());
        let mut outbox = Outbox::default();
        let id = outbox
            .queue_image("!a:s", image(&budgets))
            .unwrap()
            .local_id;
        let first = outbox.next_job().unwrap();
        let transaction = first.transaction_id.clone();
        drop(first);
        outbox.finish(id, SendOutcome::Unknown);
        assert!(outbox.holds_image(id), "kept for the retry");
        outbox.retry(id);
        let again = outbox.next_job().unwrap();
        assert_eq!(again.transaction_id, transaction);
        let Content::Image(bytes) = &again.content else {
            panic!("an image job");
        };
        assert_eq!(bytes.bytes(), b"\x89PNG\r\n\x1a\nimage");
        drop(again);
        outbox.finish(id, SendOutcome::Sent("$image".into()));
        assert!(!outbox.holds_image(id));
        assert_eq!(
            budgets.class_used(persephone_media_protocol::BudgetClass::EncodedInFlight),
            0,
            "the stripped bytes were released"
        );
        let view = &outbox.for_room("!a:s")[0];
        assert_eq!(
            (view.kind, view.state, view.event_id.as_deref()),
            (PendingKind::Image, PendingState::Sent, Some("$image"))
        );
        // A lock clears the outbox, images included.
        outbox.queue_image("!a:s", image(&budgets)).unwrap();
        outbox.clear();
        assert_eq!(budgets.aggregate_used(), 0);
    }

    /// GIVEN states WHEN retrying or discarding THEN only failed and unknown
    /// messages retry, a message being sent cannot be discarded, and an
    /// unknown ID changes nothing.
    #[test]
    fn given_each_state_then_retry_and_discard_apply_only_where_safe() {
        let mut outbox = Outbox::default();
        let id = outbox.queue("!a:s", body("x"), None).unwrap().local_id;
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
            assert!(outbox.queue(&format!("!r{n}:s"), body("x"), None).is_some());
        }
        assert!(
            outbox.queue("!r:s", body("x"), None).is_none(),
            "unsent capped"
        );
        for _ in 0..MAX_UNSENT {
            let job = outbox.next_job().unwrap();
            outbox.finish(
                job.local_id,
                SendOutcome::Sent(format!("${}", job.local_id)),
            );
        }
        for n in 0..MAX_UNSENT {
            assert!(outbox.queue(&format!("!s{n}:s"), body("y"), None).is_some());
        }
        // Full: one more drops the oldest sent message.
        let job = outbox.next_job().unwrap();
        outbox.finish(job.local_id, SendOutcome::Sent("$late".into()));
        assert!(outbox.queue("!t:s", body("z"), None).is_some());
        assert!(outbox.for_room("!r0:s").is_empty(), "oldest sent dropped");
        assert_eq!(outbox.for_room("!r1:s").len(), 1);
    }

    /// GIVEN messages in each state WHEN the sender asks whether one is ready
    /// THEN asking changes nothing, and the answer matches what `next_job`
    /// would take.
    #[test]
    fn given_each_state_then_has_job_only_looks() {
        let mut outbox = Outbox::default();
        assert!(!outbox.has_job());
        let id = outbox.queue("!a:s", body("x"), None).unwrap().local_id;
        assert!(outbox.has_job());
        assert!(outbox.has_job(), "asking twice changes nothing");
        assert_eq!(outbox.for_room("!a:s")[0].state, PendingState::Queued);
        outbox.queue("!a:s", body("y"), None).unwrap();
        assert_eq!(outbox.next_job().unwrap().local_id, id);
        assert!(!outbox.has_job(), "the next one in the room waits");
        outbox.finish(id, SendOutcome::Failed(SendFailure::Failed));
        assert!(!outbox.has_job(), "still waits behind the failure");
        outbox.discard(id);
        assert!(outbox.has_job());
    }

    /// GIVEN large bodies WHEN they are queued THEN the kept messages stay
    /// within `MAX_KEPT_BYTES`: unsent ones that do not fit are refused, and
    /// sent ones make room oldest first. Escaped characters count at their
    /// escaped size.
    #[test]
    fn given_large_bodies_then_the_kept_messages_stay_within_the_byte_budget() {
        let large = "\"".repeat(8 * 1024); // 16 KiB once escaped
        let total = |outbox: &Outbox| {
            outbox
                .for_room("!a:s")
                .iter()
                .map(|message| entry_size(&message.room_id, &message.body))
                .sum::<usize>()
        };
        let mut outbox = Outbox::default();
        let mut fitted = 0;
        while outbox.queue("!a:s", body(&large), None).is_some() {
            fitted += 1;
        }
        assert_eq!(fitted, MAX_KEPT_BYTES / entry_size("!a:s", &large));
        assert!(total(&outbox) <= MAX_KEPT_BYTES);
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
            outbox.queue("!a:s", body(&large), None).is_some(),
            "sent ones make room"
        );
        assert!(total(&outbox) <= MAX_KEPT_BYTES);
        assert!(
            outbox
                .for_room("!a:s")
                .iter()
                .all(|message| message.local_id != first),
            "oldest sent dropped"
        );
    }

    /// GIVEN every field at its longest WHEN one message is written as JSON
    /// THEN it takes no more than its budgeted size, comma included.
    #[test]
    fn given_the_longest_fields_then_an_entry_fits_its_budget() {
        let room = format!("!{}", "\"".repeat(254));
        let text = "\u{1}".repeat(100);
        let longest = PendingMessage {
            local_id: u32::MAX,
            room_id: room.clone(),
            kind: PendingKind::Image,
            body: text.clone(),
            state: PendingState::Unknown,
            event_id: Some(format!("${}", "\u{1}".repeat(254))),
            failure: Some(SendFailure::ConfirmationRequired),
            queued_at: UnixMillis::saturating(u64::MAX),
        };
        let written = serde_json::to_vec(&longest).unwrap().len() + 1;
        assert!(written <= entry_size(&room, &text), "{written} bytes");
    }

    /// GIVEN the worst case for one room (every room-ID, body and event-ID
    /// byte escaped, the event ID as control characters, the longest IDs)
    /// WHEN the outbox is full THEN the room's pending list, as the reply is
    /// written, fits one bounded view.
    #[test]
    fn given_the_worst_case_then_a_rooms_pending_list_fits_one_view() {
        let room = format!("!{}", "\"".repeat(254));
        // Control characters are the longest escape (`\u0001`, 6 bytes).
        let event_id = format!("${}", "\u{1}".repeat(254));
        // About 2.5 KiB escaped: 32 of these fit a body-only budget of 80 KiB,
        // and their escaped IDs then push the list past one view.
        let text = "\\".repeat(1270);
        let mut outbox = Outbox::default();
        // Fill with sent messages until the budget makes room, then with
        // unsent ones until it refuses.
        for _ in 0..MAX_KEPT * 2 {
            if outbox.queue(&room, body(&text), None).is_none() {
                break;
            }
            let job = outbox.next_job().unwrap();
            outbox.finish(job.local_id, SendOutcome::Sent(event_id.clone()));
        }
        // Sent messages carry the longest event IDs: the largest list.
        let fits = |outbox: &Outbox| {
            let reply = serde_json::to_vec(&outbox.for_room(&room)).unwrap();
            assert!(
                reply.len() <= crate::views::MAX_PAGE_BYTES,
                "{} bytes",
                reply.len()
            );
        };
        fits(&outbox);
        while outbox.queue(&room, body(&text), None).is_some() {}
        fits(&outbox);
        // And with many small messages, the count limit holds it.
        let mut outbox = Outbox::default();
        for _ in 0..MAX_KEPT * 2 {
            if outbox.queue(&room, body("x"), None).is_none() {
                break;
            }
            let job = outbox.next_job().unwrap();
            outbox.finish(job.local_id, SendOutcome::Sent(event_id.clone()));
        }
        let reply = serde_json::to_vec(&outbox.for_room(&room)).unwrap();
        assert!(reply.len() <= crate::views::MAX_PAGE_BYTES);
        assert_eq!(outbox.for_room(&room).len(), MAX_KEPT);
    }
}
