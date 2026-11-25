//! The core's side of the Windows screen helper (ADR-0067): one helper
//! process for each share, and its stop-and-join.
//!
//! The platform code (`platform/windows/screen_helper.rs`) starts the
//! process through the media broker and hands it over as a
//! [`HelperProcess`]. A [`HelperShare`] then owns it:
//!
//! * a waiter thread watches the process and reports how it exited, so the
//!   call task can map an exit to the share state ([`HelperShare::exited`]);
//! * [`HelperShare::stop_and_join`] is the explicit stop: close the helper's
//!   control pipe, wait up to [`STOP_WAIT`] for the exit, terminate the Job
//!   if it did not end, and join the waiter thread;
//! * dropping a share that was not stopped does the same on a reaper thread,
//!   so no helper outlives its share, the call or the core's session.
//!
//! The code is plain `std` and runs on every OS, so its tests do too. Only
//! Windows has a helper process to put in it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use persephone_call_protocol::ScreenState;
use persephone_call_protocol::screen_helper::state_for_exit;
use tokio::sync::oneshot;

/// How long a helper gets to end after its control pipe closed. It checks
/// the pipe about every 20 ms, so an end takes far less. After this the Job
/// is terminated.
pub const STOP_WAIT: Duration = Duration::from_secs(2);
/// How long a terminated helper gets to be seen as ended. The kernel ends a
/// terminated Job at once; this only bounds a fault.
const TERMINATE_WAIT: Duration = Duration::from_secs(5);
/// How often the waiter thread looks at the process.
const WATCH_INTERVAL: Duration = Duration::from_millis(50);

/// Helpers that were started and whose stop-and-join is not finished.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// The number of helpers that still run or are being stopped.
pub fn helpers_active() -> usize {
    ACTIVE.load(Ordering::Acquire)
}

/// Waits up to `timeout` until no helper is active. A new share starts its
/// helper only after this, so two helpers never run at once. Returns whether
/// none is active.
pub fn wait_until_idle(timeout: Duration) -> bool {
    let started = Instant::now();
    while helpers_active() > 0 {
        if started.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

/// What the core needs from a helper process. The Windows platform code
/// implements it over the media broker's Job; a test implements it with a
/// fake.
pub trait HelperProcess: Send + Sync + 'static {
    /// Asks the helper to end: closes the control pipe. Idempotent.
    fn close_control(&self);
    /// Waits up to `timeout` for the process to exit. Returns whether it did.
    fn wait_exit(&self, timeout: Duration) -> bool;
    /// Ends the process now (terminates its Job). Idempotent.
    fn terminate(&self);
    /// The exit code, once the process has exited.
    fn exit_code(&self) -> Option<u32>;
    /// The peak committed memory of the process in bytes; 0 when unknown.
    /// Only a test of the real helper reads it.
    #[cfg_attr(not(test), allow(dead_code))]
    fn peak_memory(&self) -> usize {
        0
    }
}

/// The exit of a helper, set by its waiter thread.
struct Exit {
    code: Mutex<Option<u32>>,
    changed: Condvar,
}

impl Exit {
    fn set(&self, code: u32) {
        *self.code.lock().unwrap_or_else(|p| p.into_inner()) = Some(code);
        self.changed.notify_all();
    }

    /// Waits up to `timeout` for the exit; gives its code.
    fn wait(&self, timeout: Duration) -> Option<u32> {
        let guard = self.code.lock().unwrap_or_else(|p| p.into_inner());
        let (guard, _) = self
            .changed
            .wait_timeout_while(guard, timeout, |code| code.is_none())
            .unwrap_or_else(|p| p.into_inner());
        *guard
    }
}

/// The parts of a running share's helper that stop-and-join needs.
struct Running {
    process: Arc<dyn HelperProcess>,
    exit: Arc<Exit>,
    waiter: JoinHandle<()>,
}

impl Running {
    /// The explicit stop: close the control pipe, wait for the exit,
    /// terminate the Job if it did not come, and join the waiter thread.
    /// Gives the exit code, if one was seen. Always ends in bounded time.
    fn stop_and_join(self) -> Option<u32> {
        self.process.close_control();
        let mut code = self.exit.wait(STOP_WAIT);
        if code.is_none() {
            self.process.terminate();
            code = self.exit.wait(TERMINATE_WAIT);
        }
        // The waiter ends once it saw the exit. A fault that left no exit
        // after the terminate leaves the thread to end with the process
        // object; the join would wait for it.
        if code.is_some() {
            let _ = self.waiter.join();
        }
        ACTIVE.fetch_sub(1, Ordering::AcqRel);
        code
    }
}

/// One share's helper. Dropping it stops the helper on a reaper thread; use
/// [`HelperShare::stop_and_join`] to stop it and wait.
pub struct HelperShare {
    share: u32,
    running: Option<Running>,
    exited: Option<oneshot::Receiver<u32>>,
}

impl HelperShare {
    /// Takes over a started helper of share `share` and starts watching it.
    /// `None` when the watching thread cannot start: the helper is stopped
    /// first, so none is left running.
    pub fn start(share: u32, process: Arc<dyn HelperProcess>) -> Option<Self> {
        let exit = Arc::new(Exit {
            code: Mutex::new(None),
            changed: Condvar::new(),
        });
        let (sender, exited) = oneshot::channel();
        ACTIVE.fetch_add(1, Ordering::AcqRel);
        let watched = process.clone();
        let seen = exit.clone();
        let waiter = std::thread::Builder::new()
            .name("screen-helper-waiter".to_owned())
            .spawn(move || {
                while !watched.wait_exit(WATCH_INTERVAL) {}
                // An exit the OS cannot name fails the share closed.
                let code = watched.exit_code().unwrap_or(u32::MAX);
                seen.set(code);
                let _ = sender.send(code);
            });
        let Ok(waiter) = waiter else {
            process.close_control();
            process.terminate();
            ACTIVE.fetch_sub(1, Ordering::AcqRel);
            return None;
        };
        Some(Self {
            share,
            running: Some(Running {
                process,
                exit,
                waiter,
            }),
            exited: Some(exited),
        })
    }

    /// The number of the share this helper serves.
    pub fn share(&self) -> u32 {
        self.share
    }

    /// Waits for the helper to exit by itself and gives the share state its
    /// exit code means (`state_for_exit`). Never ends while the helper runs,
    /// and never ends after it was reported once.
    pub async fn exited(&mut self) -> ScreenState {
        let Some(receiver) = self.exited.as_mut() else {
            return std::future::pending().await;
        };
        let state = match receiver.await {
            Ok(code) => state_for_exit(code),
            // The waiter ended without an exit: fail closed.
            Err(_) => ScreenState::Unavailable,
        };
        self.exited = None;
        state
    }

    /// Stops the helper and waits for it: closes its control pipe, waits up
    /// to [`STOP_WAIT`], terminates its Job if it still runs, and joins the
    /// waiter thread. Blocks, so call it off the runtime threads. Gives the
    /// exit code, if one was seen.
    pub fn stop_and_join(mut self) -> Option<u32> {
        self.running.take()?.stop_and_join()
    }
}

impl std::fmt::Debug for HelperShare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelperShare")
            .field("share", &self.share)
            .field("running", &self.running.is_some())
            .finish()
    }
}

