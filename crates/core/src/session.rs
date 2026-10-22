//! The session owner: the core's single authority over the protected stores
//! and the Matrix account that uses them.
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
//! 4. Join the sync thread, drop the SDK client, close the stores, then
//!    release the profile. A failed close leaves `lock_failed`.
//!
//! The sync loop and sign-in take one lease per unit of work and check for a
//! lock only between complete SDK operations. The SDK's own background tasks
//! are not leases: once a client has run, a completed wait does not prove they
//! stopped, so the session ends `restart_required` and will not reopen the
//! stores in this process. [`TrackedDrain::require_full_sdk_drain`] always
//! refuses until the SDK provides a stop-and-join contract.

use crate::account;
use crate::credentials::{CredentialSource, PromptError};
use crate::homeserver::Homeserver;
use crate::store::{self, Stores};
use crate::vault::KeyProtector;
use matrix_sdk::Client;
use persephone_contracts::{AccountState, AccountStatus, SignInError, StoreState};
use std::{
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// Most leases outstanding at once.
pub const MAX_LEASES: usize = 8;
/// Longest wait between sync attempts after a failure.
const MAX_SYNC_BACKOFF: Duration = Duration::from_secs(30);
/// Shortest time between the starts of two sync rounds. A homeserver should
/// long-poll; one that answers at once must not make the core spin.
const MIN_SYNC_INTERVAL: Duration = Duration::from_millis(500);

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

/// Receives the session's changes. Called outside the session's own locks.
pub trait SessionListener: Send + Sync {
    /// A sync round touched these rooms.
    fn rooms_changed(&self, rooms: &[String]);
    /// The account's status changed.
    fn session_changed(&self);
}

struct Inner {
    state: StoreState,
    stores: Option<Arc<Stores>>,
    generation: u64,
    active: usize,
}

struct ActiveAccount {
    client: Client,
    sync: Option<JoinHandle<()>>,
}

struct Shared {
    inner: Mutex<Inner>,
    /// Notified when a lease ends or a lock begins.
    drained: Condvar,
    status: Mutex<AccountStatus>,
    account: Mutex<Option<ActiveAccount>>,
    listener: Mutex<Option<Arc<dyn SessionListener>>>,
    /// Set once any SDK client was built over these stores.
    client_ran: AtomicBool,
}

fn guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock(shared: &Shared) -> MutexGuard<'_, Inner> {
    guard(&shared.inner)
}

impl Shared {
    fn admit(self: &Arc<Self>) -> Result<Lease, AdmissionError> {
        let mut inner = lock(self);
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
            shared: self.clone(),
            generation: inner.generation,
            stores: Some(stores),
        })
    }

    fn listener(&self) -> Option<Arc<dyn SessionListener>> {
        guard(&self.listener).clone()
    }

    fn set_status(&self, status: AccountStatus) {
        *guard(&self.status) = status;
        if let Some(listener) = self.listener() {
            listener.session_changed();
        }
    }

    /// Waits up to `period`; returns `false` early once the session is no
    /// longer open.
    fn wait_open(&self, period: Duration) -> bool {
        let inner = lock(self);
        let (inner, _) = self
            .drained
            .wait_timeout_while(inner, period, |inner| inner.state == StoreState::Protected)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.state == StoreState::Protected
    }
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

fn failed(error: SignInError) -> AccountStatus {
    AccountStatus {
        state: AccountState::SignInFailed,
        user_id: None,
        error: Some(error),
    }
}

fn signed_in(user_id: &str, error: Option<SignInError>) -> AccountStatus {
    AccountStatus {
        state: AccountState::SignedIn,
        // Matrix user IDs are at most 255 bytes; anything else is not shown.
        user_id: (user_id.len() <= 255 && !user_id.chars().any(char::is_control))
            .then(|| user_id.to_owned()),
        error,
    }
}

