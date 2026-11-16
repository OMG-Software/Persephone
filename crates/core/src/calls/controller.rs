//! The call controller: one MatrixRTC audio call at a time (ADR-0043).
//!
//! The controller does the Matrix side of a call and drives the contained
//! call worker. Joining fails closed at every step and undoes the earlier
//! steps; see [`CallController::join`] for the order. While a call runs, one
//! task owns the worker link, the frame keys and the call state. The page
//! gets only [`CallState`]: no key, token, URL or peer text.

use super::delayed::{DelayedLeave, RestartHealth};
use super::encryption_keys::{self, IncomingKey};
use super::incoming::{self, Candidate};
use super::keys::{KeyError, Outgoing, OwnKeys, Recipients, RemoteKeys};
use super::membership::{
    ActiveMember, DELAYED_LEAVE_RESTART, EVENT_TYPE, MEMBERSHIP_REFRESH, leave_content,
    own_membership, own_state_key, participant_identity, read_members,
};
use super::readiness::{TurnServers, sfu_transport, turn_servers};
use super::sfu::{self, OpenIdToken, SfuError};
use super::worker::{CallHost, CallWorkerLink, StartError};
use super::{MAX_CALL_MEMBERS, keys::FrameKey};
use matrix_sdk::{
    Client, Room, RoomState,
    event_handler::EventHandlerHandle,
    ruma::{
        OwnedDeviceId, OwnedRoomId, OwnedUserId, RoomId,
        api::client::account::request_openid_token, events::AnySyncStateEvent, serde::Raw,
    },
};
use persephone_call_protocol::{FailureCode, FrameKeyMsg, FromWorker, ToWorker, TurnServer};
use std::{
    collections::{BTreeSet, HashSet},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
    time,
};
use url::Url;
use zeroize::Zeroizing;

/// How long the worker has to leave on its own before it is ended.
const WORKER_EXIT: Duration = Duration::from_secs(5);

/// Longest wait for one delayed-event restart. It is shorter than the
/// restart interval.
const RESTART_TIMEOUT: Duration = Duration::from_secs(4);

/// Longest wait for one key send or membership write.
const SEND_TIMEOUT: Duration = Duration::from_secs(20);

/// Most keys held back for a sender whose membership event is not here yet.
const MAX_PARKED: usize = 8;

/// How long such a key is held back.
const PARK_TIME: Duration = Duration::from_secs(30);

/// Waiting commands for the running call.
const COMMAND_QUEUE: usize = 8;

/// Why the controller refused or stopped a join. A closed set with no text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallError {
    AlreadyInCall,
    /// The room is unknown or not joined.
    NotJoined,
    /// The room is not encrypted, or its state is unknown. A call never
    /// runs without frame encryption.
    NotEncrypted,
    /// The homeserver offers no usable SFU, or could not be asked.
    NoSfu,
    /// The profile has a proxy and the user did not allow a direct call.
    Declined,
    /// The profile's proxy setting could not be read.
    ProxyUnreadable,
    Worker(StartError),
    /// The homeserver does not accept delayed events (MSC4140).
    NoDelayedEvents,
    /// Own membership could not be written or read.
    Membership,
    OpenId,
    Sfu(SfuError),
    /// No random key, or the worker queue refused it.
    Keys,
    NotInCall,
}

/// Why a call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// This user left.
    Left,
    WorkerFailed(FailureCode),
    /// The worker exited or its channel closed.
    WorkerStopped,
    /// The delayed leave could not be kept alive, or this device's
    /// membership disappeared.
    MembershipLost,
    /// No random key could be made.
    KeysUnavailable,
    Internal,
}

/// A device in the call, as the page shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Participant {
    pub user_id: OwnedUserId,
    pub device_id: OwnedDeviceId,
    pub speaking: bool,
    /// It publishes to the SFU this call uses. Other members cannot be
    /// heard in this version.
    pub reachable: bool,
    /// This device holds a key for it. Without one it cannot be heard.
    pub keyed: bool,
}

/// The call as the page sees it. It holds no secret and no peer text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallState {
    Idle,
    Joining {
        room_id: OwnedRoomId,
    },
    Active {
        room_id: OwnedRoomId,
        muted: bool,
        /// At most [`MAX_CALL_MEMBERS`], other devices only.
        participants: Vec<Participant>,
    },
    Ended {
        room_id: OwnedRoomId,
        reason: EndReason,
    },
}

/// Whether the profile has a SOCKS5 proxy: the `proxy_configured` argument of
/// [`CallController::join`]. A record that cannot be read refuses the call.
pub async fn proxy_configured(stores: &crate::store::Stores) -> Result<bool, CallError> {
    crate::proxy::load(stores)
        .await
        .map(|proxy| proxy.is_some())
        .map_err(|_| CallError::ProxyUnreadable)
}

// ---------------------------------------------------------------------
// Pure parts.

/// Who joined and left since the last look, by participant identity.
#[derive(Debug, Default, PartialEq, Eq)]
struct MemberDiff {
    joined: Vec<String>,
    left: Vec<String>,
}

