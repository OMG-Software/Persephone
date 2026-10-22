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
//! 4. Join the sync and security threads, drop the SDK client, close the
//!    stores, then release the profile. A failed close leaves `lock_failed`.
//!
//! The sync loop and sign-in take one lease per unit of work and check for a
//! lock only between complete SDK operations. The SDK's own background tasks
//! are not leases: once a client has run, a completed wait does not prove they
//! stopped, so the session ends `restart_required` and will not reopen the
//! stores in this process. [`TrackedDrain::require_full_sdk_drain`] always
//! refuses until the SDK provides a stop-and-join contract.
//!
//! Encryption setup (`security.rs`) runs on its own thread, one lease per
//! step, like sync. Its prompts run on short-lived threads without a lease, so
//! a prompt left open never holds up a lock; an answer given after a lock (or
//! a lock and unlock) is dropped.

use crate::account;
use crate::credentials::{CredentialSource, PromptError, SecretPrompt};
use crate::homeserver::Homeserver;
use crate::security;
use crate::store::{self, Stores};
use crate::vault::KeyProtector;
use matrix_sdk::{Client, ruma::OwnedUserId};
use persephone_contracts::{
    AccountState, AccountStatus, IdentityState, RecoveryState, SecurityError, SecurityOperation,
    SecurityStatus, SignInError, StoreState,
};
use std::{
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

/// Most leases outstanding at once.
pub const MAX_LEASES: usize = 8;
/// Longest wait between sync attempts after a failure.
const MAX_SYNC_BACKOFF: Duration = Duration::from_secs(30);
/// Shortest time between the starts of two sync rounds. A homeserver should
/// long-poll; one that answers at once must not make the core spin.
const MIN_SYNC_INTERVAL: Duration = Duration::from_millis(500);
/// How often the security thread checks for work.
const SECURITY_TICK: Duration = Duration::from_millis(250);
/// How often the shown encryption state is read again from the store.
const SECURITY_REFRESH: Duration = Duration::from_secs(2);
/// How often the homeserver is asked again while the account seems to have no
/// identity (another device may set one up).
const IDENTITY_REQUERY: Duration = Duration::from_secs(30);
/// Stack for the security thread: the SDK's verification futures need more
/// than the default in debug builds (`docs/evidence/verified-recipients.md`).
const SECURITY_STACK_BYTES: usize = 8 * 1024 * 1024;

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
    /// The account's encryption state changed.
    fn security_changed(&self) {}
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
    security: Option<JoinHandle<()>>,
    user_id: String,
    /// The homeserver as the prompts name it.
    host: String,
}

/// Work for the security thread, with the secret the user entered.
enum SecurityCommand {
    SetUp {
        password: Zeroizing<String>,
        source: Arc<dyn CredentialSource>,
    },
    Recover(Zeroizing<String>),
    /// Set up recovery alone, for an identity this device already holds.
    SetUpRecovery {
        source: Arc<dyn CredentialSource>,
    },
}

struct SecurityShared {
    status: SecurityStatus,
    command: Option<SecurityCommand>,
    /// This device holds the private cross-signing keys of a published
    /// identity, so recovery can be set up from it.
    can_set_up_recovery: bool,
}

impl Default for SecurityShared {
    fn default() -> Self {
        Self {
            status: SecurityStatus::unknown(),
            command: None,
            can_set_up_recovery: false,
        }
    }
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
    security: Mutex<SecurityShared>,
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

    /// The generation of the open session, or `None` while it is not open.
    fn open_generation(&self) -> Option<u64> {
        let inner = lock(self);
        (inner.state == StoreState::Protected).then_some(inner.generation)
    }

