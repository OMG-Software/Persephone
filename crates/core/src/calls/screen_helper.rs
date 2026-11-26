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

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

/// One count for each launch under way ([`LaunchGuard`]) and for each helper
/// that was started and whose stop-and-join is not finished. It is one
/// counter, so one read sees a launch and its helper together: a launch
/// counts its helper before it gives up its own count, and the count never
/// falls to 0 in between.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Lets one test at a time start helpers or launches, or wait until none is
/// active: such tests share [`ACTIVE`], and a hanging helper of one holds it
/// for seconds while another waits for 0. A call's end reads it when that
/// call began a pick (also one later cancelled or refused) or held a helper,
/// and whenever the end of the session stops a call (`finish(true)`), so a
/// test that ends such a call takes it too; a test whose call the user
/// leaves without either never reads it. A tokio mutex, so an async test holds it across
/// its awaits; a plain test takes it with `blocking_lock`.
#[cfg(test)]
pub(crate) static HELPER_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The number of helpers that are being launched, still run or are being
/// stopped.
pub fn helpers_active() -> usize {
    ACTIVE.load(Ordering::SeqCst)
}

/// A launch under way. The pick takes it for its last cancel check and
/// holds it until its helper counts as active, so the end of a call, which
/// sets the cancel first and then waits until no helper is active, also
/// waits for a launch that passed the check. Either that end sees the launch
/// or the launch sees the cancel: both sides use sequentially consistent
/// order, and the count goes up before the cancel is read.
pub struct LaunchGuard(());

impl LaunchGuard {
    /// Counts a launch, then reads `cancel`. `None` when it is set: nothing
    /// may be launched then.
    pub fn begin(cancel: &AtomicBool) -> Option<Self> {
        ACTIVE.fetch_add(1, Ordering::SeqCst);
        let guard = Self(());
        if cancel.load(Ordering::SeqCst) {
            return None;
        }
        Some(guard)
    }
}

impl Drop for LaunchGuard {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
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
        // The waiter ends once it saw the exit, and it ends the helper's
        // count in `ACTIVE`. A fault that left no exit after the terminate
        // leaves the thread to end with the process, and the helper counts
        // as active until then: no other helper starts beside it.
        if code.is_some() {
            let _ = self.waiter.join();
        }
        code
    }
}

/// One share's helper. Dropping it stops the helper on a reaper thread; use
/// [`HelperShare::stop_and_join`] to stop it and wait.
pub struct HelperShare {
    share: u32,
    running: Option<Running>,
    exited: Option<oneshot::Receiver<u32>>,
    /// Set by a [`HelperStop`]: the end was asked for, so the exit is not
    /// the helper's own.
    stopping: Arc<AtomicBool>,
}

/// Asks a share's helper to end without owning it: the handle's stop uses
/// it, so the capture ends before `Off` shows, also while the call task
/// that owns the [`HelperShare`] is busy. The owner still stops and joins
/// the helper. Idempotent.
pub struct HelperStop {
    process: Arc<dyn HelperProcess>,
    stopping: Arc<AtomicBool>,
}