impl Drop for HelperShare {
    fn drop(&mut self) {
        let Some(running) = self.running.take() else {
            return;
        };
        // Ask for the end now, so the capture border and the copy stop at
        // once. The wait, the terminate and the join go to a reaper thread:
        // a drop can come on a runtime thread.
        running.process.close_control();
        let process = running.process.clone();
        let reaper = std::thread::Builder::new()
            .name("screen-helper-reaper".to_owned())
            .spawn(move || {
                running.stop_and_join();
            });
        if reaper.is_err() {
            // No thread: the Job ends the process at once.
            process.terminate();
            ACTIVE.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// A helper process that tests drive.
    pub struct FakeHelper {
        pub controls_closed: AtomicUsize,
        pub terminated: AtomicBool,
        /// Whether closing the control pipe makes the process exit.
        pub exits_on_close: bool,
        pub code: Mutex<Option<u32>>,
        exited: (Mutex<bool>, Condvar),
    }

    impl FakeHelper {
        pub fn new(exits_on_close: bool, code_on_exit: u32) -> Arc<Self> {
            Arc::new(Self {
                controls_closed: AtomicUsize::new(0),
                terminated: AtomicBool::new(false),
                exits_on_close,
                code: Mutex::new(Some(code_on_exit)),
                exited: (Mutex::new(false), Condvar::new()),
            })
        }

        /// Whether the process has ended within `timeout`.
        pub fn wait_exit_for_test(self: &Arc<Self>, timeout: Duration) -> bool {
            HelperProcess::wait_exit(self, timeout)
        }

        /// The process ends by itself, as a crash or a finished capture does.
        pub fn exit(&self) {
            *self.exited.0.lock().unwrap() = true;
            self.exited.1.notify_all();
        }
    }

    impl HelperProcess for Arc<FakeHelper> {
        fn close_control(&self) {
            self.controls_closed.fetch_add(1, Ordering::SeqCst);
            if self.exits_on_close {
                self.exit();
            }
        }

        fn wait_exit(&self, timeout: Duration) -> bool {
            let guard = self.exited.0.lock().unwrap();
            let (guard, _) = self
                .exited
                .1
                .wait_timeout_while(guard, timeout, |done| !*done)
                .unwrap();
            *guard
        }

        fn terminate(&self) {
            self.terminated.store(true, Ordering::SeqCst);
            *self.code.lock().unwrap() = Some(1);
            self.exit();
        }

        fn exit_code(&self) -> Option<u32> {
            if *self.exited.0.lock().unwrap() {
                *self.code.lock().unwrap()
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeHelper;
    use super::*;
    use persephone_call_protocol::screen_helper::{
        EXIT_BAD_START, EXIT_NOT_FOUND, EXIT_STOPPED, EXIT_UNAVAILABLE,
    };
    use std::sync::atomic::Ordering;

    fn start(process: &Arc<FakeHelper>, share: u32) -> HelperShare {
        HelperShare::start(share, Arc::new(process.clone())).expect("a helper share")
    }

    /// GIVEN a helper that ends when its control pipe closes WHEN the share
    /// is stopped and joined THEN the control pipe was closed, the Job was
    /// not terminated, the code is the helper's, and no helper is active.
    #[test]
    fn given_a_helper_that_obeys_when_stopped_then_it_is_joined_without_a_terminate() {
        let helper = FakeHelper::new(true, EXIT_STOPPED);
        let share = start(&helper, 3);
        assert_eq!(share.share(), 3);
        assert!(helpers_active() >= 1);
        assert_eq!(share.stop_and_join(), Some(EXIT_STOPPED));
        assert_eq!(helper.controls_closed.load(Ordering::SeqCst), 1);
        assert!(!helper.terminated.load(Ordering::SeqCst));
        assert!(wait_until_idle(Duration::from_secs(5)));
    }

    /// GIVEN a helper that ignores its control pipe WHEN the share is
    /// stopped and joined THEN the Job is terminated after the wait and the
    /// join still ends, in about the stop wait.
    #[test]
    fn given_a_helper_that_hangs_when_stopped_then_the_job_is_terminated_after_the_wait() {
        let helper = FakeHelper::new(false, EXIT_STOPPED);
        let share = start(&helper, 4);
        let started = Instant::now();
        assert_eq!(share.stop_and_join(), Some(1));
        let waited = started.elapsed();
        assert!(helper.terminated.load(Ordering::SeqCst));
        assert!(waited >= STOP_WAIT, "{waited:?}");
        assert!(waited < STOP_WAIT + Duration::from_secs(2), "{waited:?}");
        assert!(wait_until_idle(Duration::from_secs(5)));
    }

    /// GIVEN a share whose helper exits by itself with each code WHEN the
    /// share waits for the exit THEN the state is the code's: a stop is off,
    /// a missing monitor is not found, and a failure, a bad start or a crash
    /// status is unavailable.
    #[tokio::test]
    async fn given_a_helper_that_exits_when_waited_for_then_the_code_maps_to_the_state() {
        for (code, state) in [
            (EXIT_STOPPED, ScreenState::Off),
            (EXIT_NOT_FOUND, ScreenState::NotFound),
            (EXIT_UNAVAILABLE, ScreenState::Unavailable),
            (EXIT_BAD_START, ScreenState::Unavailable),
            (0xC000_0005, ScreenState::Unavailable),
        ] {
            let (helper, mut share) = {
                let helper = FakeHelper::new(true, code);
                let share = start(&helper, 1);
                (helper, share)
            };
            helper.exit();
            assert_eq!(share.exited().await, state, "{code:#x}");
            let _ = tokio::task::spawn_blocking(move || share.stop_and_join())
                .await
                .unwrap();
        }
    }

    /// GIVEN a share that was reported ended once WHEN it waits again THEN
    /// it never ends, so the call task's select does not spin on it.
    #[tokio::test]
    async fn given_an_exit_reported_once_when_waited_again_then_it_stays_pending() {
        let helper = FakeHelper::new(true, EXIT_NOT_FOUND);
        let mut share = { start(&helper, 1) };
        helper.exit();
        assert_eq!(share.exited().await, ScreenState::NotFound);
        let again = tokio::time::timeout(Duration::from_millis(100), share.exited()).await;
        assert!(again.is_err());
        let _ = tokio::task::spawn_blocking(move || share.stop_and_join())
            .await
            .unwrap();
    }

    /// GIVEN a share that is dropped (a call that ends, a stop that races, a
    /// panic) WHEN the reaper has run THEN the control pipe was closed at
    /// once, a helper that hangs is terminated, none stays active, and
    /// `wait_until_idle` sees it.
    #[test]
    fn given_a_dropped_share_then_the_helper_is_stopped_and_reaped() {
        let polite = FakeHelper::new(true, EXIT_STOPPED);
        drop(start(&polite, 1));
        assert_eq!(polite.controls_closed.load(Ordering::SeqCst), 1);
        assert!(wait_until_idle(Duration::from_secs(10)));
        assert!(!polite.terminated.load(Ordering::SeqCst));

        let stubborn = FakeHelper::new(false, EXIT_STOPPED);
        drop(start(&stubborn, 2));
        assert_eq!(stubborn.controls_closed.load(Ordering::SeqCst), 1);
        assert!(
            !wait_until_idle(Duration::from_millis(200)),
            "still stopping"
        );
        assert!(wait_until_idle(STOP_WAIT + Duration::from_secs(3)));
        assert!(stubborn.terminated.load(Ordering::SeqCst));
        assert!(wait_until_idle(Duration::from_secs(5)));
    }
}
