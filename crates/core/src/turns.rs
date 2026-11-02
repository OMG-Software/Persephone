//! Operations of one kind that take turns instead of refusing each other.
//!
//! One operation runs at a time. Up to a fixed number more may wait for it;
//! a further one is `limit_exceeded` at once, so a page cannot make the core
//! hold an unbounded number of waiting threads. A place in the queue is taken
//! on the serving thread without blocking (`Queue::join`); the wait happens
//! on the call's own thread (`Ticket::wait`) and ends with `limit_exceeded`
//! after a time limit. A waiting call holds no session lease: it is admitted
//! again when its turn comes, so a lock never waits on the queue.

use persephone_core_protocol::ErrorCode;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

#[derive(Default)]
struct State {
    running: bool,
    // Places taken and not yet turned into a turn or given back.
    waiting: usize,
}

pub struct Queue {
    state: Mutex<State>,
    turn: Condvar,
    places: usize,
}

impl Queue {
    /// A queue where up to `places` operations may wait while one runs.
    pub fn new(places: usize) -> Self {
        Self {
            state: Mutex::new(State::default()),
            turn: Condvar::new(),
            places,
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A poisoned lock means a panic mid-update; the counts stay usable.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes a place without blocking; `limit_exceeded` when every place is
    /// taken.
    pub fn join(&self) -> Result<Ticket<'_>, ErrorCode> {
        let mut state = self.state();
        if state.waiting >= self.places {
            return Err(ErrorCode::LimitExceeded);
        }
        state.waiting += 1;
        Ok(Ticket {
            queue: self,
            counted: true,
        })
    }
}

/// A place in the queue. Dropping it gives the place back.
pub struct Ticket<'a> {
    queue: &'a Queue,
    counted: bool,
}

impl<'a> Ticket<'a> {
    /// Waits until no other operation runs, for at most `limit`; then this
    /// one runs until the turn is dropped. `limit_exceeded` when the time
    /// runs out first.
    pub fn wait(mut self, limit: Duration) -> Result<Turn<'a>, ErrorCode> {
        let queue = self.queue;
        let deadline = Instant::now().checked_add(limit);
        let mut state = queue.state();
        while state.running {
            let left = deadline.map_or(limit, |deadline| {
                deadline.saturating_duration_since(Instant::now())
            });
            if left.is_zero() {
                // The place is given back when `self` drops, after the guard.
                drop(state);
                return Err(ErrorCode::LimitExceeded);
            }
            state = queue
                .turn
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        state.running = true;
        state.waiting -= 1;
        self.counted = false;
        Ok(Turn(queue))
    }
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        if self.counted {
            self.queue.state().waiting -= 1;
        }
    }
}

/// The running operation's turn. Dropping it lets the next one run.
pub struct Turn<'a>(&'a Queue);

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.0.state().running = false;
        self.0.turn.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// GIVEN a queue with two places WHEN a third place is asked for THEN it
    /// is refused at once, and a place given back can be taken again.
    #[test]
    fn given_every_place_taken_when_joined_then_refused_until_one_is_given_back() {
        let queue = Queue::new(2);
        let first = queue.join().unwrap();
        let _second = queue.join().unwrap();
        assert_eq!(queue.join().err(), Some(ErrorCode::LimitExceeded));
        drop(first);
        assert!(queue.join().is_ok());
    }

    /// GIVEN nothing running WHEN a ticket waits THEN it runs at once, and its
    /// place is free again while it runs.
    #[test]
    fn given_nothing_running_when_waited_then_the_turn_is_immediate() {
        let queue = Queue::new(1);
        let turn = queue.join().unwrap().wait(Duration::ZERO).unwrap();
        // The running operation holds no place: another may wait for it.
        let waiting = queue.join().unwrap();
        assert_eq!(queue.join().err(), Some(ErrorCode::LimitExceeded));
        drop(turn);
        assert!(waiting.wait(Duration::ZERO).is_ok());
    }

    /// GIVEN an operation running WHEN another waits past its limit THEN it
    /// is refused, and its place is given back.
    #[test]
    fn given_an_operation_running_when_the_wait_runs_out_then_limit_exceeded() {
        let queue = Queue::new(1);
        let _running = queue.join().unwrap().wait(Duration::ZERO).unwrap();
        let ticket = queue.join().unwrap();
        assert_eq!(
            ticket.wait(Duration::from_millis(20)).err(),
            Some(ErrorCode::LimitExceeded)
        );
        assert!(queue.join().is_ok(), "the place was given back");
    }

    /// GIVEN an operation running WHEN it ends while another waits THEN the
    /// waiting one runs, and only then.
    #[test]
    fn given_a_waiting_operation_when_the_running_one_ends_then_it_runs() {
        let queue = Queue::new(1);
        let running = queue.join().unwrap().wait(Duration::ZERO).unwrap();
        let ticket = queue.join().unwrap();
        let (started, seen) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let turn = ticket.wait(Duration::from_secs(10));
                started.send(turn.is_ok()).unwrap();
            });
            assert!(
                seen.recv_timeout(Duration::from_millis(50)).is_err(),
                "it waits while the first one runs"
            );
            drop(running);
            assert_eq!(seen.recv_timeout(Duration::from_secs(10)), Ok(true));
        });
    }
}