/// The change from `known` to `current`.
fn diff_members(known: &BTreeSet<String>, current: &[ActiveMember]) -> MemberDiff {
    let now: BTreeSet<&str> = current.iter().map(|m| m.identity.as_str()).collect();
    MemberDiff {
        joined: current
            .iter()
            .filter(|m| !known.contains(&m.identity))
            .map(|m| m.identity.clone())
            .collect(),
        left: known
            .iter()
            .filter(|id| !now.contains(id.as_str()))
            .cloned()
            .collect(),
    }
}

/// The key action for `diff`: a leave always rotates (the member may hold the
/// key; the new key also reaches new members); else a join shares the
/// current key when it is young, or rotates. `None` when nothing changed.
fn key_action(
    diff: &MemberDiff,
    own: &mut OwnKeys,
    now: Instant,
) -> Result<Option<Outgoing>, KeyError> {
    if !diff.left.is_empty() {
        own.member_left(now).map(Some)
    } else if !diff.joined.is_empty() {
        own.member_joined(now).map(Some)
    } else {
        Ok(None)
    }
}

/// The devices to send `outgoing` to. `members` are the other devices.
fn recipients(
    outgoing: &Outgoing,
    members: &[ActiveMember],
    joined: &[String],
) -> Vec<(OwnedUserId, OwnedDeviceId)> {
    members
        .iter()
        .filter(|m| match outgoing.recipients {
            Recipients::AllMembers => true,
            Recipients::NewMembersOnly => joined.contains(&m.identity),
        })
        .take(MAX_CALL_MEMBERS)
        .map(|m| (m.user_id.clone(), m.device_id.clone()))
        .collect()
}

fn worker_key(key: &FrameKey) -> FrameKeyMsg {
    FrameKeyMsg {
        index: key.index,
        bytes: Zeroizing::new(key.bytes.to_vec()),
    }
}

/// What a worker event did to the view.
#[derive(Debug, PartialEq, Eq)]
enum Applied {
    Same,
    Changed,
    End(EndReason),
}

/// The facts the page's [`CallState`] comes from.
struct View {
    room_id: OwnedRoomId,
    own_sfu: Url,
    connected: bool,
    muted: bool,
    /// Other devices only.
    members: Vec<ActiveMember>,
    speaking: HashSet<String>,
    keyed: HashSet<String>,
}

impl View {
    fn new(room_id: OwnedRoomId, own_sfu: Url) -> Self {
        Self {
            room_id,
            own_sfu,
            connected: false,
            muted: false,
            members: Vec::new(),
            speaking: HashSet::new(),
            keyed: HashSet::new(),
        }
    }

    fn is_member(&self, identity: &str) -> bool {
        self.members.iter().any(|m| m.identity == identity)
    }

    fn set_members(&mut self, members: Vec<ActiveMember>) {
        self.members = members;
        let present: HashSet<&str> = self.members.iter().map(|m| m.identity.as_str()).collect();
        self.speaking.retain(|id| present.contains(id.as_str()));
        self.keyed.retain(|id| present.contains(id.as_str()));
    }

    fn apply(&mut self, event: &FromWorker) -> Applied {
        match event {
            FromWorker::Ready { .. } | FromWorker::ParticipantJoined { .. } => Applied::Same,
            FromWorker::Connected => {
                let changed = !self.connected;
                self.connected = true;
                if changed {
                    Applied::Changed
                } else {
                    Applied::Same
                }
            }
            FromWorker::ParticipantLeft { identity } => {
                if self.speaking.remove(identity) {
                    Applied::Changed
                } else {
                    Applied::Same
                }
            }
            FromWorker::Speaking { identity, speaking } => {
                // Only a current member can be shown as speaking.
                if !self.is_member(identity) {
                    return Applied::Same;
                }
                let changed = if *speaking {
                    self.speaking.insert(identity.clone())
                } else {
                    self.speaking.remove(identity)
                };
                if changed {
                    Applied::Changed
                } else {
                    Applied::Same
                }
            }
            FromWorker::Muted { muted } => {
                let changed = self.muted != *muted;
                self.muted = *muted;
                if changed {
                    Applied::Changed
                } else {
                    Applied::Same
                }
            }
            FromWorker::Failed { code } => Applied::End(EndReason::WorkerFailed(*code)),
            FromWorker::Left => Applied::End(EndReason::WorkerStopped),
        }
    }

