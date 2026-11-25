//! Leaving a call after 30 seconds alone (ADR-0063).
//!
//! The worker names every remote participant that joins or leaves, with the
//! number of the SFU connection it is on (0 is the own SFU, 1 to 3 are other
//! SFUs, ADR-0054). This type keeps that set and one deadline. The deadline
//! starts when the worker is connected and nobody else is present, stays
//! while that holds, and goes at once when anyone else is present. It never
//! sleeps itself: the call loop waits for [`Alone::due`] and then ends the
//! call through its normal leave path.
//!
//! The rule fails toward staying: a participant that the worker never
//! reported as gone, or more participants than the bound, keep the call.

use persephone_call_protocol::{FromWorker, MAX_PARTICIPANTS};
use std::{collections::HashSet, time::Duration};
use tokio::time::{self, Instant};

/// How long the own account may be the only participant before the call ends.
pub const ALONE_LIMIT: Duration = Duration::from_secs(30);

/// The own SFU and at most three other SFUs (ADR-0054).
const CONNECTIONS: usize = 4;

/// Most participants kept. More than this ends the rule for the call: the
/// call is then never left for being alone.
const MAX_PEERS: usize = CONNECTIONS * MAX_PARTICIPANTS;

#[derive(Debug, Default)]
pub struct Alone {
    connected: bool,
    peers: HashSet<(u8, String)>,
    overflow: bool,
    since: Option<Instant>,
}

impl Alone {
    /// Takes one worker message. Only `Connected` and the participants'
    /// joins and leaves matter; `now` is the time the message arrived.
    pub fn observe(&mut self, event: &FromWorker, now: Instant) {
        match event {
            FromWorker::Connected => self.connected = true,
            FromWorker::ParticipantJoined {
                connection,
                identity,
            } => {
                let key = (*connection, identity.clone());
                if self.peers.len() < MAX_PEERS || self.peers.contains(&key) {
                    self.peers.insert(key);
                } else {
                    self.overflow = true;
                }
            }
            FromWorker::ParticipantLeft {
                connection,
                identity,
            } => {
                self.peers.remove(&(*connection, identity.clone()));
            }
            // Another SFU that ended sends no leave for its participants
            // (ADR-0054), so they all go with it. A connection that comes up
            // reports its participants itself.
            FromWorker::Connection {
                connection,
                up: false,
            } => self.peers.retain(|(on, _)| on != connection),
            _ => return,
        }
        let alone = self.connected && !self.overflow && self.peers.is_empty();
        match (alone, self.since) {
            (true, None) => self.since = Some(now),
            (false, _) => self.since = None,
            (true, Some(_)) => {}
        }
    }

    /// When the call ends for being alone, if the account is alone now.
    pub fn deadline(&self) -> Option<Instant> {
        self.since.map(|since| since + ALONE_LIMIT)
    }