impl HelperStop {
    /// Closes the helper's control pipe. The exit that follows is not
    /// reported by [`HelperShare::exited`].
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.process.close_control();
    }
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
        ACTIVE.fetch_add(1, Ordering::SeqCst);
        let watched = process.clone();
        let seen = exit.clone();
        let waiter = std::thread::Builder::new()
            .name("screen-helper-waiter".to_owned())
            .spawn(move || {
                loop {
                    let looked = Instant::now();
                    if watched.wait_exit(WATCH_INTERVAL) {
                        break;
                    }
                    // A wait that failed returns at once: wait out the rest
                    // of the interval, so a fault does not spin this thread.
                    std::thread::sleep(WATCH_INTERVAL.saturating_sub(looked.elapsed()));
                }
                // An exit the OS cannot name fails the share closed.
                let code = watched.exit_code().unwrap_or(u32::MAX);
                seen.set(code);
                let _ = sender.send(code);
                // The process has ended: only now may another helper start.
                ACTIVE.fetch_sub(1, Ordering::SeqCst);
            });
        let Ok(waiter) = waiter else {
            process.close_control();
            process.terminate();
            // No waiter will see the exit: count the helper as ended only
            // when this wait sees it. Otherwise it stays active, and no
            // other helper starts.
            if process.wait_exit(TERMINATE_WAIT) {
                ACTIVE.fetch_sub(1, Ordering::SeqCst);
            }
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
            stopping: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The number of the share this helper serves.
    pub fn share(&self) -> u32 {
        self.share
    }

    /// A handle that asks this helper to end; `None` once it is stopped.
    pub fn stopper(&self) -> Option<HelperStop> {
        let running = self.running.as_ref()?;
        Some(HelperStop {
            process: running.process.clone(),
            stopping: self.stopping.clone(),
        })
    }

    /// Waits for the helper to exit by itself and gives the share state its
    /// exit code means (`state_for_exit`). Never ends while the helper runs,
    /// never ends after it was reported once, and never ends for an exit
    /// that a [`HelperStop`] asked for.
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
        if self.stopping.load(Ordering::SeqCst) {
            return std::future::pending().await;
        }
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
            // No thread: the Job ends the process at once. The waiter still
            // runs, and it ends the helper's count when it sees the exit.
            process.terminate();
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
        /// While set, a wait fails at once, as a wait on a bad handle does.
        pub failing_waits: AtomicBool,
        /// How many waits ran.
        pub waits: AtomicUsize,
        exited: (Mutex<bool>, Condvar),
    }

    impl FakeHelper {
        pub fn new(exits_on_close: bool, code_on_exit: u32) -> Arc<Self> {
            Arc::new(Self {
                controls_closed: AtomicUsize::new(0),
                terminated: AtomicBool::new(false),
                exits_on_close,
                code: Mutex::new(Some(code_on_exit)),
                failing_waits: AtomicBool::new(false),
                waits: AtomicUsize::new(0),
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
        /// Idempotent, as the broker's pipe in an `Option` is: only the first
        /// close counts. A drop closes the pipe and its reaper closes it
        /// again, in any order.
        fn close_control(&self) {
            let first = self
                .controls_closed
                .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok();
            if first && self.exits_on_close {
                self.exit();
            }
        }

        fn wait_exit(&self, timeout: Duration) -> bool {
            self.waits.fetch_add(1, Ordering::SeqCst);
            if self.failing_waits.load(Ordering::SeqCst) {
                return false;
            }
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
        let _serial = HELPER_TESTS.blocking_lock();
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
        let _serial = HELPER_TESTS.blocking_lock();
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
        let _serial = HELPER_TESTS.lock().await;
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
        let _serial = HELPER_TESTS.lock().await;
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

    /// GIVEN a helper whose waits fail at once WHEN the waiter watches it
    /// THEN it waits out each interval instead of spinning, and it still
    /// sees the exit once the waits work again.
    #[test]
    fn given_waits_that_fail_when_watched_then_the_waiter_does_not_spin() {
        let _serial = HELPER_TESTS.blocking_lock();
        let helper = FakeHelper::new(true, EXIT_STOPPED);
        helper.failing_waits.store(true, Ordering::SeqCst);
        let share = start(&helper, 5);
        std::thread::sleep(Duration::from_millis(300));
        let waits = helper.waits.load(Ordering::SeqCst);
        assert!(waits <= 10, "{waits} waits in 300 ms");
        helper.failing_waits.store(false, Ordering::SeqCst);
        assert_eq!(share.stop_and_join(), Some(EXIT_STOPPED));
    }

    /// GIVEN a helper that exits by itself WHEN its exit is seen THEN it no
    /// longer counts as active, although its share is still held: a new
    /// share does not wait for a process that has ended.
    #[test]
    fn given_a_helper_that_exits_when_its_exit_is_seen_then_it_is_not_active() {
        let _serial = HELPER_TESTS.blocking_lock();
        let helper = FakeHelper::new(true, EXIT_NOT_FOUND);
        let share = start(&helper, 6);
        helper.exit();
        assert!(
            wait_until_idle(Duration::from_secs(10)),
            "the share is held"
        );
        drop(share);
    }

    /// GIVEN a share that is dropped (a call that ends, a stop that races, a
    /// panic) WHEN the reaper has run THEN the control pipe was closed at
    /// once, a helper that hangs is terminated, none stays active, and
    /// `wait_until_idle` sees it.
    #[test]
    fn given_a_dropped_share_then_the_helper_is_stopped_and_reaped() {
        let _serial = HELPER_TESTS.blocking_lock();
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

    /// GIVEN a running share WHEN its stop handle asks for the end THEN the
    /// control pipe closes at once, the exit that follows is not reported as
    /// the helper's own, and the owner's stop-and-join still ends it; a
    /// stopped share gives no handle.
    #[tokio::test]
    async fn given_a_stop_handle_when_it_stops_the_helper_then_the_exit_is_not_its_own() {
        let _serial = HELPER_TESTS.lock().await;
        let helper = FakeHelper::new(true, EXIT_NOT_FOUND);
        let mut share = start(&helper, 8);
        let stop = share.stopper().expect("a running share has a handle");
        stop.stop();
        stop.stop();
        assert_eq!(helper.controls_closed.load(Ordering::SeqCst), 1);
        assert!(helper.wait_exit_for_test(Duration::from_secs(5)));
        assert!(
            tokio::time::timeout(Duration::from_millis(500), share.exited())
                .await
                .is_err(),
            "an end that was asked for is not the helper's own"
        );
        assert_eq!(share.stop_and_join(), Some(EXIT_NOT_FOUND));

        let mut stopped = start(&FakeHelper::new(true, EXIT_STOPPED), 9);
        stopped.running.take().expect("running").stop_and_join();
        assert!(stopped.stopper().is_none());
    }

    /// GIVEN a set cancel WHEN a launch begins THEN it is refused; GIVEN a
    /// launch under way THEN the helpers count as active until it ends, so
    /// the end of a call waits for it; GIVEN a launch that started its
    /// helper WHEN the launch ends THEN the helper still counts, until its
    /// stop-and-join.
    #[test]
    fn given_a_cancel_or_a_launch_under_way_then_the_launch_guard_says_so() {
        let _serial = HELPER_TESTS.blocking_lock();
        let cancel = AtomicBool::new(true);
        assert!(LaunchGuard::begin(&cancel).is_none());
        cancel.store(false, Ordering::SeqCst);
        let launching = LaunchGuard::begin(&cancel).expect("a launch may begin");
        assert!(helpers_active() >= 1);
        assert!(
            !wait_until_idle(Duration::from_millis(100)),
            "a launch under way is waited for"
        );
        drop(launching);
        assert!(wait_until_idle(Duration::from_secs(10)));

        let launching = LaunchGuard::begin(&cancel).expect("a launch may begin");
        let helper = FakeHelper::new(true, EXIT_STOPPED);
        let share = start(&helper, 3);
        drop(launching);
        assert!(helpers_active() >= 1, "the launch handed its count on");
        assert!(
            !wait_until_idle(Duration::from_millis(100)),
            "the started helper is waited for"
        );
        assert_eq!(share.stop_and_join(), Some(EXIT_STOPPED));
        assert!(wait_until_idle(Duration::from_secs(10)));
    }
}
