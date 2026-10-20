//! Prototype owner for cooperative, explicitly tracked work.
//!
//! This can prove only that tasks and callback leases admitted through this
//! owner have completed. Matrix SDK internals do not expose a global drain API.

use std::{
    future::Future,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::{sync::watch, task::JoinHandle, time};

const MAX_ACTIVE_WORK: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Running,
    Draining,
    TimedOut,
    TrackedDrained,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionError {
    Closed,
    Saturated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopError {
    TimedOut,
    WorkFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IncompleteSdkDrain;

#[derive(Debug)]
struct GateState {
    phase: Phase,
    generation: u64,
    active: usize,
    failed: bool,
}

struct Gate {
    state: Mutex<GateState>,
    active_tx: watch::Sender<usize>,
    stopping_tx: watch::Sender<bool>,
}

fn lock_state(gate: &Gate) -> MutexGuard<'_, GateState> {
    match gate.state.lock() {
        Ok(state) => state,
        Err(poisoned) => {
            let mut state = poisoned.into_inner();
            state.failed = true;
            state.phase = Phase::Failed;
            gate.stopping_tx.send_replace(true);
            state
        }
    }
}

pub struct LifecycleOwner {
    gate: Arc<Gate>,
    jobs: Vec<Option<JoinHandle<Result<(), ()>>>>,
    failed: bool,
}

#[derive(Clone)]
pub struct Admission {
    gate: Arc<Gate>,
}

pub struct WorkLease {
    gate: Arc<Gate>,
    generation: u64,
}

struct OwnedJob {
    lease: WorkLease,
    succeeded: bool,
}

impl Drop for OwnedJob {
    fn drop(&mut self) {
        if !self.succeeded {
            let mut state = lock_state(&self.lease.gate);
            state.failed = true;
            state.phase = Phase::Failed;
            self.lease.gate.stopping_tx.send_replace(true);
        }
    }
}

/// Evidence that this owner's tracked work drained. It says nothing about SDK
/// tasks, detached response processing, or other work outside this owner.
#[derive(Debug)]
pub struct TrackedDrain {
    _private: (),
}

impl LifecycleOwner {
    pub fn new() -> Self {
        let (active_tx, _) = watch::channel(0);
        let (stopping_tx, _) = watch::channel(false);
        Self {
            gate: Arc::new(Gate {
                state: Mutex::new(GateState {
                    phase: Phase::Running,
                    generation: 0,
                    active: 0,
                    failed: false,
                }),
                active_tx,
                stopping_tx,
            }),
            jobs: Vec::new(),
            failed: false,
        }
    }

    pub fn admission(&self) -> Admission {
        Admission {
            gate: self.gate.clone(),
        }
    }

    pub fn phase(&self) -> Phase {
        lock_state(&self.gate).phase
    }

    /// Start work under this owner. The owner retains each JoinHandle until it
    /// has observed completion, with a fixed lifetime cap of eight handles.
    /// Put a long-running loop inside one owned job instead of spawning one job
    /// per request; completed handles still count until `stop` joins them.
    pub fn spawn<F>(&mut self, future: F) -> Result<(), AdmissionError>
    where
        F: Future<Output = Result<(), ()>> + Send + 'static,
    {
        let lease = self.admission().admit()?;
        // Handles remain retained until stop observes them. Bound total owner
        // lifetime work, even when short tasks complete during normal running.
        if self.jobs.len() >= MAX_ACTIVE_WORK {
            drop(lease);
            return Err(AdmissionError::Saturated);
        }
        self.jobs.push(Some(tokio::spawn(async move {
            // An error, panic or cancelled job closes admission immediately,
            // before stop observes its JoinHandle. Other old leases then fence
            // their updates too.
            let mut job = OwnedJob {
                lease,
                succeeded: false,
            };
            let result = future.await;
            job.succeeded = result.is_ok();
            result
        })));
        Ok(())
    }

    /// Close admission, invalidate old leases, and wait for all tracked work.
    /// Timeout or cancellation keeps the gate closed and the JoinHandles owned,
    /// so a later call can retry. This does not drain Matrix SDK internals.
    pub async fn stop(&mut self, timeout: Duration) -> Result<TrackedDrain, StopError> {
        self.close_admission();
        match time::timeout(timeout, self.drain_tracked()).await {
            Err(_) => {
                lock_state(&self.gate).phase = Phase::TimedOut;
                Err(StopError::TimedOut)
            }
            Ok(()) => {
                let mut state = lock_state(&self.gate);
                if self.failed || state.failed {
                    state.phase = Phase::Failed;
                    Err(StopError::WorkFailed)
                } else {
                    state.phase = Phase::TrackedDrained;
                    Ok(TrackedDrain { _private: () })
                }
            }
        }
    }

    fn close_admission(&self) {
        let mut state = lock_state(&self.gate);
        if state.phase == Phase::Running {
            if let Some(generation) = state.generation.checked_add(1) {
                state.generation = generation;
                state.phase = Phase::Draining;
            } else {
                state.failed = true;
                state.phase = Phase::Failed;
            }
        }
        self.gate.stopping_tx.send_replace(true);
    }

    async fn drain_tracked(&mut self) {
        for slot in &mut self.jobs {
            if let Some(handle) = slot {
                let result = handle.await;
                let failed = !matches!(result, Ok(Ok(())));
                *slot = None;
                if failed {
                    self.failed = true;
                    let mut state = lock_state(&self.gate);
                    state.failed = true;
                    state.phase = Phase::Failed;
                    self.gate.stopping_tx.send_replace(true);
                }
            }
        }

        let mut active = self.gate.active_tx.subscribe();
        loop {
            if *active.borrow_and_update() == 0 {
                return;
            }
            if active.changed().await.is_err() {
                return;
            }
        }
    }
}

impl Default for LifecycleOwner {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for LifecycleOwner {
    fn drop(&mut self) {
        // JoinHandle drop detaches. Closing admission prevents new protected
        // work from being claimed; Drop itself is never evidence of a drain.
        self.close_admission();
    }
}

impl Admission {
    pub fn stop_requested(&self) -> bool {
        *self.gate.stopping_tx.borrow()
    }

    /// Cooperatively stop an admitted loop BETWEEN complete SDK operations.
    /// Do not select this signal against and drop an in-flight SDK call.
    pub async fn wait_for_stop(&self) {
        let mut stopping = self.gate.stopping_tx.subscribe();
        while !*stopping.borrow_and_update() {
            if stopping.changed().await.is_err() {
                return;
            }
        }
    }

    pub fn admit(&self) -> Result<WorkLease, AdmissionError> {
        let mut state = lock_state(&self.gate);
        if state.phase != Phase::Running {
            return Err(AdmissionError::Closed);
        }
        if state.active >= MAX_ACTIVE_WORK {
            return Err(AdmissionError::Saturated);
        }
        state.active += 1;
        let generation = state.generation;
        self.gate.active_tx.send_replace(state.active);
        Ok(WorkLease {
            gate: self.gate.clone(),
            generation,
        })
    }
}

impl WorkLease {
    /// Run a short synchronous state update only while this lease is current.
    /// The closure must not block or perform async work; the gate lock is held
    /// so stop cannot invalidate the lease halfway through the update.
    pub fn commit<T>(&self, update: impl FnOnce() -> T) -> Result<T, AdmissionError> {
        let state = lock_state(&self.gate);
        if state.phase != Phase::Running || state.generation != self.generation {
            return Err(AdmissionError::Closed);
        }
        Ok(update())
    }
}

impl Drop for WorkLease {
    fn drop(&mut self) {
        let mut state = lock_state(&self.gate);
        if let Some(active) = state.active.checked_sub(1) {
            state.active = active;
        } else {
            state.failed = true;
            state.phase = Phase::Failed;
            self.gate.stopping_tx.send_replace(true);
        }
        self.gate.active_tx.send_replace(state.active);
    }
}

impl TrackedDrain {
    /// Refuse to treat this prototype's tracked work as a full SDK drain.
    pub fn require_full_sdk_drain(&self) -> Result<(), IncompleteSdkDrain> {
        Err(IncompleteSdkDrain)
    }
}