    /// Completes at the deadline. Never completes while someone else is
    /// present or the worker is not connected. Cancel safe.
    pub async fn due(&self) {
        match self.deadline() {
            Some(at) => time::sleep_until(at).await,
            None => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::timeout;

    fn joined(connection: u8, identity: &str) -> FromWorker {
        FromWorker::ParticipantJoined {
            connection,
            identity: identity.to_owned(),
        }
    }

    fn left(connection: u8, identity: &str) -> FromWorker {
        FromWorker::ParticipantLeft {
            connection,
            identity: identity.to_owned(),
        }
    }

    /// Whether `due` completes within `wait` of paused time.
    async fn fires_within(alone: &Alone, wait: Duration) -> bool {
        timeout(wait, alone.due()).await.is_ok()
    }

    /// GIVEN a connected call with nobody else WHEN 30 seconds pass THEN the
    /// call is due, and not a moment before.
    #[tokio::test(start_paused = true)]
    async fn given_a_call_alone_from_the_join_then_it_is_due_at_30_seconds() {
        let mut alone = Alone::default();
        alone.observe(&FromWorker::Connected, Instant::now());
        assert!(!fires_within(&alone, Duration::from_secs(29)).await);
        assert!(fires_within(&alone, Duration::from_secs(1)).await);
    }

    /// GIVEN a call that is still connecting WHEN any time passes THEN it is
    /// never due: the timer starts at the connection.
    #[tokio::test(start_paused = true)]
    async fn given_a_call_not_connected_then_the_timer_does_not_run() {
        let alone = Alone::default();
        assert_eq!(alone.deadline(), None);
        assert!(!fires_within(&alone, Duration::from_secs(300)).await);
    }

    /// GIVEN a call alone for 20 seconds WHEN someone joins THEN it is never
    /// due while that person is there.
    #[tokio::test(start_paused = true)]
    async fn given_someone_joins_at_20_seconds_then_the_call_stays() {
        let mut alone = Alone::default();
        alone.observe(&FromWorker::Connected, Instant::now());
        time::advance(Duration::from_secs(20)).await;
        alone.observe(&joined(0, "@b:example.org:B"), Instant::now());
        assert_eq!(alone.deadline(), None);
        assert!(!fires_within(&alone, Duration::from_secs(300)).await);
    }

    /// GIVEN a call with one other participant WHEN the last one leaves
    /// THEN the call is due 30 seconds after that, not from the old start.
    #[tokio::test(start_paused = true)]
    async fn given_the_last_one_leaves_then_the_timer_starts_again_from_30_seconds() {
        let mut alone = Alone::default();
        alone.observe(&FromWorker::Connected, Instant::now());
        alone.observe(&joined(1, "@b:example.org:B"), Instant::now());
        time::advance(Duration::from_secs(100)).await;
        alone.observe(&left(1, "@b:example.org:B"), Instant::now());
        assert!(!fires_within(&alone, Duration::from_secs(29)).await);
        assert!(fires_within(&alone, Duration::from_secs(1)).await);
    }

    /// GIVEN participants on two connections WHEN one leaves THEN the other
    /// still keeps the call, and a leave on another connection changes
    /// nothing.
    #[tokio::test(start_paused = true)]
    async fn given_participants_on_two_connections_then_each_keeps_the_call() {
        let mut alone = Alone::default();
        alone.observe(&FromWorker::Connected, Instant::now());
        alone.observe(&joined(0, "@b:example.org:B"), Instant::now());
        alone.observe(&joined(2, "@c:example.org:C"), Instant::now());
        alone.observe(&left(0, "@b:example.org:B"), Instant::now());
        assert_eq!(alone.deadline(), None);
        alone.observe(&left(1, "@c:example.org:C"), Instant::now());
        assert_eq!(alone.deadline(), None);
        alone.observe(&left(2, "@c:example.org:C"), Instant::now());
        assert!(alone.deadline().is_some());
    }

    /// GIVEN the only other participants on another SFU WHEN that connection
    /// ends without a leave for each THEN they are gone and the call is due
    /// 30 seconds later; an end of another connection keeps them.
    #[tokio::test(start_paused = true)]
    async fn given_another_sfu_ends_then_its_participants_go_with_it() {
        let mut alone = Alone::default();
        alone.observe(&FromWorker::Connected, Instant::now());
        alone.observe(&joined(1, "@b:example.org:B"), Instant::now());
        alone.observe(&joined(1, "@c:example.org:C"), Instant::now());
        let down = |connection| FromWorker::Connection {
            connection,
            up: false,
        };
        alone.observe(&down(2), Instant::now());
        assert_eq!(alone.deadline(), None);
        alone.observe(&down(1), Instant::now());
        assert!(!fires_within(&alone, Duration::from_secs(29)).await);
        assert!(fires_within(&alone, Duration::from_secs(1)).await);
    }

    /// GIVEN a timer that runs WHEN messages that do not change presence
    /// arrive THEN the deadline does not move.
    #[tokio::test(start_paused = true)]
    async fn given_other_messages_then_the_deadline_stays() {
        let mut alone = Alone::default();
        alone.observe(&FromWorker::Connected, Instant::now());
        let deadline = alone.deadline();
        time::advance(Duration::from_secs(10)).await;
        alone.observe(&FromWorker::Muted { muted: true }, Instant::now());
        alone.observe(&FromWorker::Connected, Instant::now());
        alone.observe(&left(0, "@nobody:example.org:X"), Instant::now());
        assert_eq!(alone.deadline(), deadline);
    }

    /// GIVEN more participants than the bound WHEN they all leave THEN the
    /// call is not left: the set no longer says who is there.
    #[tokio::test(start_paused = true)]
    async fn given_more_participants_than_the_bound_then_the_call_is_never_alone() {
        let mut alone = Alone::default();
        alone.observe(&FromWorker::Connected, Instant::now());
        for n in 0..=MAX_PEERS {
            alone.observe(&joined(0, &format!("@u{n}:example.org:D")), Instant::now());
        }
        for n in 0..=MAX_PEERS {
            alone.observe(&left(0, &format!("@u{n}:example.org:D")), Instant::now());
        }
        assert_eq!(alone.deadline(), None);
    }

    /// GIVEN an old call that ran 25 seconds alone WHEN a new call starts
    /// THEN the new one has its own timer and holds nothing of the old one.
    #[tokio::test(start_paused = true)]
    async fn given_a_new_call_then_it_does_not_inherit_the_old_timer() {
        let mut old = Alone::default();
        old.observe(&FromWorker::Connected, Instant::now());
        time::advance(Duration::from_secs(25)).await;
        let fresh = Alone::default();
        assert_eq!(fresh.deadline(), None);
        assert!(!fires_within(&fresh, Duration::from_secs(300)).await);
        assert!(old.deadline().is_some());
    }
}
