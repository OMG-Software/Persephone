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
//! 4. Join the sync, security and sending threads, drop the SDK client,
//!    close the stores, then release the profile. A failed close leaves
//!    `lock_failed`. Unsent messages are dropped (`outbox.rs`).
//!
//! The sync loop and sign-in take one lease per unit of work and check for a
//! lock only between complete SDK operations. The SDK's own background tasks
//! are not leases: once a client has run, a completed wait does not prove they
//! stopped, so the session ends `restart_required` and will not reopen the
//! stores in this process. [`TrackedDrain::require_full_sdk_drain`] always
//! refuses until the SDK provides a stop-and-join contract.
//!
//! Encryption setup (`security.rs`) runs on its own thread, one lease per
//! step, like sync. So do changing the recovery key (ADR-0021) and
//! verifying another of the account's devices. Its prompts run on
//! short-lived threads without a lease, so a prompt left open never holds up
//! a lock; an answer given after a lock (or a lock and unlock) is dropped.
//!
//! Unlocking and resetting the profile ask for the passphrase (ADR-0006) in
//! the core's own window. They run on a worker thread in the `unlocking`
//! state, which refuses every other transition, so the serving loop is never
//! blocked by an open prompt and nothing else can change the stores meanwhile.

use crate::account;
use crate::activity;
use crate::contacts;
use crate::credentials::{CredentialSource, NewPassword, PromptError, SecretPrompt};
use crate::homeserver::Homeserver;
use crate::identities;
use crate::media::ImageUpload;
use crate::notifications;
use crate::outbox::{Content, Outbox};
use crate::passphrase::Unlock;
use crate::recovery_file;
use crate::registration::{self, Registered};
use crate::security;
use crate::sending;
use crate::settings;
use crate::store::{self, Stores};
use crate::vault::KeyProtector;
use crate::verification::{self, Action as VerifyAction, Flow};
use matrix_sdk::{
    Client, Room,
    ruma::{
        OwnedUserId,
        events::{
            GlobalAccountDataEvent,
            direct::DirectEventContent,
            key::verification::request::ToDeviceKeyVerificationRequestEvent,
            room::message::{MessageType, OriginalSyncRoomMessageEvent},
            typing::SyncTypingEvent,
        },
    },
};
use persephone_contracts::{
    AccountSettings, AccountState, AccountStatus, IdentityState, PendingMessage, RecoveryState,
    SecurityError, SecurityOperation, SecurityStatus, SignInError, StoreState, VerificationPhase,
};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, AtomicU16, Ordering},
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
/// How often the sending thread checks for queued messages.
const SEND_TICK: Duration = Duration::from_millis(100);
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
    /// A message the user sent in this room changed state.
    fn outbox_changed(&self, _room_id: &str) {}
    /// Room keys arrived for these rooms, so messages there that could not
    /// be decrypted may be readable now.
    fn room_keys_arrived(&self, _rooms: &account::KeyRooms) {}
    /// The account's direct-message rooms changed (`m.direct`), so the room
    /// list names different contacts. Account data touches no room, so a
    /// sync round that brings only this reports no rooms.
    fn direct_rooms_changed(&self) {}
    /// A message to show as a desktop notification arrived (ADR-0022): the
    /// room's and sender's projected names, never its text. Called only while
    /// desktop notifications are on.
    fn notify(&self, _room: &str, _sender: &str) {}
}