    fn state(&self) -> CallState {
        if !self.connected {
            return CallState::Joining {
                room_id: self.room_id.clone(),
            };
        }
        CallState::Active {
            room_id: self.room_id.clone(),
            muted: self.muted,
            participants: self
                .members
                .iter()
                .take(MAX_CALL_MEMBERS)
                .map(|m| Participant {
                    user_id: m.user_id.clone(),
                    device_id: m.device_id.clone(),
                    speaking: self.speaking.contains(&m.identity),
                    reachable: m.service_url == self.own_sfu,
                    keyed: self.keyed.contains(&m.identity),
                })
                .collect(),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn turn_for_worker(turn: Option<TurnServers>) -> Vec<TurnServer> {
    turn.map(|turn| {
        turn.uris
            .iter()
            .map(|uri| TurnServer {
                uri: uri.clone(),
                username: turn.username.clone(),
                credential: turn.password.clone(),
            })
            .collect()
    })
    .unwrap_or_default()
}

/// Asks before a call connects around the profile's proxy. Anything but an
/// explicit yes declines.
async fn confirm_direct(host: &Arc<dyn CallHost>, proxy_configured: bool) -> Result<(), CallError> {
    if !proxy_configured {
        return Ok(());
    }
    let host = host.clone();
    match tokio::task::spawn_blocking(move || host.confirm_direct_connection()).await {
        Ok(true) => Ok(()),
        _ => Err(CallError::Declined),
    }
}

/// Starts the contained worker. It runs before any Matrix side effect.
async fn start_worker(host: &Arc<dyn CallHost>) -> Result<CallWorkerLink, CallError> {
    let host = host.clone();
    match tokio::task::spawn_blocking(move || host.start()).await {
        Ok(Ok(link)) => Ok(link),
        Ok(Err(error)) => Err(CallError::Worker(error)),
        Err(_) => Err(CallError::Worker(StartError::Launch)),
    }
}

/// Ends the worker; blocks in a blocking task.
async fn finish_worker(link: CallWorkerLink) {
    let _ = tokio::task::spawn_blocking(move || link.finish(WORKER_EXIT)).await;
}

/// Writes the leave now: through the delayed event, else directly.
async fn end_membership(client: &Client, room: &Room, delayed: &DelayedLeave, state_key: &str) {
    if delayed.send_now(client).await.is_ok() {
        return;
    }
    let _ = time::timeout(
        SEND_TIMEOUT,
        room.send_state_event_raw(EVENT_TYPE, state_key, leave_content()),
    )
    .await;
}

// ---------------------------------------------------------------------
// The controller.

enum Slot {
    Idle,
    /// A join is in progress.
    Joining,
    Running(RunHandle),
}

struct RunHandle {
    stop: Option<oneshot::Sender<()>>,
    commands: mpsc::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

struct Inner {
    host: Arc<dyn CallHost>,
    state: watch::Sender<CallState>,
    slot: Mutex<Slot>,
}

impl Inner {
    fn slot(&self) -> MutexGuard<'_, Slot> {
        // A poisoned slot only means a task panicked; the data is a plain
        // enum.
        self.slot.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn publish(&self, state: CallState) {
        self.state.send_if_modified(|current| {
            if *current == state {
                false
            } else {
                *current = state;
                true
            }
        });
    }
}

/// Frees the slot when a join ends without a running call, also when the
/// join future is dropped. A dropped worker link is killed; a scheduled
/// delayed leave then fires on its own.
struct JoinGuard {
    inner: Arc<Inner>,
    armed: bool,
}

impl Drop for JoinGuard {
    fn drop(&mut self) {
        if self.armed {
            *self.inner.slot() = Slot::Idle;
            self.inner.publish(CallState::Idle);
        }
    }
}

/// The one call of this core.
pub struct CallController {
    inner: Arc<Inner>,
}

impl CallController {
    pub fn new(host: Arc<dyn CallHost>) -> Self {
        let (state, _) = watch::channel(CallState::Idle);
        Self {
            inner: Arc::new(Inner {
                host,
                state,
                slot: Mutex::new(Slot::Idle),
            }),
        }
    }

    /// The content-free call state.
    pub fn subscribe(&self) -> watch::Receiver<CallState> {
        self.inner.state.subscribe()
    }

    fn claim(&self, room_id: &RoomId) -> Result<JoinGuard, CallError> {
        let mut slot = self.inner.slot();
        if !matches!(*slot, Slot::Idle) {
            return Err(CallError::AlreadyInCall);
        }
        *slot = Slot::Joining;
        drop(slot);
        self.inner.publish(CallState::Joining {
            room_id: room_id.to_owned(),
        });
        Ok(JoinGuard {
            inner: self.inner.clone(),
            armed: true,
        })
    }

    /// Joins the call in `room_id`. `proxy_configured` is from
    /// [`proxy_configured`].
    ///
    /// Each step fails closed and undoes the earlier ones:
    /// 1. one call at a time;
    /// 2. the room is joined and encrypted;
    /// 3. the homeserver offers an SFU (TURN servers are optional);
    /// 4. with a proxy, the user allows a direct connection;
    /// 5. the contained worker starts, before any Matrix side effect;
    /// 6. the delayed leave is scheduled (MSC4140) and restarted from now;
    /// 7. own membership is written;
    /// 8. an OpenID token buys an SFU grant;
    /// 9. frame keys are made and the worker joins. The task then sends the
    ///    key to the members that are there.
    pub async fn join(
        &self,
        client: &Client,
        room_id: &RoomId,
        proxy_configured: bool,
    ) -> Result<(), CallError> {
        let mut guard = self.claim(room_id)?;
        let mut setup = Setup::new(client.clone());
        let prepared = self
            .prepare(client, room_id, proxy_configured, &mut setup)
            .await;
        let run = match prepared {
            Ok(run) => run,
            Err(error) => {
                setup.undo().await;
                return Err(error);
            }
        };
        let (stop, stop_rx) = oneshot::channel();
        let (commands, command_rx) = mpsc::channel(COMMAND_QUEUE);
        let inner = self.inner.clone();
        let mut slot = self.inner.slot();
        let task = tokio::spawn(run_call(inner, run, stop_rx, command_rx));
        *slot = Slot::Running(RunHandle {
            stop: Some(stop),
            commands,
            task: Some(task),
        });
        guard.armed = false;
        Ok(())
    }

    async fn prepare(
        &self,
        client: &Client,
        room_id: &RoomId,
        proxy_configured: bool,
        setup: &mut Setup,
    ) -> Result<Call, CallError> {
        // 2. A joined, encrypted room.
        let room = client.get_room(room_id).ok_or(CallError::NotJoined)?;
        if room.state() != RoomState::Joined {
            return Err(CallError::NotJoined);
        }
        match room.latest_encryption_state().await {
            Ok(state) if state.is_encrypted() => {}
            _ => return Err(CallError::NotEncrypted),
        }
        let user_id = client.user_id().ok_or(CallError::NotJoined)?.to_owned();
        let device_id = client.device_id().ok_or(CallError::NotJoined)?.to_owned();

        // 3. An SFU; TURN is optional.
        let sfu_url = match sfu_transport(client).await {
            Ok(Some(url)) => url,
            _ => return Err(CallError::NoSfu),
        };
        let turn = turn_servers(client).await.ok().flatten();

        // 4. Direct connection, only when the user says yes.
        confirm_direct(&self.inner.host, proxy_configured).await?;

        // 5. The worker, before any Matrix side effect.
        setup.link = Some(start_worker(&self.inner.host).await?);

        let (change_tx, changes) = mpsc::channel(1);
        let (key_tx, incoming) = mpsc::channel(incoming::QUEUE);
        let (failed_tx, failed) = mpsc::channel(1);
        setup
            .handles
            .push(room.add_event_handler(move |raw: Raw<AnySyncStateEvent>| {
                let change_tx = change_tx.clone();
                async move {
                    #[derive(serde::Deserialize)]
                    struct Kind {
                        #[serde(rename = "type")]
                        kind: String,
                    }
                    if serde_json::from_str::<Kind>(raw.json().get())
                        .is_ok_and(|k| k.kind == EVENT_TYPE)
                    {
                        // A full queue already holds a pending change.
                        let _ = change_tx.try_send(());
                    }
                }
            }));
        setup
            .handles
            .push(incoming::register(client, room.clone(), key_tx));

        // 6. The delayed leave, restarted from now on.
        let state_key = own_state_key(&user_id, &device_id);
        let delayed = DelayedLeave::schedule(client, room_id, &state_key)
            .await
            .map_err(|_| CallError::NoDelayedEvents)?;
        setup.delayed = Some(delayed.clone());
        let created_ts = now_ms();
        setup.tasks.spawn(keeper(
            client.clone(),
            room.clone(),
            delayed.clone(),
            KeeperMembership {
                state_key: state_key.clone(),
                user_id: user_id.clone(),
                device_id: device_id.clone(),
                sfu_url: sfu_url.clone(),
                created_ts,
            },
            failed_tx,
        ));

        // 7. Own membership.
        let content = own_membership(
            room_id.as_str(),
            &device_id,
            &user_id,
            &sfu_url,
            Some(created_ts),
        );
        setup.membership_sent = true;
        time::timeout(
            SEND_TIMEOUT,
            room.send_state_event_raw(EVENT_TYPE, &state_key, content),
        )
        .await
        .map_err(|_| CallError::Membership)?
        .map_err(|_| CallError::Membership)?;

        // 8. The SFU grant.
        let openid = client
            .send(request_openid_token::v3::Request::new(user_id.clone()))
            .await
            .map_err(|_| CallError::OpenId)?;
        let body = sfu::request_body(room_id, &device_id, &OpenIdToken::from(openid));
        let grant = sfu::get_grant(client.http_client(), &sfu_url, &body)
            .await
            .map_err(CallError::Sfu)?;

        // 9. Keys, then the worker joins.
        let own_keys = OwnKeys::new(Instant::now()).map_err(|_| CallError::Keys)?;
        let own_identity = participant_identity(&user_id, &device_id);
        let link = setup.link.take().ok_or(CallError::Keys)?;
        let join = ToWorker::Join {
            sfu_url: grant.url.to_string(),
            jwt: grant.jwt.clone(),
            turn: turn_for_worker(turn),
            relay_only: false,
            own_identity: own_identity.clone(),
            own_key: worker_key(own_keys.current()),
        };
        if link.send(join).is_err() {
            setup.link = Some(link);
            return Err(CallError::Worker(StartError::NotReady));
        }
        Ok(Call {
            client: client.clone(),
            room,
            link,
            delayed,
            state_key,
            own_identity,
            own_device: device_id,
            own_keys,
            remote: RemoteKeys::new(),
            view: View::new(room_id.to_owned(), sfu_url),
            changes,
            incoming,
            failed,
            handles: std::mem::take(&mut setup.handles),
            tasks: std::mem::take(&mut setup.tasks),
        })
    }

    /// Leaves the call: the worker ends, the leave is written, the handlers
    /// go. Returns when the call is over.
    pub async fn leave(&self) -> Result<(), CallError> {
        let (stop, task) = {
            let mut slot = self.inner.slot();
            match &mut *slot {
                Slot::Running(run) if run.stop.is_some() => (run.stop.take(), run.task.take()),
                _ => return Err(CallError::NotInCall),
            }
        };
        if let Some(stop) = stop {
            let _ = stop.send(());
        }
        if let Some(task) = task {
            let _ = task.await;
        }
        Ok(())
    }

    /// Mutes or unmutes the microphone. The state shows the worker's answer.
    pub fn set_muted(&self, muted: bool) -> Result<(), CallError> {
        match &*self.inner.slot() {
            Slot::Running(run) => run
                .commands
                .try_send(muted)
                .map_err(|_| CallError::NotInCall),
            _ => Err(CallError::NotInCall),
        }
    }
}

/// What a join has made so far, so a failure can undo it.
struct Setup {
    client: Client,
    link: Option<CallWorkerLink>,
    delayed: Option<DelayedLeave>,
    membership_sent: bool,
    handles: Vec<EventHandlerHandle>,
    tasks: JoinSet<()>,
}

impl Setup {
    fn new(client: Client) -> Self {
        Self {
            client,
            link: None,
            delayed: None,
            membership_sent: false,
            handles: Vec::new(),
            tasks: JoinSet::new(),
        }
    }

    async fn undo(mut self) {
        self.tasks.abort_all();
        for handle in self.handles.drain(..) {
            self.client.remove_event_handler(handle);
        }
        if let Some(link) = self.link.take() {
            finish_worker(link).await;
        }
        if let Some(delayed) = self.delayed.take() {
            // Nothing was written when the membership was not sent.
            if self.membership_sent {
                // Write the leave now; if that fails, the delay still fires.
                let _ = delayed.send_now(&self.client).await;
            } else {
                let _ = delayed.cancel(&self.client).await;
            }
        }
    }
}

struct KeeperMembership {
    state_key: String,
    user_id: OwnedUserId,
    device_id: OwnedDeviceId,
    sfu_url: Url,
    created_ts: u64,
}

/// Restarts the delayed leave and refreshes the membership while the call
/// runs. It sends on `failed` when the restart failed twice in a row.
async fn keeper(
    client: Client,
    room: Room,
    delayed: DelayedLeave,
    membership: KeeperMembership,
    failed: mpsc::Sender<()>,
) {
    let mut beat = time::interval(DELAYED_LEAVE_RESTART);
    beat.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    let mut refresh = time::interval_at(
        time::Instant::now() + MEMBERSHIP_REFRESH,
        MEMBERSHIP_REFRESH,
    );
    refresh.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    let mut health = RestartHealth::default();
    loop {
        tokio::select! {
            _ = beat.tick() => {
                let ok = matches!(
                    time::timeout(RESTART_TIMEOUT, delayed.restart(&client)).await,
                    Ok(Ok(()))
                );
                if health.record(ok) {
                    let _ = failed.send(()).await;
                    return;
                }
            }
            _ = refresh.tick() => {
                let content = own_membership(
                    room.room_id().as_str(),
                    &membership.device_id,
                    &membership.user_id,
                    &membership.sfu_url,
                    Some(membership.created_ts),
                );
                let _ = time::timeout(
                    SEND_TIMEOUT,
                    room.send_state_event_raw(EVENT_TYPE, &membership.state_key, content),
                )
                .await;
            }
        }
    }
}

/// Everything one running call owns.
struct Call {
    client: Client,
    room: Room,
    link: CallWorkerLink,
    delayed: DelayedLeave,
    state_key: String,
    own_identity: String,
    own_device: OwnedDeviceId,
    own_keys: OwnKeys,
    remote: RemoteKeys,
    view: View,
    changes: mpsc::Receiver<()>,
    incoming: mpsc::Receiver<Candidate>,
    failed: mpsc::Receiver<()>,
    handles: Vec<EventHandlerHandle>,
    tasks: JoinSet<()>,
}

/// Resets the slot when the task ends, also after a panic.
struct EndGuard {
    inner: Arc<Inner>,
    room_id: OwnedRoomId,
}

impl Drop for EndGuard {
    fn drop(&mut self) {
        *self.inner.slot() = Slot::Idle;
        if !matches!(*self.inner.state.borrow(), CallState::Ended { .. }) {
            self.inner.publish(CallState::Ended {
                room_id: self.room_id.clone(),
                reason: EndReason::Internal,
            });
        }
    }
}

async fn run_call(
    inner: Arc<Inner>,
    mut call: Call,
    mut stop: oneshot::Receiver<()>,
    mut commands: mpsc::Receiver<bool>,
) {
    let room_id = call.view.room_id.clone();
    let _guard = EndGuard {
        inner: inner.clone(),
        room_id: room_id.clone(),
    };
    let reason = match call.run(&inner, &mut stop, &mut commands).await {
        Ok(()) => EndReason::Left,
        Err(reason) => reason,
    };
    call.finish().await;
    inner.publish(CallState::Ended { room_id, reason });
}

impl Call {
    fn send_worker(&self, message: ToWorker) -> Result<(), EndReason> {
        self.link
            .send(message)
            .map_err(|_| EndReason::WorkerStopped)
    }

    fn publish(&self, inner: &Inner) {
        inner.publish(self.view.state());
    }

    /// Sends `outgoing` to `recipients`. A failure is not fatal: a device
    /// that did not get the key cannot hear this user.
    async fn send_keys(&self, outgoing: &Outgoing, recipients: Vec<(OwnedUserId, OwnedDeviceId)>) {
        if recipients.is_empty() {
            return;
        }
        let content = encryption_keys::outgoing_content(
            self.room.room_id(),
            &self.own_identity,
            &self.own_device,
            &outgoing.key,
        );
        let _ = time::timeout(
            SEND_TIMEOUT,
            encryption_keys::send_key(&self.room, &recipients, content),
        )
        .await;
    }

    /// Reads the members and gives the current key to all of them.
    async fn start_keys(&mut self, inner: &Inner) -> Result<(), EndReason> {
        if let Ok(members) = read_members(&self.room, now_ms()).await {
            self.view.set_members(others(members, &self.own_identity));
        }
        let outgoing = Outgoing {
            key: self.own_keys.current().clone(),
            use_at: Instant::now(),
            recipients: Recipients::AllMembers,
        };
        let all = recipients(&outgoing, &self.view.members, &[]);
        self.send_keys(&outgoing, all).await;
        self.publish(inner);
        Ok(())
    }

    async fn run(
        &mut self,
        inner: &Inner,
        stop: &mut oneshot::Receiver<()>,
        commands: &mut mpsc::Receiver<bool>,
    ) -> Result<(), EndReason> {
        let mut parked: Vec<Candidate> = Vec::new();
        let mut pending_own: Option<(Instant, FrameKeyMsg)> = None;
        let mut seen_self = false;
        self.start_keys(inner).await?;
        loop {
            let due = pending_own.as_ref().map(|(at, _)| *at);
            tokio::select! {
                _ = &mut *stop => return Ok(()),
                command = commands.recv() => match command {
                    Some(muted) => self.send_worker(ToWorker::SetMuted { muted })?,
                    None => return Ok(()),
                },
                event = self.link.events.recv() => {
                    let Some(event) = event else {
                        return Err(EndReason::WorkerStopped);
                    };
                    match self.view.apply(&event) {
                        Applied::Same => {}
                        Applied::Changed => self.publish(inner),
                        Applied::End(reason) => return Err(reason),
                    }
                }
                Some(()) = self.changes.recv() => {
                    self.members_changed(inner, &mut parked, &mut pending_own, &mut seen_self)
                        .await?;
                }
                Some(candidate) = self.incoming.recv() => {
                    self.key_received(inner, candidate, &mut parked)?;
                }
                Some(()) = self.failed.recv() => return Err(EndReason::MembershipLost),
                () = async {
                    match due {
                        Some(at) => time::sleep_until(at.into()).await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some((_, key)) = pending_own.take() {
                        self.send_worker(ToWorker::SetOwnKey { key })?;
                    }
                }
            }
        }
    }

    async fn members_changed(
        &mut self,
        inner: &Inner,
        parked: &mut Vec<Candidate>,
        pending_own: &mut Option<(Instant, FrameKeyMsg)>,
        seen_self: &mut bool,
    ) -> Result<(), EndReason> {
        let Ok(all) = read_members(&self.room, now_ms()).await else {
            return Ok(());
        };
        let has_self = all.iter().any(|m| m.identity == self.own_identity);
        if has_self {
            *seen_self = true;
        } else if *seen_self {
            return Err(EndReason::MembershipLost);
        }
        let current = others(all, &self.own_identity);
        let known: BTreeSet<String> = self
            .view
            .members
            .iter()
            .map(|m| m.identity.clone())
            .collect();
        let diff = diff_members(&known, &current);
        self.view.set_members(current);
        if !diff.left.is_empty() {
            let view = &self.view;
            self.remote.retain_members(|id| view.is_member(id));
            for identity in &diff.left {
                self.send_worker(ToWorker::RemoveParticipant {
                    identity: identity.clone(),
                })?;
            }
        }
        if let Some(outgoing) = key_action(&diff, &mut self.own_keys, Instant::now())
            .map_err(|_| EndReason::KeysUnavailable)?
        {
            let to = recipients(&outgoing, &self.view.members, &diff.joined);
            self.send_keys(&outgoing, to).await;
            // A key already in use is not set again.
            if outgoing.recipients == Recipients::AllMembers {
                *pending_own = Some((outgoing.use_at, worker_key(&outgoing.key)));
            }
        }
        // Keys that came before their sender's membership may fit now.
        for candidate in std::mem::take(parked) {
            self.key_received(inner, candidate, parked)?;
        }
        self.publish(inner);
        Ok(())
    }

    fn key_received(
        &mut self,
        inner: &Inner,
        candidate: Candidate,
        parked: &mut Vec<Candidate>,
    ) -> Result<(), EndReason> {
        let view = &self.view;
        let result = incoming::accept(
            &candidate,
            self.room.room_id(),
            &|id| view.is_member(id),
            &mut self.remote,
        );
        match result {
            Ok(IncomingKey {
                identity,
                index,
                bytes,
            }) => {
                self.view.keyed.insert(identity.clone());
                self.send_worker(ToWorker::SetKey {
                    identity,
                    key: FrameKeyMsg { index, bytes },
                })?;
                self.publish(inner);
            }
            Err(refusal) if refusal.is_early() => {
                parked.retain(|c| c.received.elapsed() < PARK_TIME);
                if parked.len() >= MAX_PARKED {
                    parked.remove(0);
                }
                parked.push(candidate);
            }
            Err(_) => {}
        }
        Ok(())
    }

    /// Ends the call: the keeper, the worker, the membership, the handlers.
    async fn finish(self) {
        let Call {
            client,
            room,
            link,
            delayed,
            state_key,
            handles,
            mut tasks,
            ..
        } = self;
        // No more restarts, so the leave cannot be pushed back.
        tasks.abort_all();
        finish_worker(link).await;
        end_membership(&client, &room, &delayed, &state_key).await;
        for handle in handles {
            client.remove_event_handler(handle);
        }
    }
}

/// The other devices of `members`.
fn others(members: Vec<ActiveMember>, own_identity: &str) -> Vec<ActiveMember> {
    members
        .into_iter()
        .filter(|m| m.identity != own_identity)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calls::worker::{CallWorkerLink, WorkerProcess};
    use matrix_sdk::ruma::{owned_device_id, owned_user_id, room_id};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn member(user: &str, device: &str, sfu: &str) -> ActiveMember {
        let user_id = OwnedUserId::try_from(user).unwrap();
        let device_id = OwnedDeviceId::from(device);
        ActiveMember {
            identity: participant_identity(&user_id, &device_id),
            user_id,
            device_id,
            service_url: Url::parse(sfu).unwrap(),
            joined_ts: 1,
            expires_ts: u64::MAX,
        }
    }

    fn known(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| (*s).to_owned()).collect()
    }

    /// GIVEN known members and a new list WHEN diffed THEN joined and left
    /// are named by identity.
    #[test]
    fn given_a_changed_member_list_then_the_diff_names_joined_and_left() {
        let current = [
            member("@a:example.org", "A", "https://sfu.example.org"),
            member("@c:example.org", "C", "https://sfu.example.org"),
        ];
        let diff = diff_members(&known(&["@a:example.org:A", "@b:example.org:B"]), &current);
        assert_eq!(diff.joined, ["@c:example.org:C"]);
        assert_eq!(diff.left, ["@b:example.org:B"]);
        assert_eq!(
            diff_members(&known(&["@a:example.org:A"]), &current[..1]),
            MemberDiff::default()
        );
    }

    /// GIVEN a young key WHEN one member joins THEN the key goes only to the
    /// new member and is not set again; WHEN one leaves THEN a new key goes
    /// to everyone.
    #[test]
    fn given_joins_and_leaves_then_key_actions_and_recipients_follow() {
        let now = Instant::now();
        let mut own = OwnKeys::new(now).unwrap();
        let members = [
            member("@a:example.org", "A", "https://sfu.example.org"),
            member("@b:example.org", "B", "https://sfu.example.org"),
        ];
        let joined = MemberDiff {
            joined: vec!["@b:example.org:B".to_owned()],
            left: vec![],
        };
        let first = key_action(&joined, &mut own, now).unwrap().unwrap();
        assert_eq!(first.recipients, Recipients::NewMembersOnly);
        assert_eq!(first.key.index, 0);
        let to = recipients(&first, &members, &joined.joined);
        assert_eq!(
            to,
            [(owned_user_id!("@b:example.org"), owned_device_id!("B"))]
        );

        let left = MemberDiff {
            joined: vec!["@b:example.org:B".to_owned()],
            left: vec!["@x:example.org:X".to_owned()],
        };
        let second = key_action(&left, &mut own, now).unwrap().unwrap();
        assert_eq!(second.recipients, Recipients::AllMembers);
        assert_eq!(second.key.index, 1);
        assert_eq!(recipients(&second, &members, &left.joined).len(), 2);

        assert!(
            key_action(&MemberDiff::default(), &mut own, now)
                .unwrap()
                .is_none()
        );
    }

    /// GIVEN an old key WHEN a member joins THEN the key rotates for all.
    #[test]
    fn given_an_old_key_when_a_member_joins_then_it_rotates() {
        let start = Instant::now();
        let mut own = OwnKeys::new(start).unwrap();
        let diff = MemberDiff {
            joined: vec!["@b:example.org:B".to_owned()],
            left: vec![],
        };
        let later = start + Duration::from_secs(60);
        let outgoing = key_action(&diff, &mut own, later).unwrap().unwrap();
        assert_eq!(outgoing.recipients, Recipients::AllMembers);
    }

    fn view() -> View {
        let mut view = View::new(
            room_id!("!r:example.org").to_owned(),
            Url::parse("https://sfu.example.org").unwrap(),
        );
        view.set_members(vec![
            member("@a:example.org", "A", "https://sfu.example.org"),
            member("@b:example.org", "B", "https://other.example.org"),
        ]);
        view
    }

    /// GIVEN a joining call WHEN the worker connects THEN the state is
    /// active, with reachability from each member's SFU.
    #[test]
    fn given_the_worker_connects_then_the_state_is_active() {
        let mut view = view();
        assert!(matches!(view.state(), CallState::Joining { .. }));
        assert_eq!(view.apply(&FromWorker::Connected), Applied::Changed);
        assert_eq!(view.apply(&FromWorker::Connected), Applied::Same);
        let CallState::Active {
            participants,
            muted,
            ..
        } = view.state()
        else {
            panic!("active");
        };
        assert!(!muted);
        assert!(participants[0].reachable && !participants[1].reachable);
        assert!(participants.iter().all(|p| !p.keyed && !p.speaking));
    }

    /// GIVEN speaking reports WHEN they name a member or a stranger THEN only
    /// a member is shown, and a member who leaves stops speaking.
    #[test]
    fn given_speaking_reports_then_only_members_speak() {
        let mut view = view();
        view.apply(&FromWorker::Connected);
        let speak = |id: &str, on| FromWorker::Speaking {
            identity: id.to_owned(),
            speaking: on,
        };
        assert_eq!(
            view.apply(&speak("@evil:example.org:E", true)),
            Applied::Same
        );
        assert_eq!(
            view.apply(&speak("@a:example.org:A", true)),
            Applied::Changed
        );
        let CallState::Active { participants, .. } = view.state() else {
            panic!("active");
        };
        assert!(participants[0].speaking);
        view.set_members(vec![member("@b:example.org", "B", "https://x.example.org")]);
        assert!(view.speaking.is_empty());
    }

    /// GIVEN a worker failure, close or mute report WHEN applied THEN they end
    /// the call or change the mute flag.
    #[test]
    fn given_worker_end_events_then_the_call_ends() {
        let mut view = view();
        assert_eq!(
            view.apply(&FromWorker::Muted { muted: true }),
            Applied::Changed
        );
        assert!(view.muted);
        assert_eq!(
            view.apply(&FromWorker::Failed {
                code: FailureCode::Tls
            }),
            Applied::End(EndReason::WorkerFailed(FailureCode::Tls))
        );
        assert_eq!(
            view.apply(&FromWorker::Left),
            Applied::End(EndReason::WorkerStopped)
        );
    }

    struct FakeProcess(Arc<AtomicBool>);

    impl WorkerProcess for FakeProcess {
        fn kill(&self) {
            self.0.store(true, Ordering::SeqCst);
        }
        fn wait_exit(&self, _: Duration) -> bool {
            true
        }
    }

    struct FakeHost {
        confirm: bool,
        start_error: Option<StartError>,
        confirms: AtomicUsize,
        starts: AtomicUsize,
    }

    impl FakeHost {
        fn new(confirm: bool, start_error: Option<StartError>) -> Arc<Self> {
            Arc::new(Self {
                confirm,
                start_error,
                confirms: AtomicUsize::new(0),
                starts: AtomicUsize::new(0),
            })
        }
    }

    impl CallHost for FakeHost {
        fn start(&self) -> Result<CallWorkerLink, StartError> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = self.start_error {
                return Err(error);
            }
            let (commands, _) = mpsc::channel(4);
            let (_, events) = mpsc::channel(4);
            Ok(CallWorkerLink::from_parts(
                commands,
                events,
                Box::new(FakeProcess(Arc::new(AtomicBool::new(false)))),
            ))
        }

        fn confirm_direct_connection(&self) -> bool {
            self.confirms.fetch_add(1, Ordering::SeqCst);
            self.confirm
        }
    }

    /// GIVEN a profile proxy WHEN the user does not confirm THEN the call is
    /// declined; WHEN there is no proxy THEN nobody is asked.
    #[tokio::test]
    async fn given_a_proxy_without_confirmation_then_the_call_is_declined() {
        let host = FakeHost::new(false, None);
        let dynamic: Arc<dyn CallHost> = host.clone();
        assert_eq!(
            confirm_direct(&dynamic, true).await,
            Err(CallError::Declined)
        );
        assert_eq!(host.confirms.load(Ordering::SeqCst), 1);
        assert_eq!(confirm_direct(&dynamic, false).await, Ok(()));
        assert_eq!(host.confirms.load(Ordering::SeqCst), 1);

        let yes: Arc<dyn CallHost> = FakeHost::new(true, None);
        assert_eq!(confirm_direct(&yes, true).await, Ok(()));
    }

    /// GIVEN a host whose containment fails WHEN the worker starts THEN the
    /// error names the code and no link exists.
    #[tokio::test]
    async fn given_failed_containment_then_no_worker_runs() {
        let host: Arc<dyn CallHost> = FakeHost::new(true, Some(StartError::Containment(4)));
        assert!(matches!(
            start_worker(&host).await,
            Err(CallError::Worker(StartError::Containment(4)))
        ));
        let host: Arc<dyn CallHost> = FakeHost::new(true, None);
        assert!(start_worker(&host).await.is_ok());
    }

    /// GIVEN a call in progress WHEN another join is claimed THEN it is
    /// refused; WHEN the first join fails THEN the slot is free again.
    #[tokio::test]
    async fn given_a_call_in_progress_then_a_second_is_refused() {
        let controller = CallController::new(FakeHost::new(true, None));
        let room = room_id!("!r:example.org");
        let guard = controller.claim(room).expect("first claim");
        assert!(matches!(
            *controller.subscribe().borrow(),
            CallState::Joining { .. }
        ));
        assert_eq!(controller.claim(room).err(), Some(CallError::AlreadyInCall));
        drop(guard);
        assert_eq!(*controller.subscribe().borrow(), CallState::Idle);
        assert!(controller.claim(room).is_ok());
    }

    /// GIVEN no call WHEN leaving or muting THEN the controller answers
    /// NotInCall.
    #[tokio::test]
    async fn given_no_call_then_leave_and_mute_are_refused() {
        let controller = CallController::new(FakeHost::new(true, None));
        assert_eq!(controller.leave().await, Err(CallError::NotInCall));
        assert_eq!(controller.set_muted(true), Err(CallError::NotInCall));
    }
}