/// Starts the account: records it, then runs its sync loop on a thread.
/// Refused (and the client dropped) if a lock began since `lease`.
fn install(shared: &Arc<Shared>, lease: Lease, client: Client, user_id: String) {
    let installed = lease.commit(|| {
        *guard(&shared.account) = Some(ActiveAccount {
            client: client.clone(),
            sync: None,
        });
    });
    if installed.is_err() {
        // A lock began first: the account never starts.
        {
            let _context = lease.stores().runtime().enter();
            drop(client);
        }
        drop(lease);
        shared.set_status(AccountStatus::signed_out());
        return;
    }
    let runtime = lease.stores().runtime().handle().clone();
    drop(lease);
    {
        // Spawn under the account entry, so a lock that takes the entry also
        // gets the handle to join. If a lock already took it, do not start.
        let mut account = guard(&shared.account);
        let Some(entry) = account.as_mut() else {
            let _context = runtime.enter();
            drop(client);
            return;
        };
        let thread_shared = shared.clone();
        let thread_user = user_id.clone();
        let thread_runtime = runtime.clone();
        entry.sync = std::thread::Builder::new()
            .name("core-sync".into())
            .spawn(move || run_sync(&thread_shared, client, thread_user, thread_runtime))
            .ok();
    }
    shared.set_status(signed_in(&user_id, None));
}