    /// Changes the encryption state only while the session is still the open
    /// one of `generation`. The check and the change happen under the lock a
    /// lock takes to advance the generation, so nothing written here outlives
    /// a lock (which then resets the state). Tells the listener, after both
    /// locks are released, if the status changed. `None` when the session
    /// moved on.
    fn with_security<R>(
        &self,
        generation: u64,
        change: impl FnOnce(&mut SecurityShared) -> R,
    ) -> Option<R> {
        let (result, changed) = {
            let inner = lock(self);
            if inner.generation != generation || inner.state != StoreState::Protected {
                return None;
            }
            let mut security = guard(&self.security);
            let before = security.status.clone();
            let result = change(&mut security);
            (result, security.status != before)
        };
        if changed && let Some(listener) = self.listener() {
            listener.security_changed();
        }
        Some(result)
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
    let host = Homeserver::parse(client.homeserver().as_str())
        .map(|homeserver| homeserver.display_host())
        .unwrap_or_default();
    let installed = lease.commit(|| {
        *guard(&shared.account) = Some(ActiveAccount {
            client: client.clone(),
            sync: None,
            security: None,
            user_id: user_id.clone(),
            host,
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
        if let Ok(owned_user) = OwnedUserId::try_from(user_id.as_str()) {
            let security_shared = shared.clone();
            let security_client = client.clone();
            let security_runtime = runtime.clone();
            entry.security = std::thread::Builder::new()
                .name("core-security".into())
                .stack_size(SECURITY_STACK_BYTES)
                .spawn(move || {
                    run_security(
                        &security_shared,
                        security_client,
                        owned_user,
                        security_runtime,
                    )
                })
                .ok();
        }
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

/// The security loop: runs the command the prompts leave and keeps the shown
/// state current, one lease per step; a lock is noticed between steps.
fn run_security(
    shared: &Arc<Shared>,
    client: Client,
    user_id: OwnedUserId,
    runtime: tokio::runtime::Handle,
) {
    // What the homeserver last said about the identity, and when to ask again.
    let mut published: Option<bool> = None;
    let mut next_query = Instant::now();
    let mut next_refresh = Instant::now();
    // A recovery-key window can outlive this session; what it reports applies
    // only while the session is still this one.
    let generation = lock(shared).generation;
    loop {
        if !shared.wait_open(SECURITY_TICK) {
            break;
        }
        let command = guard(&shared.security).command.take();
        if command.is_none() && Instant::now() < next_refresh {
            continue;
        }
        let lease = match shared.admit() {
            Ok(lease) => lease,
            Err(AdmissionError::Locked) => break,
            Err(AdmissionError::Saturated) => {
                if command.is_some() {
                    // Dropped (and its secret zeroed) if a lock came first.
                    shared.with_security(generation, |security| security.command = command);
                }
                continue;
            }
        };
        let ran = command.is_some();
        let (outcome, key, identity, recovery, private_keys) = lease.stores().block_on(async {
            if (published.is_none() || published == Some(false)) && Instant::now() >= next_query {
                if let Ok(exists) = security::query_identity(&client, &user_id).await {
                    published = Some(exists);
                }
                next_query = Instant::now() + IDENTITY_REQUERY;
            }
            let (outcome, key) = match command {
                Some(SecurityCommand::SetUp { password, source }) => {
                    match security::set_up(&client, &user_id, &password).await {
                        Ok(key) => {
                            published = Some(true);
                            (None, Some((key, source)))
                        }
                        Err(failure) => {
                            // Where set-up stopped says whether the identity
                            // was uploaded; when it does not, ask.
                            published = match failure.published {
                                Some(known) => Some(known),
                                None => security::query_identity(&client, &user_id)
                                    .await
                                    .ok()
                                    .or(published),
                            };
                            (Some(failure.error), None)
                        }
                    }
                }
                Some(SecurityCommand::Recover(key)) => {
                    (security::recover(&client, &key).await.err(), None)
                }
                Some(SecurityCommand::SetUpRecovery { source }) => {
                    match security::enable_recovery(&client).await {
                        Ok(key) => (None, Some((key, source))),
                        Err(error) => (Some(error), None),
                    }
                }
                None => (None, None),
            };
            (
                outcome,
                key,
                security::identity_state(&client, &user_id, published).await,
                security::recovery_state(&client),
                published == Some(true) && security::holds_private_keys(&client).await,
            )
        });
        let current = lease.commit(|| ()).is_ok();
        drop(lease);
        let published_now = if current {
            next_refresh = Instant::now() + SECURITY_REFRESH;
            shared.with_security(generation, |security| {
                security.can_set_up_recovery = private_keys;
                let status = &mut security.status;
                status.identity = identity;
                status.recovery = recovery;
                status.can_set_up_recovery = private_keys;
                if ran {
                    status.operation = SecurityOperation::Idle;
                    status.error = outcome;
                }
            })
        } else {
            None
        };
        // After the status, so the result above cannot overwrite a window
        // that fails at once; and even after a lock (see the function).
        if let Some((key, source)) = key {
            show_recovery_key(shared, generation, user_id.as_str(), key, source);
        }
        if published_now.is_none() {
            break;
        }
    }
    let _context = runtime.enter();
    drop(client);
}

/// Shows a new recovery key in the credential source's own window. Recovery
/// is already enabled with this key, so it is shown even when a lock or
/// sign-out began meanwhile; only the failure report is dropped then. The
/// window waits for the user, so it has its own thread; the key is zeroed
/// when that thread ends.
fn show_recovery_key(
    shared: &Arc<Shared>,
    generation: u64,
    user_id: &str,
    key: Zeroizing<String>,
    source: Arc<dyn CredentialSource>,
) {
    let display_shared = shared.clone();
    let shown_user = user_id.to_owned();
    let spawned = std::thread::Builder::new()
        .name("core-recovery-key".into())
        .spawn(move || {
            if source.show_recovery_key(&shown_user, &key).is_err() {
                display_shared.with_security(generation, |security| {
                    security.status.error = Some(SecurityError::RecoveryKeyNotShown);
                });
            }
        });
    if spawned.is_err() {
        shared.with_security(generation, |security| {
            security.status.error = Some(SecurityError::RecoveryKeyNotShown);
        });
    }
}

fn security_prompt_error(error: PromptError) -> SecurityError {
    match error {
        PromptError::Cancelled => SecurityError::Cancelled,
        PromptError::Unsupported => SecurityError::Unsupported,
        PromptError::Invalid | PromptError::Failed => SecurityError::Failed,
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
    /// the reason as its state. A saved account is restored by
    /// [`Session::restore_account`], once a listener is set.
    pub fn open(dir: PathBuf, protector: Box<dyn KeyProtector>) -> Self {
        let (state, stores) = match store::open(&dir, &*protector) {
            Ok(stores) => (StoreState::Protected, Some(Arc::new(stores))),
            Err(state) => (state, None),
        };
        let unconfirmed = stores.as_ref().is_some_and(|s| s.unconfirmed_sign_out());
        let session = Self::with_state(Some(dir), protector, state, stores);
        if unconfirmed {
            session.shared.set_status(not_revoked());
        }
        session
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
                security: Mutex::default(),
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
        // A secret left for a command that will not run is zeroed here.
        *guard(&self.shared.security) = SecurityShared::default();
        // No lease remains, so the sync loop is between rounds and will see
        // the lock at its next admission; join it, then drop the client inside
        // the stores' runtime before closing them.
        if let Some(account) = guard(&self.shared.account).take() {
            if let Some(sync) = account.sync {
                let _ = sync.join();
            }
            if let Some(security) = account.security {
                let _ = security.join();
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
        let (began, revoked) = loop {
            match self.with_client() {
                Ok(Some((lease, client))) => {
                    // Before the request: an interrupted sign-out must never
                    // restore this account on the next start.
                    let began = store::begin_sign_out(&dir).is_ok();
                    let revoked = lease
                        .stores()
                        .block_on(client.matrix_auth().logout())
                        .is_ok();
                    break (began, revoked);
                }
                Ok(None) | Err(AdmissionError::Locked) => return Ok(self.state()),
                Err(AdmissionError::Saturated) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(error),
            }
        };
        let marked = store::mark_signed_out(&dir, revoked).is_ok() && began;
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

    /// The account's encryption state; `unknown` without a signed-in account.
    pub fn security_status(&self) -> SecurityStatus {
        guard(&self.shared.security).status.clone()
    }

    /// Starts setting up encryption for an account without an identity: the
    /// core asks for the password in its own window, then the security thread
    /// sets up cross-signing and recovery and shows the recovery key. Does
    /// nothing unless the account is known to have no identity and nothing
    /// else is running. Returns the status at once.
    pub fn set_up_encryption(&self, source: Arc<dyn CredentialSource>) -> SecurityStatus {
        self.start_secret_operation(
            IdentityState::None,
            SecurityOperation::WaitingForPassword,
            source,
            |prompt_source, user_id, host| {
                prompt_source
                    .ask_secret(SecretPrompt::Password { user_id, host })
                    .map(|password| SecurityCommand::SetUp {
                        password,
                        source: prompt_source.clone(),
                    })
            },
        )
    }

    /// Starts verifying this device with the recovery key, asked in the
    /// core's own window. Does nothing unless the account has an identity this
    /// device is not verified with and nothing else is running.
    pub fn recover_with_key(&self, source: Arc<dyn CredentialSource>) -> SecurityStatus {
        self.start_secret_operation(
            IdentityState::Unverified,
            SecurityOperation::WaitingForRecoveryKey,
            source,
            |prompt_source, user_id, _host| {
                prompt_source
                    .ask_secret(SecretPrompt::RecoveryKey { user_id })
                    .map(SecurityCommand::Recover)
            },
        )
    }

    /// Starts setting up recovery alone, for a published identity whose
    /// private keys this device holds and whose recovery is not set up (an
    /// earlier attempt failed). Needs no password; the new recovery key is
    /// shown in the core's own window.
    pub fn set_up_recovery(&self, source: Arc<dyn CredentialSource>) -> SecurityStatus {
        let Some(generation) = self.shared.open_generation() else {
            return self.security_status();
        };
        if guard(&self.shared.account).is_none() {
            return self.security_status();
        }
        self.shared.with_security(generation, |security| {
            if security.can_set_up_recovery
                && security.status.recovery == RecoveryState::Disabled
                && security.status.operation == SecurityOperation::Idle
                && security.command.is_none()
            {
                security.command = Some(SecurityCommand::SetUpRecovery { source });
                security.status.operation = SecurityOperation::Working;
                security.status.error = None;
            }
        });
        self.security_status()
    }

    /// Asks for a secret on a prompt thread without a lease, then leaves the
    /// command for the security thread, unless the session was locked (and
    /// perhaps unlocked) meanwhile.
    fn start_secret_operation(
        &self,
        required: IdentityState,
        waiting: SecurityOperation,
        source: Arc<dyn CredentialSource>,
        ask: impl FnOnce(&Arc<dyn CredentialSource>, &str, &str) -> Result<SecurityCommand, PromptError>
        + Send
        + 'static,
    ) -> SecurityStatus {
        let (user_id, host) = match guard(&self.shared.account).as_ref() {
            Some(account) => (account.user_id.clone(), account.host.clone()),
            None => return self.security_status(),
        };
        let Some(started) = self.shared.open_generation() else {
            return self.security_status();
        };
        let accepted = self.shared.with_security(started, |security| {
            let free = security.status.identity == required
                && security.status.operation == SecurityOperation::Idle
                && security.command.is_none();
            if free {
                security.status.operation = waiting;
                security.status.error = None;
            }
            free
        });
        if accepted != Some(true) {
            return self.security_status();
        }
        let shared = self.shared.clone();
        let spawned = std::thread::Builder::new()
            .name("core-secret-prompt".into())
            .spawn(move || {
                // After a lock (and perhaps an unlock) nothing is written: a
                // secret is dropped and zeroed here, and the lock reset the
                // status.
                match ask(&source, &user_id, &host) {
                    Ok(command) => {
                        shared.with_security(started, |security| {
                            security.command = Some(command);
                            security.status.operation = SecurityOperation::Working;
                        });
                    }
                    Err(error) => {
                        shared.with_security(started, |security| {
                            security.status.operation = SecurityOperation::Idle;
                            security.status.error = Some(security_prompt_error(error));
                        });
                    }
                }
            });
        if spawned.is_err() {
            self.shared.with_security(started, |security| {
                security.status.operation = SecurityOperation::Idle;
                security.status.error = Some(SecurityError::Failed);
            });
        }
        self.security_status()
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
            if stores.as_ref().is_some_and(|s| s.unconfirmed_sign_out()) {
                self.shared.set_status(not_revoked());
            }
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

/// The account after a sign-out whose revocation was not confirmed.
fn not_revoked() -> AccountStatus {
    AccountStatus {
        state: AccountState::SignedOut,
        user_id: None,
        error: Some(SignInError::SessionNotRevoked),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::{KEY_BYTES, StoreKey, VaultError};

    /// Keeps the key as it is: enough for a profile in a temporary folder.
    struct Plain;

    impl KeyProtector for Plain {
        fn protect(&self, key: &StoreKey) -> Result<Vec<u8>, VaultError> {
            Ok(key.to_vec())
        }

        fn unprotect(&self, blob: &[u8]) -> Result<StoreKey, VaultError> {
            let bytes: [u8; KEY_BYTES] = blob.try_into().map_err(|_| VaultError::Failed)?;
            Ok(Zeroizing::new(bytes))
        }
    }

    /// GIVEN a write meant for the session before a lock WHEN it arrives
    /// after the lock, or after a later unlock THEN it is refused: no stale
    /// status or command reaches the locked or the new session.
    #[test]
    fn given_a_lock_then_writes_for_the_old_session_are_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let session = Session::open(dir.path().into(), Box::new(Plain));
        let old = session.shared.open_generation().expect("open");
        assert!(
            session
                .shared
                .with_security(old, |security| security.status.identity =
                    IdentityState::None)
                .is_some()
        );
        assert_eq!(
            session.lock(Duration::from_secs(5), || {}).0,
            StoreState::Locked
        );
        let late = session.shared.with_security(old, |security| {
            security.status.operation = SecurityOperation::Working;
            security.command = Some(SecurityCommand::Recover(Zeroizing::new("key".into())));
        });
        assert_eq!(late, None);
        assert_eq!(session.security_status(), SecurityStatus::unknown());
        assert!(guard(&session.shared.security).command.is_none());

        assert_eq!(session.unlock(), StoreState::Protected);
        let new = session.shared.open_generation().expect("open again");
        assert_ne!(new, old);
        assert_eq!(
            session.shared.with_security(old, |_| ()),
            None,
            "still the old session"
        );
        assert!(session.shared.with_security(new, |_| ()).is_some());
    }
}