/// Why a message was not queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendRefusal {
    /// No signed-in account, or the session is not open.
    Unavailable,
    /// Too many messages are waiting.
    Full,
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
    sender: Option<JoinHandle<()>>,
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
    /// Start, accept, confirm or cancel a verification with another device.
    Verify(VerifyAction),
    /// Accept a room member's new identity; the user said yes in the core's
    /// own window (ADR-0007).
    AcceptIdentity {
        room_id: String,
        user_id: OwnedUserId,
        was_verified: bool,
    },
    /// Replace the recovery key; the user said yes in the core's own window
    /// (ADR-0021).
    ResetRecoveryKey {
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
    /// The account's desktop-notification setting, kept here so the sync's
    /// notification handler need not read the store (ADR-0022).
    toasts: AtomicBool,
    /// Whether those notifications name the sender and room.
    toast_names: AtomicBool,
    /// The account's settings for typing notices and public read receipts,
    /// kept here so sending them need not read the store (ADR-0023).
    send_typing: AtomicBool,
    send_receipts: AtomicBool,
    /// The account's idle lock, in minutes (0: off; ADR-0025).
    idle_lock_minutes: AtomicU16,
    /// Who is typing in each room, from the sync.
    typing: Arc<activity::Typing>,
    security: Mutex<SecurityShared>,
    outbox: Mutex<Outbox>,
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
    /// False only for [`Session::not_configured`], the synthetic views.
    configured: bool,
    protector: Arc<dyn KeyProtector>,
    unlock: Unlock,
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

    /// The session generation this lease was granted in. Results kept beyond
    /// the lease (such as image handles) are bound to it.
    pub fn generation(&self) -> u64 {
        self.generation
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

fn prompt_error(error: PromptError) -> SignInError {
    match error {
        PromptError::Cancelled => SignInError::Cancelled,
        PromptError::Unsupported => SignInError::Unsupported,
        PromptError::Invalid | PromptError::Failed => SignInError::Failed,
    }
}

/// Asks for a new account password twice, until both entries match.
fn new_password(
    source: &dyn CredentialSource,
    username: &str,
    host: &str,
) -> Result<Zeroizing<String>, SignInError> {
    let ask = |step| {
        source
            .ask_secret(SecretPrompt::NewAccountPassword {
                username,
                host,
                step,
            })
            .map_err(prompt_error)
    };
    let mut step = NewPassword::Choose;
    loop {
        let first = ask(step)?;
        let second = ask(NewPassword::Confirm)?;
        if first == second {
            return Ok(first);
        }
        step = NewPassword::Mismatch;
    }
}

/// One registration request's result, inside the stores' runtime.
enum Step {
    Done(Client, String),
    Token(Option<String>),
}

/// The steps of [`Session::register`] on its worker thread. `Err(None)`
/// means a lock abandoned the attempt. A client that does not become the
/// account is dropped inside the stores' runtime.
fn register_account(
    shared: &Arc<Shared>,
    started: u64,
    homeserver: &Homeserver,
    username: &str,
    source: &dyn CredentialSource,
) -> Result<(Lease, Client, String), Option<SignInError>> {
    let admit = || match shared.admit() {
        Ok(lease) if lease.generation == started => Ok(lease),
        _ => Err(None),
    };
    let host = homeserver.display_host();
    // The name first, so a taken one is reported before any prompt.
    let lease = admit()?;
    shared.client_ran.store(true, Ordering::SeqCst);
    lease.stores().block_on(async {
        let client = account::build_client(homeserver, lease.stores()).await?;
        let checked = registration::check_username(&client, username).await;
        drop(client);
        checked
    })?;
    lease.commit(|| ()).map_err(|_| None)?;
    drop(lease);
    let password = new_password(source, username, &host)?;
    let mut token: Option<(Zeroizing<String>, Option<String>)> = None;
    loop {
        let lease = admit()?;
        let result = lease.stores().block_on(async {
            let client = account::build_client(homeserver, lease.stores()).await?;
            let sent = token
                .as_ref()
                .map(|(token, session)| (token.as_str(), session.clone()));
            match registration::register(
                &client,
                homeserver,
                lease.stores(),
                username,
                &password,
                sent,
            )
            .await
            {
                Ok(Registered::Done(user_id)) => Ok(Step::Done(client, user_id)),
                Ok(Registered::NeedsToken(session)) => {
                    drop(client);
                    Ok(Step::Token(session))
                }
                Err(error) => {
                    drop(client);
                    Err(error)
                }
            }
        });
        match result {
            Ok(Step::Done(client, user_id)) => return Ok((lease, client, user_id)),
            // Asked again after a token was sent: it was not accepted.
            Ok(Step::Token(_)) if token.is_some() => return Err(Some(SignInError::TokenRejected)),
            Ok(Step::Token(session)) => {
                lease.commit(|| ()).map_err(|_| None)?;
                drop(lease);
                let value = source
                    .ask_secret(SecretPrompt::RegistrationToken { host: &host })
                    .map_err(|error| Some(prompt_error(error)))?;
                token = Some((value, session));
            }
            Err(error) => return Err(Some(error)),
        }
    }
}

/// Starts the account: records it, then runs its sync loop on a thread.
/// Refused (and the client dropped) if a lock began since `lease`.
fn install(shared: &Arc<Shared>, lease: Lease, client: Client, user_id: String) {
    let host = Homeserver::parse(client.homeserver().as_str())
        .map(|homeserver| homeserver.display_host())
        .unwrap_or_default();
    let loaded = lease
        .stores()
        .block_on(settings::load(lease.stores()))
        .unwrap_or_default();
    shared
        .toasts
        .store(loaded.desktop_notifications, Ordering::SeqCst);
    shared
        .toast_names
        .store(loaded.notification_names, Ordering::SeqCst);
    shared
        .send_typing
        .store(loaded.send_typing, Ordering::SeqCst);
    shared
        .send_receipts
        .store(loaded.send_read_receipts, Ordering::SeqCst);
    shared
        .idle_lock_minutes
        .store(loaded.idle_lock_minutes, Ordering::SeqCst);
    let installed = lease.commit(|| {
        *guard(&shared.account) = Some(ActiveAccount {
            client: client.clone(),
            sync: None,
            security: None,
            sender: None,
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
        let sender_shared = shared.clone();
        let sender_client = client.clone();
        let sender_runtime = runtime.clone();
        entry.sender = std::thread::Builder::new()
            .name("core-send".into())
            .stack_size(SECURITY_STACK_BYTES)
            .spawn(move || run_sender(&sender_shared, sender_client, sender_runtime))
            .ok();
        let thread_shared = shared.clone();
        let thread_user = user_id.clone();
        let thread_runtime = runtime.clone();
        entry.sync = std::thread::Builder::new()
            .name("core-sync".into())
            // Sync processes verification messages, whose SDK futures need
            // more than the default stack in debug builds.
            .stack_size(SECURITY_STACK_BYTES)
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
    // The SDK's room-key stream, taken once per round; a receiver, not a task.
    let mut keys = None;
    // The SDK's stream of changed user identities, taken once per round; a
    // receiver, not a task. Any update may change a room's paused state.
    let mut identities = None;
    // Set by the SDK while it processes a round that changed `m.direct`.
    let direct = Arc::new(AtomicBool::new(false));
    let noted = direct.clone();
    let direct_handler = {
        let _context = runtime.enter();
        client.add_event_handler(move |_: GlobalAccountDataEvent<DirectEventContent>| {
            noted.store(true, Ordering::SeqCst);
            std::future::ready(())
        })
    };
    // Typing notices are ephemeral: kept as they arrive, for the page to ask.
    let typing = shared.typing.clone();
    let typing_handler = {
        let _context = runtime.enter();
        client.add_event_handler(move |event: SyncTypingEvent, room: matrix_sdk::Room| {
            typing.set(
                room.room_id().as_str(),
                event.content.user_ids.iter().map(|user| &**user),
            );
            std::future::ready(())
        })
    };
    // The SDK keeps notification handlers as long as the client, so this one
    // holds the session only weakly.
    let mut notifying = false;
    let notified: Weak<Shared> = Arc::downgrade(shared);
    // The access token last saved, so a refreshed OAuth token is saved once
    // (ADR-0030); `None` until the first round saves or confirms it.
    let mut saved_token: Option<Zeroizing<String>> = None;
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
            if !notifying {
                notifying = true;
                let notified = notified.clone();
                client
                    .register_notification_handler(move |notification, room, client| {
                        let notified = notified.clone();
                        async move {
                            let Some(shared) = notified.upgrade() else {
                                return;
                            };
                            if !shared.toasts.load(Ordering::SeqCst) {
                                return;
                            }
                            if let Some((room, sender)) =
                                notifications::shown(&notification, &room, &client).await
                                && let Some(listener) = shared.listener()
                            {
                                // Without names, none leaves the core.
                                if shared.toast_names.load(Ordering::SeqCst) {
                                    listener.notify(&room, &sender);
                                } else {
                                    listener.notify("", "");
                                }
                            }
                        }
                    })
                    .await;
            }
            if keys.is_none() {
                keys = client
                    .encryption()
                    .room_keys_received_stream()
                    .await
                    .map(Box::pin);
            }
            if engine.is_none() {
                engine = Some(account::select_engine(&client).await?);
            }
            if identities.is_none() {
                identities = client
                    .encryption()
                    .user_identities_stream()
                    .await
                    .ok()
                    .map(Box::pin);
            }
            let rooms =
                account::sync_round(&client, engine.as_ref().expect("selected above")).await;
            // Saved even when the round failed: the SDK may have refreshed the
            // token, spending the old refresh token, before the error.
            saved_token =
                account::save_if_refreshed(&client, lease.stores(), saved_token.take()).await;
            let rooms = rooms?;
            let keyed = match keys.as_mut() {
                Some(stream) => Some(account::take_key_rooms(stream.as_mut()).await),
                None => None,
            };
            let changed = match identities.as_mut() {
                Some(stream) => account::take_any(stream.as_mut()).await,
                None => false,
            };
            Ok::<_, SignInError>((rooms, keyed, changed))
        });
        let committed = lease.commit(|| ()).is_ok();
        drop(lease);
        if !committed {
            break;
        }
        match outcome {
            Ok((rooms, keyed, identities_changed)) => {
                backoff = Duration::from_secs(1);
                if guard(&shared.status).error.is_some() {
                    shared.set_status(signed_in(&user_id, None));
                }
                if let Some(listener) = shared.listener() {
                    if !rooms.is_empty() {
                        listener.rooms_changed(&rooms);
                    }
                    if direct.swap(false, Ordering::SeqCst) {
                        listener.direct_rooms_changed();
                    }
                    if identities_changed {
                        listener.security_changed();
                    }
                    if let Some(keyed) = keyed.filter(|keyed| !keyed.is_empty()) {
                        listener.room_keys_arrived(&keyed);
                    }
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
    client.remove_event_handler(direct_handler);
    client.remove_event_handler(typing_handler);
    drop(identities);
    drop(keys);
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
    // At most one verification at a time. Requests are noted (sender and
    // flow ID) as they arrive during sync: from the account's own other
    // devices, and from the contact of a direct-message room in that room.
    let mut flow: Option<Flow> = None;
    let incoming: Arc<Mutex<Option<(OwnedUserId, String)>>> = Arc::default();
    let recorder = incoming.clone();
    let own = user_id.clone();
    let handler = client.add_event_handler(move |event: ToDeviceKeyVerificationRequestEvent| {
        if event.sender == own
            && event.content.transaction_id.as_str().len() <= verification::MAX_FLOW_ID_BYTES
        {
            *guard(&recorder) = Some((own.clone(), event.content.transaction_id.to_string()));
        }
        std::future::ready(())
    });
    let recorder = incoming.clone();
    let own = user_id.clone();
    let room_handler = client.add_event_handler(
        move |event: OriginalSyncRoomMessageEvent, room: Room, client: Client| {
            if let MessageType::VerificationRequest(request) = &event.content.msgtype
                && request.to == own
                && event.sender != own
                && event.event_id.as_str().len() <= verification::MAX_FLOW_ID_BYTES
                && contacts::contact_of(&client, &room).as_ref() == Some(&event.sender)
            {
                *guard(&recorder) = Some((event.sender.clone(), event.event_id.to_string()));
            }
            std::future::ready(())
        },
    );
    loop {
        if !shared.wait_open(SECURITY_TICK) {
            break;
        }
        let command = guard(&shared.security).command.take();
        let active = flow.as_ref().is_some_and(|flow| !flow.is_over());
        let waiting = guard(&incoming).is_some();
        if command.is_none() && !active && !waiting && Instant::now() < next_refresh {
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
        let (outcome, key, identity, recovery, private_keys, view) =
            lease.stores().block_on(async {
                // A new request from another device replaces a finished flow.
                if flow.as_ref().is_none_or(Flow::is_over) {
                    let noted = guard(&incoming).take();
                    if let Some((sender, flow_id)) = noted
                        && let Some(new) = verification::incoming(&client, &sender, &flow_id).await
                    {
                        flow = Some(new);
                    }
                }
                if (published.is_none() || published == Some(false)) && Instant::now() >= next_query
                {
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
                    // A request that came in after the start was queued is
                    // kept: replacing it would leave the other device waiting.
                    Some(SecurityCommand::Verify(
                        VerifyAction::Start
                        | VerifyAction::StartContact(_)
                        | VerifyAction::StartDevice(_),
                    )) if flow.as_ref().is_some_and(|flow| !flow.is_over()) => (None, None),
                    Some(SecurityCommand::Verify(VerifyAction::StartDevice(device_id))) => {
                        match verification::start_device(&client, &user_id, &device_id).await {
                            Ok(new) => {
                                flow = Some(new);
                                (None, None)
                            }
                            Err(error) => (Some(error), None),
                        }
                    }
                    Some(SecurityCommand::ResetRecoveryKey { source }) => {
                        match security::reset_recovery_key(&client).await {
                            Ok(key) => (None, Some((key, source))),
                            Err(error) => (Some(error), None),
                        }
                    }
                    Some(SecurityCommand::Verify(VerifyAction::StartContact(room_id))) => {
                        match verification::start_contact(&client, &room_id).await {
                            Ok(new) => {
                                flow = Some(new);
                                (None, None)
                            }
                            Err(error) => (Some(error), None),
                        }
                    }
                    Some(SecurityCommand::Verify(VerifyAction::Start)) => {
                        match verification::start(&client, &user_id).await {
                            Some(new) => {
                                flow = Some(new);
                                (None, None)
                            }
                            None => (Some(SecurityError::Failed), None),
                        }
                    }
                    Some(SecurityCommand::AcceptIdentity {
                        room_id,
                        user_id,
                        was_verified,
                    }) => (
                        identities::accept(&client, &room_id, &user_id, was_verified)
                            .await
                            .err(),
                        None,
                    ),
                    Some(SecurityCommand::Verify(action)) => {
                        let outcome = match flow.as_mut() {
                            Some(flow) => flow.act(&client, &action).await,
                            None => Err(SecurityError::Failed),
                        };
                        (outcome.err(), None)
                    }
                    None => (None, None),
                };
                if let Some(flow) = flow.as_mut() {
                    flow.drive().await;
                    flow.refresh_contact(&client).await;
                }
                (
                    outcome,
                    key,
                    security::identity_state(&client, &user_id, published).await,
                    security::recovery_state(&client),
                    published == Some(true) && security::holds_private_keys(&client).await,
                    flow.as_ref().map(Flow::view),
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
                status.verification = view;
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
    client.remove_event_handler(handler);
    client.remove_event_handler(room_handler);
    drop(flow);
    drop(client);
}

/// The sending loop: one message at a time in queue order, one lease per
/// send; a lock is noticed between sends.
fn run_sender(shared: &Arc<Shared>, client: Client, runtime: tokio::runtime::Handle) {
    loop {
        if !shared.wait_open(SEND_TICK) {
            break;
        }
        // A message is marked as sending only once a lease is held, so one
        // that must wait for a lease stays queued, as the page shows it.
        if !guard(&shared.outbox).has_job() {
            continue;
        }
        let lease = match shared.admit() {
            Ok(lease) => lease,
            Err(AdmissionError::Locked) => break,
            Err(AdmissionError::Saturated) => continue,
        };
        // Discarded meanwhile: the lease is dropped unused.
        let Some(job) = guard(&shared.outbox).next_job() else {
            continue;
        };
        if let Some(listener) = shared.listener() {
            listener.outbox_changed(&job.room_id);
        }
        let outcome = lease.stores().block_on(async {
            let consented = sending::consented(lease.stores(), &job.room_id).await;
            match &job.content {
                Content::Text(body, relation) => {
                    sending::send(
                        &client,
                        &job.room_id,
                        body,
                        relation.as_ref(),
                        job.transaction_id.clone(),
                        consented,
                    )
                    .await
                }
                Content::File(file) => {
                    sending::send_file(
                        &client,
                        &job.room_id,
                        file,
                        job.transaction_id.clone(),
                        consented,
                    )
                    .await
                }
                Content::Image(image) => {
                    sending::send_image(
                        &client,
                        &job.room_id,
                        image,
                        job.transaction_id.clone(),
                        consented,
                    )
                    .await
                }
            }
        });
        let current = lease.commit(|| ()).is_ok();
        drop(lease);
        if !current {
            break;
        }
        let room = guard(&shared.outbox).finish(job.local_id, outcome);
        if let (Some(room), Some(listener)) = (room, shared.listener()) {
            listener.outbox_changed(&room);
        }
    }
    let _context = runtime.enter();
    drop(client);
}

/// Shows a new recovery key in the credential source's own window, then
/// offers to save it to a file chosen in the core's own Save dialog
/// (ADR-0021). Recovery is already enabled with this key, so it is shown
/// even when a lock or sign-out began meanwhile; only the failure report is
/// dropped then. The windows wait for the user, so they have their own
/// thread; the key is zeroed when that thread ends.
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
                return;
            }
            let saved = match source.choose_recovery_key_file() {
                Ok(Some(path)) => recovery_file::write(&path, &key).is_ok(),
                // Not saving it is the user's choice; a platform without the
                // dialog has shown the key, which is enough.
                Ok(None) | Err(PromptError::Cancelled | PromptError::Unsupported) => true,
                Err(PromptError::Invalid | PromptError::Failed) => false,
            };
            if !saved {
                display_shared.with_security(generation, |security| {
                    security.status.error = Some(SecurityError::RecoveryKeyNotSaved);
                });
            }
        });
    if spawned.is_err() {
        shared.with_security(generation, |security| {
            security.status.error = Some(SecurityError::RecoveryKeyNotShown);
        });
    }
}

/// Leaves a verification action for the security thread if the current flow
/// and phase allow it. The operation is `Working` until the thread has run
/// it, so a second action cannot be queued while the first is with the SDK.
/// Returns whether it was queued.
fn queue_verification(security: &mut SecurityShared, action: VerifyAction) -> bool {
    let flow = security.status.verification.as_ref();
    let over = flow.is_none_or(|flow| {
        matches!(
            flow.phase,
            VerificationPhase::Done | VerificationPhase::Cancelled
        )
    });
    let allowed = match &action {
        VerifyAction::Start => security.status.identity == IdentityState::Unverified && over,
        // Only a verified device vouches for someone else, or for another
        // of the account's devices.
        VerifyAction::StartContact(_) | VerifyAction::StartDevice(_) => {
            security.status.identity == IdentityState::Verified && over
        }
        VerifyAction::Accept(id) => flow
            .is_some_and(|flow| &flow.flow_id == id && flow.phase == VerificationPhase::Incoming),
        // A match is confirmed only against the seven emoji shown.
        VerifyAction::Confirm(id) => flow.is_some_and(|flow| {
            &flow.flow_id == id
                && flow.phase == VerificationPhase::Compare
                && flow.emoji.len() == verification::SAS_EMOJI
        }),
        VerifyAction::Cancel(id) => flow.is_some_and(|flow| &flow.flow_id == id) && !over,
    };
    let queued = allowed
        && security.status.operation == SecurityOperation::Idle
        && security.command.is_none();
    if queued {
        security.command = Some(SecurityCommand::Verify(action));
        security.status.operation = SecurityOperation::Working;
        security.status.error = None;
    }
    queued
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
            Arc::new(crate::vault::Unsupported),
            Unlock::new(Arc::new(crate::credentials::Unsupported)),
            StoreState::NotConfigured,
            None,
        )
    }

    /// A session whose profile directory could not be found. It stays
    /// `store_unavailable`: nothing can unlock, and the views stay locked
    /// instead of showing synthetic data.
    pub fn unavailable() -> Self {
        Self::with_state(
            None,
            Arc::new(crate::vault::Unsupported),
            Unlock::new(Arc::new(crate::credentials::Unsupported)),
            StoreState::StoreUnavailable,
            None,
        )
    }

    /// Opens the profile in `dir`, asking for its passphrase (or a new one)
    /// through `unlock` before returning; on failure the session starts locked
    /// with the reason as its state. A saved account is restored by
    /// [`Session::restore_account`], once a listener is set.
    pub fn open(dir: PathBuf, protector: Arc<dyn KeyProtector>, unlock: Unlock) -> Self {
        let (state, stores) = match store::open(&dir, &*protector, &unlock) {
            Ok(stores) => (StoreState::Protected, Some(Arc::new(stores))),
            Err(state) => (state, None),
        };
        let unconfirmed = stores.as_ref().is_some_and(|s| s.unconfirmed_sign_out());
        let session = Self::with_state(Some(dir), protector, unlock, state, stores);
        if unconfirmed {
            session.shared.set_status(not_revoked());
        }
        session
    }

    fn with_state(
        profile: Option<PathBuf>,
        protector: Arc<dyn KeyProtector>,
        unlock: Unlock,
        state: StoreState,
        stores: Option<Arc<Stores>>,
    ) -> Self {
        Self {
            profile,
            configured: state != StoreState::NotConfigured,
            protector,
            unlock,
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
                outbox: Mutex::default(),
                listener: Mutex::new(None),
                client_ran: AtomicBool::new(false),
                toasts: AtomicBool::new(false),
                toast_names: AtomicBool::new(true),
                send_typing: AtomicBool::new(false),
                send_receipts: AtomicBool::new(false),
                idle_lock_minutes: AtomicU16::new(0),
                typing: Arc::default(),
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

    /// False only for [`Session::not_configured`]: the core then serves only
    /// synthetic data and there is nothing to protect.
    pub fn is_configured(&self) -> bool {
        self.configured
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
        restore(&self.shared);
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

    /// Starts signing in to `homeserver` through its OAuth 2.0 provider
    /// (ADR-0030) on a worker thread and returns the status at once, like
    /// [`Session::sign_in`]. One lease covers starting the login (the
    /// provider's page); the question in the core's own window runs with no
    /// lease held, as the password prompt does; a second lease, in the same
    /// generation, covers the redirect wait and the install. A lock in
    /// between abandons the attempt, and abandons the wait within half a
    /// second.
    pub fn sign_in_oidc(
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
        let started = lock(&self.shared).generation;
        let spawned = std::thread::Builder::new()
            .name("core-sign-in-oidc".into())
            .spawn(move || {
                let still_current = || {
                    let inner = lock(&shared);
                    inner.generation == started && inner.state == StoreState::Protected
                };
                let admitted = || match shared.admit() {
                    Ok(lease) if lease.generation == started => Some(lease),
                    _ => None,
                };
                // Reports the end of an attempt that did not sign in.
                let end = |lease: Option<Lease>, error: SignInError| {
                    let current = lease.is_some_and(|lease| lease.commit(|| ()).is_ok());
                    shared.set_status(if current {
                        failed(error)
                    } else {
                        AccountStatus::signed_out()
                    });
                };
                // First lease: the client and the provider's page.
                let Some(lease) = admitted() else {
                    shared.set_status(AccountStatus::signed_out());
                    return;
                };
                shared.client_ran.store(true, Ordering::SeqCst);
                let runtime = lease.stores().runtime().handle().clone();
                let begun = lease.stores().block_on(async {
                    let client = account::build_client(&homeserver, lease.stores()).await?;
                    match account::start_oauth(&client).await {
                        Ok(start) => Ok((client, start)),
                        Err(error) => {
                            drop(client);
                            Err(error)
                        }
                    }
                });
                let (client, start) = match begun {
                    Ok(begun) => begun,
                    Err(error) => {
                        end(Some(lease), error);
                        return;
                    }
                };
                drop(lease);
                // Dropped inside its runtime, as everywhere else.
                let discard = |client: Client| {
                    let _context = runtime.enter();
                    drop(client);
                };
                // The question, with no lease held: it may stay open long.
                let answer =
                    match crate::oauth_login::question(&homeserver.display_host(), start.url()) {
                        None => Err(SignInError::Failed),
                        Some((caption, message)) => {
                            match source.confirm_open_link(caption, &message) {
                                Ok(true) => source
                                    .open_link(start.url().as_str())
                                    .map(|()| true)
                                    .map_err(|_| SignInError::Failed),
                                Ok(false) | Err(PromptError::Cancelled) => Ok(false),
                                Err(PromptError::Unsupported) => Err(SignInError::Unsupported),
                                Err(_) => Err(SignInError::Failed),
                            }
                        }
                    };
                // Second lease, in the same generation: a lock in between
                // abandons the attempt.
                let Some(lease) = admitted() else {
                    discard(client);
                    shared.set_status(AccountStatus::signed_out());
                    return;
                };
                match answer {
                    Ok(true) => {}
                    Ok(false) | Err(_) => {
                        lease
                            .stores()
                            .block_on(account::abort_oauth(&client, start));
                        discard(client);
                        end(Some(lease), answer.err().unwrap_or(SignInError::Cancelled));
                        return;
                    }
                }
                let result = lease.stores().block_on(async {
                    match account::finish_oauth(&client, &homeserver, lease.stores(), start, || {
                        !still_current()
                    })
                    .await
                    {
                        Ok(user_id) => Ok((client, user_id)),
                        Err(error) => {
                            drop(client);
                            Err(error)
                        }
                    }
                });
                match result {
                    Ok((client, user_id)) => install(&shared, lease, client, user_id),
                    Err(error) => {
                        let current = lease.commit(|| ()).is_ok();
                        drop(lease);
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

    /// Starts registering `username` on `homeserver` on a worker thread and
    /// returns the status at once, like [`Session::sign_in`]. The name is
    /// checked first; the password and any registration token are asked for
    /// in the core's own windows, without a lease; each homeserver step holds
    /// one. A lock in between abandons the attempt. `username` must already
    /// be valid (`registration::valid_username`).
    pub fn register(
        &self,
        homeserver: Homeserver,
        username: String,
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
        let started = lock(&self.shared).generation;
        let spawned = std::thread::Builder::new()
            .name("core-register".into())
            .spawn(move || {
                let outcome = register_account(&shared, started, &homeserver, &username, &*source);
                match outcome {
                    Ok((lease, client, user_id)) => install(&shared, lease, client, user_id),
                    // Abandoned by a lock: nothing to report.
                    Err(None) => shared.set_status(AccountStatus::signed_out()),
                    Err(Some(error)) => {
                        let current = {
                            let inner = lock(&shared);
                            inner.generation == started && inner.state == StoreState::Protected
                        };
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
        // A secret left for a command that will not run is zeroed here, and
        // unsent messages are dropped.
        *guard(&self.shared.security) = SecurityShared::default();
        guard(&self.shared.outbox).clear();
        self.shared.typing.clear();
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
            if let Some(sender) = account.sender {
                let _ = sender.join();
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
                    let revoked = lease.stores().block_on(client.logout()).is_ok();
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

    /// Queues a message for the signed-in account. The room and body were
    /// checked by the caller; whether the account is in the room and whether
    /// the room needs consent are decided when it is sent.
    pub fn send_message(
        &self,
        room_id: &str,
        body: Zeroizing<String>,
        relation: Option<crate::outbox::Relation>,
    ) -> Result<PendingMessage, SendRefusal> {
        if guard(&self.shared.account).is_none() {
            return Err(SendRefusal::Unavailable);
        }
        let queued = {
            // Held while queueing: a lock changes the state under it before it
            // clears the outbox, so nothing queued here outlives a lock.
            let inner = lock(&self.shared);
            if inner.state != StoreState::Protected {
                return Err(SendRefusal::Unavailable);
            }
            guard(&self.shared.outbox).queue(room_id, body, relation)
        }
        .ok_or(SendRefusal::Full)?;
        if let Some(listener) = self.shared.listener() {
            listener.outbox_changed(room_id);
        }
        Ok(queued)
    }

    /// Queues a picked file for the signed-in account, like
    /// [`Session::send_image`] (ADR-0029).
    pub fn send_file(
        &self,
        room_id: &str,
        file: Arc<crate::files::FileUpload>,
    ) -> Result<PendingMessage, SendRefusal> {
        if guard(&self.shared.account).is_none() {
            return Err(SendRefusal::Unavailable);
        }
        let queued = {
            // As for text: nothing queued here outlives a lock.
            let inner = lock(&self.shared);
            if inner.state != StoreState::Protected {
                return Err(SendRefusal::Unavailable);
            }
            guard(&self.shared.outbox).queue_file(room_id, file)
        }
        .ok_or(SendRefusal::Full)?;
        if let Some(listener) = self.shared.listener() {
            listener.outbox_changed(room_id);
        }
        Ok(queued)
    }

    /// Queues a picked image for the signed-in account, like
    /// [`Session::send_message`]. `Full` while another image is unsent.
    pub fn send_image(
        &self,
        room_id: &str,
        image: Arc<ImageUpload>,
    ) -> Result<PendingMessage, SendRefusal> {
        if guard(&self.shared.account).is_none() {
            return Err(SendRefusal::Unavailable);
        }
        let queued = {
            // As for text: nothing queued here outlives a lock.
            let inner = lock(&self.shared);
            if inner.state != StoreState::Protected {
                return Err(SendRefusal::Unavailable);
            }
            guard(&self.shared.outbox).queue_image(room_id, image)
        }
        .ok_or(SendRefusal::Full)?;
        if let Some(listener) = self.shared.listener() {
            listener.outbox_changed(room_id);
        }
        Ok(queued)
    }

    /// The generation of the open session, or `None` while it is not open.
    /// A lock advances it before anything else.
    pub fn open_generation(&self) -> Option<u64> {
        self.shared.open_generation()
    }

    /// The room's messages sent from this device in this session.
    pub fn pending_messages(&self, room_id: &str) -> Vec<PendingMessage> {
        guard(&self.shared.outbox).for_room(room_id)
    }

    /// Queues a failed or unknown message again, with its transaction ID.
    pub fn retry_message(&self, local_id: u32) -> bool {
        let room = guard(&self.shared.outbox).retry(local_id);
        self.outbox_changed(room)
    }

    /// Drops a message that is not being sent.
    pub fn discard_message(&self, local_id: u32) -> bool {
        let room = guard(&self.shared.outbox).discard(local_id);
        self.outbox_changed(room)
    }

    fn outbox_changed(&self, room: Option<String>) -> bool {
        let Some(room) = room else {
            return false;
        };
        if let Some(listener) = self.shared.listener() {
            listener.outbox_changed(&room);
        }
        true
    }

    /// Records the user's consent to send unencrypted messages in a room.
    /// `Ok(false)` when the consent list is full.
    pub fn allow_unencrypted(&self, room_id: &str) -> Result<bool, AdmissionError> {
        if guard(&self.shared.account).is_none() {
            return Err(AdmissionError::Locked);
        }
        let lease = self.shared.admit()?;
        let allowed = lease
            .stores()
            .block_on(sending::allow_plaintext(lease.stores(), room_id));
        Ok(allowed && lease.commit(|| ()).is_ok())
    }

    /// This account's settings. `Ok(None)` when the store cannot be read.
    pub fn account_settings(&self) -> Result<Option<AccountSettings>, AdmissionError> {
        if guard(&self.shared.account).is_none() {
            return Err(AdmissionError::Locked);
        }
        let lease = self.shared.admit()?;
        Ok(lease.stores().block_on(settings::load(lease.stores())))
    }

    /// Whether the account's settings send typing notices (ADR-0023).
    pub fn sends_typing(&self) -> bool {
        self.shared.send_typing.load(Ordering::SeqCst)
    }

    /// Whether the account's settings send public read receipts (ADR-0023).
    pub fn sends_read_receipts(&self) -> bool {
        self.shared.send_receipts.load(Ordering::SeqCst)
    }

    /// The account's idle lock in minutes, 0 when off (ADR-0025). It stays
    /// as last loaded while the profile is locked or signed out, which is
    /// harmless: only an open profile can be locked.
    pub fn idle_lock_minutes(&self) -> u16 {
        self.shared.idle_lock_minutes.load(Ordering::SeqCst)
    }

    /// Who is typing in each room, from the sync.
    pub fn typing(&self) -> &activity::Typing {
        &self.shared.typing
    }

    /// Replaces this account's settings. `Ok(false)` when the store could
    /// not be written.
    pub fn set_account_settings(&self, value: AccountSettings) -> Result<bool, AdmissionError> {
        if guard(&self.shared.account).is_none() {
            return Err(AdmissionError::Locked);
        }
        let lease = self.shared.admit()?;
        let saved = lease
            .stores()
            .block_on(settings::save(lease.stores(), value));
        let committed = saved
            && lease
                .commit(|| {
                    let shared = &self.shared;
                    shared
                        .toasts
                        .store(value.desktop_notifications, Ordering::SeqCst);
                    shared
                        .toast_names
                        .store(value.notification_names, Ordering::SeqCst);
                    shared
                        .send_typing
                        .store(value.send_typing, Ordering::SeqCst);
                    shared
                        .send_receipts
                        .store(value.send_read_receipts, Ordering::SeqCst);
                    shared
                        .idle_lock_minutes
                        .store(value.idle_lock_minutes, Ordering::SeqCst);
                })
                .is_ok();
        Ok(committed)
    }

    /// The signed-in account's user ID and its homeserver as the prompts
    /// name it, or `None` without one.
    pub fn account_names(&self) -> Option<(String, String)> {
        guard(&self.shared.account)
            .as_ref()
            .map(|account| (account.user_id.clone(), account.host.clone()))
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

    /// Starts, accepts, confirms or cancels a verification with another of
    /// the account's devices or with a contact. Verifying this device is
    /// offered only while it is not verified yet, and verifying a contact only
    /// once it is; the others need the current flow's ID and the right phase.
    /// Returns the status at once.
    pub fn verify(&self, action: VerifyAction) -> SecurityStatus {
        let Some(generation) = self.shared.open_generation() else {
            return self.security_status();
        };
        if guard(&self.shared.account).is_none() {
            return self.security_status();
        }
        self.shared
            .with_security(generation, |security| queue_verification(security, action));
        self.security_status()
    }

    /// Asks in the core's own window whether to accept `user_id`'s new
    /// identity in `room_id` (ADR-0007), then leaves the command for the
    /// security thread, which checks the change again before applying it.
    /// The caller has checked that the change exists and how; the window
    /// runs on its own thread without a lease, like the secret prompts, and
    /// an answer given after a lock is dropped. Does nothing while another
    /// operation runs.
    pub fn accept_identity(
        &self,
        room_id: String,
        user_id: OwnedUserId,
        was_verified: bool,
        source: Arc<dyn CredentialSource>,
    ) -> SecurityStatus {
        if guard(&self.shared.account).is_none() {
            return self.security_status();
        }
        let Some(started) = self.shared.open_generation() else {
            return self.security_status();
        };
        let accepted = self.shared.with_security(started, |security| {
            let free =
                security.status.operation == SecurityOperation::Idle && security.command.is_none();
            if free {
                security.status.operation = SecurityOperation::WaitingForConfirmation;
                security.status.error = None;
            }
            free
        });
        if accepted != Some(true) {
            return self.security_status();
        }
        let shared = self.shared.clone();
        let spawned = std::thread::Builder::new()
            .name("core-identity-prompt".into())
            .spawn(move || {
                let answer = source.confirm_identity_change(user_id.as_str(), was_verified);
                shared.with_security(started, |security| match answer {
                    Ok(true) => {
                        security.command = Some(SecurityCommand::AcceptIdentity {
                            room_id,
                            user_id,
                            was_verified,
                        });
                        security.status.operation = SecurityOperation::Working;
                    }
                    Ok(false) => {
                        security.status.operation = SecurityOperation::Idle;
                        security.status.error = Some(SecurityError::Cancelled);
                    }
                    Err(error) => {
                        security.status.operation = SecurityOperation::Idle;
                        security.status.error = Some(security_prompt_error(error));
                    }
                });
            });
        if spawned.is_err() {
            self.shared.with_security(started, |security| {
                security.status.operation = SecurityOperation::Idle;
                security.status.error = Some(SecurityError::Failed);
            });
        }
        self.security_status()
    }

    /// Starts replacing the recovery key (ADR-0021), for a verified device
    /// whose recovery is set up: the core warns in its own window that the
    /// old key stops working, and only an explicit yes leaves the command for
    /// the security thread, which shows the new key and offers to save it.
    /// The window runs on its own thread without a lease, like the identity
    /// question. Does nothing while another operation runs.
    pub fn change_recovery_key(&self, source: Arc<dyn CredentialSource>) -> SecurityStatus {
        if guard(&self.shared.account).is_none() {
            return self.security_status();
        }
        let Some(started) = self.shared.open_generation() else {
            return self.security_status();
        };
        let accepted = self.shared.with_security(started, |security| {
            let free = security.status.identity == IdentityState::Verified
                && security.status.recovery == RecoveryState::Enabled
                && security.status.operation == SecurityOperation::Idle
                && security.command.is_none();
            if free {
                security.status.operation = SecurityOperation::WaitingForConfirmation;
                security.status.error = None;
            }
            free
        });
        if accepted != Some(true) {
            return self.security_status();
        }
        let shared = self.shared.clone();
        let spawned = std::thread::Builder::new()
            .name("core-recovery-change".into())
            .spawn(move || {
                let answer = source.confirm_recovery_key_change();
                shared.with_security(started, |security| match answer {
                    Ok(true) => {
                        security.command = Some(SecurityCommand::ResetRecoveryKey { source });
                        security.status.operation = SecurityOperation::Working;
                    }
                    Ok(false) => {
                        security.status.operation = SecurityOperation::Idle;
                        security.status.error = Some(SecurityError::Cancelled);
                    }
                    Err(error) => {
                        security.status.operation = SecurityOperation::Idle;
                        security.status.error = Some(security_prompt_error(error));
                    }
                });
            });
        if spawned.is_err() {
            self.shared.with_security(started, |security| {
                security.status.operation = SecurityOperation::Idle;
                security.status.error = Some(SecurityError::Failed);
            });
        }
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

    /// Starts reopening a locked session: replies `unlocking` at once, then
    /// asks for the passphrase on a worker thread, opens the stores, restores
    /// a saved account and announces the result. A session that failed to
    /// open may retry; `lock_failed`, `restart_required`, `locking`,
    /// `unlocking` and an unconfigured session cannot unlock, and get their
    /// state back unchanged.
    pub fn unlock(&self) -> StoreState {
        let _transition = self
            .transition
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = self.state();
        let Some(dir) = self.profile.clone() else {
            return current;
        };
        match current {
            StoreState::Protected
            | StoreState::Locking
            | StoreState::Unlocking
            | StoreState::LockFailed
            | StoreState::RestartRequired
            | StoreState::NotConfigured => return current,
            _ => {}
        }
        self.open_in_background(dir, current, |dir, protector, unlock, _| {
            Some(store::open(dir, protector, unlock))
        })
    }

    /// Starts resetting the profile, for a forgotten passphrase (ADR-0006).
    /// Only a closed profile that no Matrix client used in this process can
    /// be reset. Replies `unlocking` at once; the worker shows the core's own
    /// warning, and only an explicit yes deletes the profile, after which a
    /// new passphrase is asked for a new, empty profile. Any other answer
    /// puts the session back as it was.
    pub fn reset_profile(&self) -> StoreState {
        let _transition = self
            .transition
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = self.state();
        let Some(dir) = self.profile.clone() else {
            return current;
        };
        let closed = matches!(
            current,
            StoreState::Locked
                | StoreState::SecretRejected
                | StoreState::KeyMissing
                | StoreState::KeyUnreadable
                | StoreState::KeyRejected
                | StoreState::StoreUnavailable
        );
        if !closed || self.shared.client_ran.load(Ordering::SeqCst) {
            return current;
        }
        self.open_in_background(dir, current, |dir, protector, unlock, shared| {
            if unlock.source.confirm_reset() != Ok(true) {
                return None;
            }
            Some(store::reset(dir).and_then(|had_data| {
                // The access token was in the deleted store, so the
                // homeserver was not told.
                if had_data {
                    shared.set_status(not_revoked());
                }
                store::open(dir, protector, unlock)
            }))
        })
    }

    /// Moves the session to `unlocking` and runs `open` on a worker thread
    /// without the transition lock, so an open prompt blocks no other call;
    /// `unlocking` refuses every other transition meanwhile. `None` from
    /// `open` puts the session back in `previous`. A panic in `open` (a
    /// prompt, the vault or a store) ends in `store_unavailable`, closed and
    /// retryable, instead of leaving the session `unlocking` for good.
    fn open_in_background(
        &self,
        dir: PathBuf,
        previous: StoreState,
        open: impl FnOnce(
            &Path,
            &dyn KeyProtector,
            &Unlock,
            &Arc<Shared>,
        ) -> Option<Result<Stores, StoreState>>
        + Send
        + 'static,
    ) -> StoreState {
        lock(&self.shared).state = StoreState::Unlocking;
        let shared = self.shared.clone();
        let protector = self.protector.clone();
        let unlock = self.unlock.clone();
        let spawned = std::thread::Builder::new()
            .name("core-unlock".into())
            .spawn(move || {
                // Anything `open` held (stores, the profile lock) is dropped
                // while the panic unwinds, before the state is published.
                let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    open(&dir, &*protector, &unlock, &shared)
                }));
                match opened {
                    Ok(Some(opened)) => finish_open(&shared, opened),
                    Ok(None) => {
                        lock(&shared).state = previous;
                        if let Some(listener) = shared.listener() {
                            listener.session_changed();
                        }
                    }
                    Err(_) => finish_open(&shared, Err(StoreState::StoreUnavailable)),
                }
            });
        if spawned.is_err() {
            lock(&self.shared).state = previous;
            return previous;
        }
        StoreState::Unlocking
    }
}

/// Publishes the result of an unlock or reset, then restores a saved account.
/// No listener runs before the state and stores are published together, so
/// a listener that panics cannot leave the session `unlocking`.
fn finish_open(shared: &Arc<Shared>, opened: Result<Stores, StoreState>) {
    let (state, stores) = match opened {
        Ok(stores) => (StoreState::Protected, Some(Arc::new(stores))),
        Err(state) => (state, None),
    };
    if stores.as_ref().is_some_and(|s| s.unconfirmed_sign_out()) {
        // Announced with the state change below.
        *guard(&shared.status) = not_revoked();
    }
    {
        let mut inner = lock(shared);
        inner.state = state;
        inner.stores = stores;
    }
    if let Some(listener) = shared.listener() {
        listener.session_changed();
    }
    if state == StoreState::Protected {
        restore(shared);
    }
}

/// Restores a saved account without a prompt and starts syncing. Does
/// nothing when no account was saved or one is already running.
fn restore(shared: &Arc<Shared>) {
    if guard(&shared.account).is_some() {
        return;
    }
    let Ok(lease) = shared.admit() else {
        return;
    };
    let restored = lease.stores().block_on(async {
        let result = account::restore(lease.stores()).await;
        if !matches!(result, Ok(None)) {
            shared.client_ran.store(true, Ordering::SeqCst);
        }
        result
    });
    match restored {
        Ok(Some((client, user_id))) => install(shared, lease, client, user_id),
        Ok(None) => {}
        Err(error) => shared.set_status(failed(error)),
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

    fn unverified() -> SecurityShared {
        let mut security = SecurityShared::default();
        security.status.identity = IdentityState::Unverified;
        security.status.operation = SecurityOperation::Idle;
        security
    }

    fn flow(phase: VerificationPhase) -> Option<VerificationView> {
        // A comparison always carries its seven emoji.
        let emoji = if phase == VerificationPhase::Compare {
            vec![
                SasEmoji {
                    symbol: "x".into(),
                    description: "X".into(),
                };
                verification::SAS_EMOJI
            ]
        } else {
            Vec::new()
        };
        Some(VerificationView {
            flow_id: "f1".into(),
            phase,
            emoji,
            other_device: None,
            contact: None,
        })
    }

    /// GIVEN a comparison without its seven emoji WHEN the page confirms THEN
    /// nothing is queued: a match needs something to compare. Declining is
    /// still allowed.
    #[test]
    fn given_a_comparison_without_seven_emoji_then_confirm_is_refused() {
        for count in [0, 6, 8] {
            let mut security = unverified();
            let mut view = flow(VerificationPhase::Compare).unwrap();
            let one = view.emoji[0].clone();
            view.emoji.resize(count, one);
            security.status.verification = Some(view);
            assert!(
                !queue_verification(&mut security, VerifyAction::Confirm("f1".into())),
                "{count} emoji"
            );
            assert!(queue_verification(
                &mut security,
                VerifyAction::Cancel("f1".into())
            ));
        }
    }

    /// GIVEN an unverified device WHEN a start is queued THEN the operation
    /// is working until the security thread has run it, so a second start
    /// (or any other action) is refused meanwhile.
    #[test]
    fn given_a_queued_verification_then_a_second_action_waits() {
        let mut security = unverified();
        assert!(queue_verification(&mut security, VerifyAction::Start));
        assert_eq!(security.status.operation, SecurityOperation::Working);
        assert!(!queue_verification(&mut security, VerifyAction::Start));
        // The thread takes the command; until it publishes, still working.
        security.command = None;
        assert!(!queue_verification(&mut security, VerifyAction::Start));
    }

    /// GIVEN this device's identity state WHEN the page asks to verify a
    /// contact THEN only a verified device queues it, and not while another
    /// flow is open; verifying this device is no longer offered once it is
    /// verified.
    #[test]
    fn given_a_contact_verification_then_only_a_verified_device_queues_it() {
        let contact = || VerifyAction::StartContact("!dm:s".into());
        for identity in [
            IdentityState::Unknown,
            IdentityState::None,
            IdentityState::Unverified,
        ] {
            let mut security = unverified();
            security.status.identity = identity;
            assert!(
                !queue_verification(&mut security, contact()),
                "{identity:?}"
            );
            assert!(security.command.is_none());
        }
        let mut verified = unverified();
        verified.status.identity = IdentityState::Verified;
        assert!(!queue_verification(&mut verified, VerifyAction::Start));
        verified.status.verification = flow(VerificationPhase::Waiting);
        assert!(
            !queue_verification(&mut verified, contact()),
            "a flow is open"
        );
        verified.status.verification = flow(VerificationPhase::Done);
        assert!(queue_verification(&mut verified, contact()));
        assert!(matches!(
            verified.command,
            Some(SecurityCommand::Verify(VerifyAction::StartContact(ref room))) if room == "!dm:s"
        ));
        assert_eq!(verified.status.operation, SecurityOperation::Working);
    }

    /// GIVEN this device's state WHEN it asks to verify another of the
    /// account's devices THEN only a verified device with no open flow and
    /// nothing running queues it.
    #[test]
    fn given_a_device_verification_then_only_a_verified_idle_device_queues_it() {
        let device = || VerifyAction::StartDevice("OTHER".into());
        for identity in [
            IdentityState::Unknown,
            IdentityState::None,
            IdentityState::Unverified,
        ] {
            let mut security = unverified();
            security.status.identity = identity;
            assert!(!queue_verification(&mut security, device()), "{identity:?}");
            assert!(security.command.is_none());
        }
        let mut verified = unverified();
        verified.status.identity = IdentityState::Verified;
        verified.status.verification = flow(VerificationPhase::Compare);
        assert!(
            !queue_verification(&mut verified, device()),
            "a flow is open"
        );
        verified.status.verification = flow(VerificationPhase::Cancelled);
        verified.status.operation = SecurityOperation::WaitingForConfirmation;
        assert!(!queue_verification(&mut verified, device()), "busy");
        verified.status.operation = SecurityOperation::Idle;
        assert!(queue_verification(&mut verified, device()));
        assert!(matches!(
            verified.command,
            Some(SecurityCommand::Verify(VerifyAction::StartDevice(ref id))) if id == "OTHER"
        ));
        assert!(
            !queue_verification(&mut verified, device()),
            "a second one waits for the first"
        );
    }

    /// GIVEN each phase WHEN an action arrives THEN only the action for that
    /// flow and phase is queued.
    #[test]
    fn given_each_phase_then_only_its_actions_are_queued() {
        let id = || "f1".to_owned();
        let cases = [
            (
                VerificationPhase::Incoming,
                VerifyAction::Accept(id()),
                true,
            ),
            (
                VerificationPhase::Incoming,
                VerifyAction::Confirm(id()),
                false,
            ),
            (VerificationPhase::Incoming, VerifyAction::Start, false),
            (
                VerificationPhase::Compare,
                VerifyAction::Confirm(id()),
                true,
            ),
            (
                VerificationPhase::Compare,
                VerifyAction::Accept(id()),
                false,
            ),
            (VerificationPhase::Compare, VerifyAction::Cancel(id()), true),
            (
                VerificationPhase::Compare,
                VerifyAction::Cancel("other".into()),
                false,
            ),
            // A confirmation the other side never got must not leave the
            // flow stuck: waiting can be cancelled.
            (
                VerificationPhase::Confirmed,
                VerifyAction::Cancel(id()),
                true,
            ),
            (
                VerificationPhase::Confirmed,
                VerifyAction::Confirm(id()),
                false,
            ),
            (VerificationPhase::Cancelled, VerifyAction::Start, true),
            (
                VerificationPhase::Cancelled,
                VerifyAction::Cancel(id()),
                false,
            ),
        ];
        for (phase, action, expected) in cases {
            let mut security = unverified();
            security.status.verification = flow(phase);
            let label = format!("{phase:?} {action:?}");
            assert_eq!(
                queue_verification(&mut security, action),
                expected,
                "{label}"
            );
        }
    }
    use crate::credentials::{CredentialSource, Credentials, PromptError, SecretPrompt};
    use crate::passphrase::KdfParams;
    use crate::vault::VaultError;
    use persephone_contracts::{SasEmoji, VerificationView};

    /// Keeps the sealed key as it is: enough for a profile in a temporary
    /// folder.
    struct Plain;

    impl KeyProtector for Plain {
        fn protect(&self, data: &[u8]) -> Result<Vec<u8>, VaultError> {
            Ok(data.to_vec())
        }

        fn unprotect(&self, blob: &[u8]) -> Result<Zeroizing<Vec<u8>>, VaultError> {
            Ok(Zeroizing::new(blob.to_vec()))
        }
    }

    /// Answers every passphrase prompt with the same passphrase.
    struct Answer;

    impl CredentialSource for Answer {
        fn ask(&self, _: &str) -> Result<Credentials, PromptError> {
            Err(PromptError::Unsupported)
        }

        fn ask_secret(&self, _: SecretPrompt<'_>) -> Result<Zeroizing<String>, PromptError> {
            Ok(Zeroizing::new("correct horse battery".into()))
        }
    }

    fn unlock() -> Unlock {
        Unlock {
            source: Arc::new(Answer),
            kdf: KdfParams::new(64, 1, 1).unwrap(),
            backoff: Duration::ZERO,
        }
    }

    fn settled(session: &Session) -> StoreState {
        let deadline = Instant::now() + Duration::from_secs(30);
        while session.state() == StoreState::Unlocking && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        session.state()
    }

    /// GIVEN a write meant for the session before a lock WHEN it arrives
    /// after the lock, or after a later unlock THEN it is refused: no stale
    /// status or command reaches the locked or the new session.
    #[test]
    fn given_a_lock_then_writes_for_the_old_session_are_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let session = Session::open(dir.path().into(), Arc::new(Plain), unlock());
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

        assert_eq!(session.unlock(), StoreState::Unlocking);
        assert_eq!(settled(&session), StoreState::Protected);
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
