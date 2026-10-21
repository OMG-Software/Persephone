//! The session owner: the core's single authority over the protected stores.
//!
//! All work that touches the stores is admitted through a [`Lease`]. Locking
//! follows `docs/architecture/sdk-lifecycle.md`:
//!
//! 1. Close admission and advance the generation, so no new lease is granted
//!    and no lease from before the lock can commit a result.
//! 2. Announce the change (the core tells the UI to clear its views).
//! 3. Wait for every admitted lease to finish. A timeout leaves the session in
//!    `locking` with the stores still open and owned; locking again resumes
//!    the wait. Nothing is released early.
//! 4. Close the stores, then release the profile. A failed close leaves
//!    `lock_failed`, which refuses unlocking until the application restarts.
//!
//! No Matrix SDK client exists yet, so every user of the stores holds a lease
//! and a completed wait covers all of them. Once an SDK client runs its own
//! tasks over these stores, a completed wait proves only that leased work
//! finished: [`TrackedDrain::require_full_sdk_drain`] always refuses until the
//! SDK provides a stop-and-join contract.

use crate::store::{self, Stores};
use crate::vault::KeyProtector;
use persephone_contracts::StoreState;
use std::{
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, MutexGuard},
    time::{Duration, Instant},
};

/// Most leases outstanding at once.
pub const MAX_LEASES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionError {
    /// The session is not open (locked, locking or never unlocked).
    Locked,
    /// [`MAX_LEASES`] leases are outstanding.
    Saturated,
}

/// Proof that leased work drained before the stores closed. It says nothing
/// about work outside the owner.
#[derive(Debug)]
pub struct TrackedDrain {
    _private: (),
}

#[derive(Debug, PartialEq, Eq)]
pub struct IncompleteSdkDrain;

impl TrackedDrain {
    /// Refuses to treat leased work as a full SDK drain.
    pub fn require_full_sdk_drain(&self) -> Result<(), IncompleteSdkDrain> {
        Err(IncompleteSdkDrain)
    }
}

struct Inner {
    state: StoreState,
    stores: Option<Arc<Stores>>,
    generation: u64,
    active: usize,
}

struct Shared {
    inner: Mutex<Inner>,
    drained: Condvar,
}

fn lock(shared: &Shared) -> MutexGuard<'_, Inner> {
    shared
        .inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct Session {
    profile: Option<PathBuf>,
    protector: Box<dyn KeyProtector>,
    shared: Arc<Shared>,
    /// Serializes lock and unlock; admission does not take it.
    transition: Mutex<()>,
}

/// Admitted work. While a lease exists the stores stay open; its results are
/// published only through [`Lease::commit`], which refuses once the session
/// has begun to lock.
pub struct Lease {
    shared: Arc<Shared>,
    generation: u64,
    /// `Some` for the lease's whole life; taken only in `drop`.
    stores: Option<Arc<Stores>>,
}

impl Lease {
    pub fn stores(&self) -> &Stores {
        self.stores.as_ref().expect("held until the lease drops")
    }

    /// Runs a short update only if the session has not begun to lock since
    /// this lease was granted. The update runs under the session's lock, so a
    /// lock cannot begin halfway through it; it must not block.
    pub fn commit<T>(&self, update: impl FnOnce() -> T) -> Result<T, AdmissionError> {
        let inner = lock(&self.shared);
        if inner.state != StoreState::Protected || inner.generation != self.generation {
            return Err(AdmissionError::Locked);
        }
        Ok(update())
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // Release the stores before counting this lease out: a locker woken
        // by the count must find the session holding the only reference.
        // (Fields drop only after this body, so it cannot be left to them.)
        drop(self.stores.take());
        let mut inner = lock(&self.shared);
        inner.active = inner.active.saturating_sub(1);
        self.shared.drained.notify_all();
    }
}

impl Session {
    /// A session with no profile: nothing is stored and nothing can unlock.
    pub fn not_configured() -> Self {
        Self::with_state(
            None,
            Box::new(crate::vault::Unsupported),
            StoreState::NotConfigured,
            None,
        )
    }