/// The sync loop: one lease per complete round; a lock is noticed between
/// rounds. Drops its SDK values inside the stores' runtime.
fn run_sync(
    shared: &Arc<Shared>,
    client: Client,
    user_id: String,
    runtime: tokio::runtime::Handle,
) {
    let mut backoff = Duration::from_secs(1);
    let mut engine = None;
    loop {
        let lease = match shared.admit() {
            Ok(lease) => lease,
            Err(AdmissionError::Locked) => break,
            Err(AdmissionError::Saturated) => {
                if !shared.wait_open(Duration::from_millis(100)) {
                    break;
                }
                continue;
            }
        };
        let started = Instant::now();
        let outcome = lease.stores().block_on(async {
            if engine.is_none() {
                engine = Some(account::select_engine(&client).await?);
            }
            account::sync_round(&client, engine.as_ref().expect("selected above")).await
        });
        let committed = lease.commit(|| ()).is_ok();
        drop(lease);
        if !committed {
            break;
        }
        match outcome {
            Ok(rooms) => {
                backoff = Duration::from_secs(1);
                if guard(&shared.status).error.is_some() {
                    shared.set_status(signed_in(&user_id, None));
                }
                if !rooms.is_empty()
                    && let Some(listener) = shared.listener()
                {
                    listener.rooms_changed(&rooms);
                }
                let elapsed = started.elapsed();
                if elapsed < MIN_SYNC_INTERVAL && !shared.wait_open(MIN_SYNC_INTERVAL - elapsed) {
                    break;
                }
            }
            Err(error) => {
                // A discovery or sync failure is reported and retried; it
                // never switches the sync mode.
                if guard(&shared.status).error != Some(error) {
                    shared.set_status(signed_in(&user_id, Some(error)));
                }
                if !shared.wait_open(backoff) {
                    break;
                }
                backoff = (backoff * 2).min(MAX_SYNC_BACKOFF);
            }
        }
    }
    let _context = runtime.enter();
    drop(engine);
    drop(client);
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
    /// the reason as its state. A saved account is restored by
    /// [`Session::restore_account`], once a listener is set.
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
                status: Mutex::new(AccountStatus::signed_out()),
                account: Mutex::new(None),
                listener: Mutex::new(None),
                client_ran: AtomicBool::new(false),
            }),
            transition: Mutex::new(()),
        }
    }

    pub fn state(&self) -> StoreState {
        lock(&self.shared).state
    }

    pub fn account_status(&self) -> AccountStatus {
        guard(&self.shared.status).clone()
    }

    /// Whether a profile was given at all. Without one, the core serves only
    /// synthetic data and there is nothing to protect.
    pub fn is_configured(&self) -> bool {
        self.profile.is_some()
    }

    pub fn set_listener(&self, listener: Arc<dyn SessionListener>) {
        *guard(&self.shared.listener) = Some(listener);
    }

    pub fn admit(&self) -> Result<Lease, AdmissionError> {
        self.shared.admit()
    }

    /// A lease together with the signed-in client, for reading views.
    pub fn with_client(&self) -> Result<Option<(Lease, Client)>, AdmissionError> {
        let lease = self.admit()?;
        let client = guard(&self.shared.account)
            .as_ref()
            .map(|account| account.client.clone());
        Ok(client.map(|client| (lease, client)))
    }

    /// Restores a saved account without a prompt and starts syncing. Does
    /// nothing when no account was saved or one is already running.
    pub fn restore_account(&self) {
        if guard(&self.shared.account).is_some() {
            return;
        }
        let Ok(lease) = self.admit() else {
            return;
        };
        let restored = lease.stores().block_on(async {
            let result = account::restore(lease.stores()).await;
            if !matches!(result, Ok(None)) {
                self.shared.client_ran.store(true, Ordering::SeqCst);
            }
            result
        });
        match restored {
            Ok(Some((client, user_id))) => install(&self.shared, lease, client, user_id),
            Ok(None) => {}
            Err(error) => self.shared.set_status(failed(error)),
        }
    }

    /// Starts signing in to `homeserver` on a worker thread and returns the
    /// status at once. The prompt runs without a lease (it may stay open for
    /// minutes); the network work holds one.
    pub fn sign_in(
        &self,
        homeserver: Homeserver,
        source: Arc<dyn CredentialSource>,
    ) -> Result<AccountStatus, AdmissionError> {
        if self.state() != StoreState::Protected {
            return Err(AdmissionError::Locked);
        }
        {
            let mut status = guard(&self.shared.status);
            if matches!(
                status.state,
                AccountState::SigningIn | AccountState::SignedIn
            ) {
                return Ok(status.clone());
            }
            *status = AccountStatus {
                state: AccountState::SigningIn,
                user_id: None,
                error: None,
            };
        }
        if let Some(listener) = self.shared.listener() {
            listener.session_changed();
        }
        let shared = self.shared.clone();
        // The attempt belongs to this generation. A lock while the prompt is
        // open advances it, so the attempt is abandoned even if the session
        // was unlocked again before the prompt returned.
        let started = lock(&self.shared).generation;
        let spawned = std::thread::Builder::new()
            .name("core-sign-in".into())
            .spawn(move || {
                let still_current = || {
                    let inner = lock(&shared);
                    inner.generation == started && inner.state == StoreState::Protected
                };
                let credentials = match source.ask(&homeserver.display_host()) {
                    Ok(credentials) => credentials,
                    Err(error) => {
                        shared.set_status(if still_current() {
                            failed(match error {
                                PromptError::Cancelled => SignInError::Cancelled,
                                PromptError::Unsupported => SignInError::Unsupported,
                                PromptError::Invalid | PromptError::Failed => SignInError::Failed,
                            })
                        } else {
                            AccountStatus::signed_out()
                        });
                        return;
                    }
                };
                let lease = match shared.admit() {
                    Ok(lease) if lease.generation == started => lease,
                    _ => {
                        // Locked while the prompt was open (and perhaps
                        // unlocked since): abandon the attempt, so a later
                        // sign-in prompts again instead of reusing this one.
                        shared.set_status(AccountStatus::signed_out());
                        return;
                    }
                };
                shared.client_ran.store(true, Ordering::SeqCst);
                let result = lease.stores().block_on(async {
                    let client = account::build_client(&homeserver, lease.stores()).await?;
                    match account::sign_in(&client, &homeserver, &credentials, lease.stores()).await
                    {
                        Ok(user_id) => Ok((client, user_id)),
                        Err(error) => {
                            drop(client);
                            Err(error)
                        }
                    }
                });
                drop(credentials);
                match result {
                    Ok((client, user_id)) => install(&shared, lease, client, user_id),
                    Err(error) => {
                        let current = lease.commit(|| ()).is_ok();
                        drop(lease);
                        // After a lock the failure belongs to an abandoned
                        // attempt; report it only while the session is current.
                        shared.set_status(if current {
                            failed(error)
                        } else {
                            AccountStatus::signed_out()
                        });
                    }
                }
            });
        if spawned.is_err() {
            self.shared.set_status(failed(SignInError::Failed));
        }
        Ok(self.account_status())
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
                self.shared.drained.notify_all();
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
        // No lease remains, so the sync loop is between rounds and will see
        // the lock at its next admission; join it, then drop the client inside
        // the stores' runtime before closing them.
        if let Some(account) = guard(&self.shared.account).take() {
            if let Some(sync) = account.sync {
                let _ = sync.join();
            }
            match &stores {
                Some(stores) => {
                    let _context = stores.runtime().enter();
                    drop(account.client);
                }
                None => drop(account.client),
            }
        }
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
        let state = if !closed {
            StoreState::LockFailed
        } else if self.shared.client_ran.load(Ordering::SeqCst) {
            StoreState::RestartRequired
        } else {
            StoreState::Locked
        };
        lock(&self.shared).state = state;
        (state, closed.then_some(TrackedDrain { _private: () }))
    }

    /// Signs out (owner decisions, 21 October 2026): revokes the session on
    /// the homeserver if it can be reached, then deletes this device's local
    /// profile. The key goes first, so the encrypted stores are unreadable at
    /// once; then the session locks as usual (ending `restart_required`, since
    /// a client ran). The store files are deleted by the next start, before
    /// any client exists: the SDK's background work is not proven stopped in
    /// this process (docs/architecture/sdk-lifecycle.md), so its files are not
    /// deleted under it. A failed revocation still signs out locally and is
    /// reported as `session_not_revoked`.
    ///
    /// Returns the resulting state; without a signed-in account nothing
    /// changes. A saturated session is waited for up to `timeout` (leases
    /// last one request or one sync round); after that the sign-out has not
    /// started and `Saturated` is returned.
    pub fn sign_out(
        &self,
        timeout: Duration,
        announce: impl FnOnce(),
    ) -> Result<StoreState, AdmissionError> {
        let Some(dir) = self.profile.clone() else {
            return Ok(self.state());
        };
        let deadline = Instant::now() + timeout;
        let revoked = loop {
            match self.with_client() {
                Ok(Some((lease, client))) => {
                    break lease
                        .stores()
                        .block_on(client.matrix_auth().logout())
                        .is_ok();
                }
                Ok(None) | Err(AdmissionError::Locked) => return Ok(self.state()),
                Err(AdmissionError::Saturated) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(error),
            }
        };
        let marked = store::mark_signed_out(&dir).is_ok();
        let (state, _) = self.lock(timeout, announce);
        // The sync loop has been joined, so this status is not overwritten.
        self.shared.set_status(AccountStatus {
            state: AccountState::SignedOut,
            user_id: None,
            error: if !marked {
                Some(SignInError::Failed)
            } else if !revoked {
                Some(SignInError::SessionNotRevoked)
            } else {
                None
            },
        });
        Ok(state)
    }

    /// Reopens a locked session with the key from the vault and restores a
    /// saved account. A session that failed to open may retry; `lock_failed`,
    /// `restart_required`, `locking` and an unconfigured session cannot unlock.
    pub fn unlock(&self) -> StoreState {
        let state = {
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
                | StoreState::RestartRequired
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
        };
        if state == StoreState::Protected {
            self.restore_account();
        }
        state
    }
}