    /// Opens the profile in `dir`; on failure the session starts locked with
    /// the reason as its state.
    pub fn open(dir: PathBuf, protector: Box<dyn KeyProtector>) -> Self {
        let (state, stores) = match store::open(&dir, &*protector) {
            Ok(stores) => (StoreState::Protected, Some(Arc::new(stores))),
            Err(state) => (state, None),
        };
        Self::with_state(Some(dir), protector, state, stores)
    }

    fn with_state(
        profile: Option<PathBuf>,
        protector: Box<dyn KeyProtector>,
        state: StoreState,
        stores: Option<Arc<Stores>>,
    ) -> Self {
        Self {
            profile,
            protector,
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner {
                    state,
                    stores,
                    generation: 0,
                    active: 0,
                }),
                drained: Condvar::new(),
            }),
            transition: Mutex::new(()),
        }
    }

    pub fn state(&self) -> StoreState {
        lock(&self.shared).state
    }

    /// Whether a profile was given at all. Without one, the core serves only
    /// synthetic data and there is nothing to protect.
    pub fn is_configured(&self) -> bool {
        self.profile.is_some()
    }

    pub fn admit(&self) -> Result<Lease, AdmissionError> {
        let mut inner = lock(&self.shared);
        let Some(stores) = inner
            .stores
            .clone()
            .filter(|_| inner.state == StoreState::Protected)
        else {
            return Err(AdmissionError::Locked);
        };
        if inner.active >= MAX_LEASES {
            return Err(AdmissionError::Saturated);
        }
        inner.active += 1;
        Ok(Lease {
            shared: self.shared.clone(),
            generation: inner.generation,
            stores: Some(stores),
        })
    }

    /// Locks the session (see the module documentation). `announce` runs once,
    /// right after admission closes and before waiting, when this call is the
    /// one that began the lock. Returns the resulting state and, when the
    /// stores closed after a complete wait, the drain proof.
    pub fn lock(
        &self,
        timeout: Duration,
        announce: impl FnOnce(),
    ) -> (StoreState, Option<TrackedDrain>) {
        let _transition = self
            .transition
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let deadline = Instant::now() + timeout;
        let mut inner = lock(&self.shared);
        match inner.state {
            StoreState::Protected => {
                inner.state = match inner.generation.checked_add(1) {
                    Some(next) => {
                        inner.generation = next;
                        StoreState::Locking
                    }
                    None => StoreState::LockFailed,
                };
                drop(inner);
                announce();
                inner = lock(&self.shared);
                if inner.state == StoreState::LockFailed {
                    return (StoreState::LockFailed, None);
                }
            }
            StoreState::Locking => {}
            other => return (other, None),
        }
        while inner.active > 0 {
            let now = Instant::now();
            if now >= deadline {
                return (StoreState::Locking, None);
            }
            inner = self
                .shared
                .drained
                .wait_timeout(inner, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        let stores = inner.stores.take();
        drop(inner);
        // Each lease releases its reference before it is counted out, so with
        // no lease left the session holds the only one.
        let closed = match stores.map(Arc::try_unwrap) {
            Some(Ok(stores)) => stores.close(),
            Some(Err(still_shared)) => {
                // Something else still holds the stores: keep them, stay
                // locking, and let a later lock retry rather than failing.
                lock(&self.shared).stores = Some(still_shared);
                return (StoreState::Locking, None);
            }
            None => false,
        };
        let state = if closed {
            StoreState::Locked
        } else {
            StoreState::LockFailed
        };
        lock(&self.shared).state = state;
        (state, closed.then_some(TrackedDrain { _private: () }))
    }

    /// Reopens a locked session with the key from the vault. A session that
    /// failed to open may retry; `lock_failed`, `locking` and an unconfigured
    /// session cannot unlock.
    pub fn unlock(&self) -> StoreState {
        let _transition = self
            .transition
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = self.state();
        let Some(dir) = &self.profile else {
            return current;
        };
        match current {
            StoreState::Protected
            | StoreState::Locking
            | StoreState::LockFailed
            | StoreState::NotConfigured => return current,
            _ => {}
        }
        let (state, stores) = match store::open(dir, &*self.protector) {
            Ok(stores) => (StoreState::Protected, Some(Arc::new(stores))),
            Err(state) => (state, None),
        };
        let mut inner = lock(&self.shared);
        inner.state = state;
        inner.stores = stores;
        state
    }
}
