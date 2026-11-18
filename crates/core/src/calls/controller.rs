//! The call controller: one MatrixRTC call at a time (ADR-0043, ADR-0048).
//!
//! The controller does the Matrix side of a call and drives the contained
//! call worker. Joining fails closed at every step and undoes the earlier
//! steps; see [`CallController::join`] for the order. While a call runs, one
//! task owns the worker link, the frame keys and the call state. The page
//! gets only [`CallState`]: no key, token, URL or peer text.
//!
//! Session lifecycle (docs/architecture/sdk-lifecycle.md): the join runs
//! under the one lease its caller holds. A running call holds no client. Each
//! of its SDK operations (a membership read, a key send, a delayed-event
//! restart, the leave) takes a lease and a client from the session's
//! [`Admission`] and releases both when it ends. When admission fails the call
//! ends at once: the worker is killed and no Matrix leave is sent, so the
//! delayed leave ends the membership. [`CallController::stop_for_session_end`]
//! does the same for a lock, a sign-out or any other end of the session.

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
use super::video::{Notifier, Picture, VideoStore};
use super::worker::{CallHost, CallWorkerLink, StartError, WorkerProcess};
use super::{MAX_CALL_MEMBERS, keys::FrameKey};
use crate::session::Admission;
use matrix_sdk::{
    Client, RoomState,
    event_handler::EventHandlerHandle,
    ruma::{
        OwnedDeviceId, OwnedRoomId, OwnedUserId, RoomId,
        api::client::account::request_openid_token, events::AnySyncStateEvent, serde::Raw,
    },
};
use persephone_call_protocol::{
    CameraState, FailureCode, FrameKeyMsg, FromWorker, ToWorker, TurnServer, VIDEO_TILES,
};
use persephone_contracts::{CallCamera, CallParticipantRef, CallVideoTile};
use std::{
    collections::{BTreeSet, HashSet},
    future::Future,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    runtime::Handle,
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
    /// The saved call devices could not be read. The call never uses the
    /// defaults instead.
    DeviceSettingUnreadable,
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
    /// `leave` ended this join before the call started.
    Cancelled,
    /// The session stopped admitting work during the join.
    SessionEnded,
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
    /// The session locked, signed out or otherwise stopped admitting work.
    SessionEnded,
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
        camera: CallCamera,
        /// The active video tiles, at most [`VIDEO_TILES`].
        video: Vec<CallVideoTile>,
    },
    Ended {
        room_id: OwnedRoomId,
        reason: EndReason,
    },
}

/// The saved call devices. A record that cannot be read refuses the call.
pub async fn device_selection(
    stores: &crate::store::Stores,
) -> Result<super::devices::Selection, CallError> {
    super::devices::load(stores)
        .await
        .map_err(|_| CallError::DeviceSettingUnreadable)
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
    camera: CallCamera,
    /// The identity on each remote tile (tile 1 first), as the worker said
    /// with `VideoStarted`. An identity that is not a member yet waits here
    /// and shows once the membership is read.
    remote_video: [Option<String>; REMOTE_TILES],
}

/// Remote video tiles: tiles 1 to 4.
const REMOTE_TILES: usize = VIDEO_TILES as usize - 1;

impl View {
    /// `video` is true when the call is joined with the camera on: the
    /// camera is `starting` until the worker says how it went.
    fn new(room_id: OwnedRoomId, own_sfu: Url, video: bool) -> Self {
        Self {
            room_id,
            own_sfu,
            connected: false,
            muted: false,
            members: Vec::new(),
            speaking: HashSet::new(),
            keyed: HashSet::new(),
            camera: if video {
                CallCamera::Starting
            } else {
                CallCamera::Off
            },
            remote_video: Default::default(),
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

    /// The page asked for the camera on or off. The state shows `starting`
    /// until the worker answers.
    fn request_camera(&mut self, on: bool) -> Applied {
        let next = match (on, self.camera) {
            (true, CallCamera::On | CallCamera::Starting) => self.camera,
            (true, _) => CallCamera::Starting,
            (false, _) => CallCamera::Off,
        };
        self.set_camera(next)
    }

    fn set_camera(&mut self, camera: CallCamera) -> Applied {
        if self.camera == camera {
            return Applied::Same;
        }
        self.camera = camera;
        Applied::Changed
    }

    /// Forgets the video of a member that left.
    fn forget_video(&mut self, identity: &str) -> bool {
        let mut changed = false;
        for slot in &mut self.remote_video {
            if slot.as_deref() == Some(identity) {
                *slot = None;
                changed = true;
            }
        }
        changed
    }

    /// Which tiles may keep a picture: the own tile while the camera is on,
    /// a remote tile while its identity is a current member.
    fn video_allowed(&self) -> (bool, [bool; REMOTE_TILES]) {
        let remote = std::array::from_fn(|i| {
            self.remote_video[i]
                .as_deref()
                .is_some_and(|identity| self.is_member(identity))
        });
        (self.camera == CallCamera::On, remote)
    }

    /// The video tiles the page shows: the own camera while it is on, and
    /// each remote tile whose identity is a current member.
    fn video_tiles(&self) -> Vec<CallVideoTile> {
        let (own, _) = self.video_allowed();
        let mut tiles = Vec::with_capacity(VIDEO_TILES as usize);
        if own {
            tiles.push(CallVideoTile {
                tile: 0,
                participant: None,
                own: true,
            });
        }
        for (index, identity) in self.remote_video.iter().enumerate() {
            let Some(member) = identity
                .as_deref()
                .and_then(|identity| self.members.iter().find(|m| m.identity == identity))
            else {
                continue;
            };
            tiles.push(CallVideoTile {
                tile: index as u8 + 1,
                participant: Some(CallParticipantRef {
                    user_id: member.user_id.to_string(),
                    device_id: member.device_id.to_string(),
                }),
                own: false,
            });
        }
        tiles
    }

    fn apply(&mut self, event: &FromWorker) -> Applied {
        match event {
            FromWorker::Ready { .. } | FromWorker::ParticipantJoined { .. } => Applied::Same,
            FromWorker::Camera { state } => self.set_camera(match state {
                CameraState::Off => CallCamera::Off,
                CameraState::On => CallCamera::On,
                CameraState::NotFound => CallCamera::NotFound,
                CameraState::Unavailable => CallCamera::Unavailable,
            }),
            FromWorker::VideoStarted { tile, identity } => {
                let Some(slot) = usize::from(*tile)
                    .checked_sub(1)
                    .and_then(|index| self.remote_video.get_mut(index))
                else {
                    return Applied::Same;
                };
                if slot.as_deref() == Some(identity.as_str()) {
                    return Applied::Same;
                }
                *slot = Some(identity.clone());
                Applied::Changed
            }
            FromWorker::VideoStopped { tile } => {
                let Some(slot) = usize::from(*tile)
                    .checked_sub(1)
                    .and_then(|index| self.remote_video.get_mut(index))
                else {
                    // Tile 0 follows the camera state, which has its own message.
                    return Applied::Same;
                };
                if slot.take().is_some() {
                    Applied::Changed
                } else {
                    Applied::Same
                }
            }
            // The reader turns a picture announcement into a picture; the
            // bare message is a protocol violation.
            FromWorker::VideoFrame { .. } => {
                Applied::End(EndReason::WorkerFailed(FailureCode::Protocol))
            }
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
                let video = self.forget_video(identity);
                if self.speaking.remove(identity) || video {
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
            // The device list answers `ListDevices` only; in a call it is a
            // protocol violation.
            FromWorker::Devices { .. } => {
                Applied::End(EndReason::WorkerFailed(FailureCode::Protocol))
            }
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
            camera: self.camera,
            video: self.video_tiles(),
        }
    }
}

pub(super) fn now_ms() -> u64 {
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
async fn end_membership(
    client: &Client,
    room_id: &RoomId,
    delayed: &DelayedLeave,
    state_key: &str,
) {
    if matches!(
        time::timeout(SEND_TIMEOUT, delayed.send_now(client)).await,
        Ok(Ok(()))
    ) {
        return;
    }
    if let Some(room) = client.get_room(room_id) {
        let _ = time::timeout(
            SEND_TIMEOUT,
            room.send_state_event_raw(EVENT_TYPE, state_key, leave_content()),
        )
        .await;
    }
}

fn plain<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned lock only means a task panicked; the data stays plain.
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------
// Stopping and SDK access.

/// What stops the SDK work of one call or join.
#[derive(Default)]
struct Flags {
    /// `leave` asked to end a join in progress.
    cancelled: AtomicBool,
    /// The session stopped admitting work.
    ended: AtomicBool,
}

impl Flags {
    fn ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst)
    }

    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// `Err` once the join must stop.
    fn halt(&self) -> Result<(), CallError> {
        if self.ended() {
            Err(CallError::SessionEnded)
        } else if self.cancelled() {
            Err(CallError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// No lease can be taken: the call must end.
struct Gone;

/// How a running call reaches the SDK: one lease and client per complete
/// operation, never longer.
#[derive(Clone)]
struct Access {
    admit: Admission,
    /// The session generation the call started in.
    generation: u64,
    flags: Arc<Flags>,
}

impl Access {
    /// Runs one complete SDK operation under a fresh lease. `work` gets its
    /// own client clone and drops it, and every `Room` it took, before it
    /// ends. Refused, without running `work`, once the call is stopped, the
    /// session stops admitting work or it is no longer the session the call
    /// started in.
    async fn run<T, Fut, F>(&self, work: F) -> Result<T, Gone>
    where
        F: FnOnce(Client) -> Fut + Send,
        Fut: Future<Output = T> + Send,
    {
        if self.flags.ended() {
            return Err(Gone);
        }
        let Some((lease, client)) = (self.admit)() else {
            return Err(Gone);
        };
        let result = if lease.generation() == self.generation && !self.flags.ended() {
            Some(work(client.clone()).await)
        } else {
            None
        };
        {
            let _context = lease.stores().runtime().enter();
            drop(client);
        }
        drop(lease);
        result.ok_or(Gone)
    }
}

/// The user's yes to a direct connection around the profile's proxy, or the
/// fact that the profile has no proxy. Only [`CallController::confirm_direct`]
/// makes one, so a join cannot skip the question.
#[derive(Debug)]
pub struct DirectConsent(());

/// The session a join runs in: the generation and the runtime of the lease
/// its caller holds. The call's tasks run on that runtime.
pub struct JoinScope {
    pub generation: u64,
    pub runtime: Handle,
    /// The saved microphone, speaker and camera, read with the proxy setting.
    pub devices: super::devices::Selection,
    /// Join with the camera on, and with the `video` call intent (ADR-0048).
    pub video: bool,
}

// ---------------------------------------------------------------------
// The controller.

enum Slot {
    Idle,
    /// A join is in progress.
    Joining(Joining),
    Running(RunHandle),
}

struct Joining {
    id: u64,
    room_id: OwnedRoomId,
    flags: Arc<Flags>,
    /// The worker, once it runs, so the end of the session can kill it.
    kill: Arc<Mutex<Option<Arc<dyn WorkerProcess>>>>,
}

struct RunHandle {
    id: u64,
    room_id: OwnedRoomId,
    flags: Arc<Flags>,
    /// Ends the worker from outside the call task.
    kill: Arc<dyn WorkerProcess>,
    stop: Option<oneshot::Sender<()>>,
    commands: mpsc::Sender<Command>,
    /// The user's latest camera request, shared with the call task.
    camera_wanted: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
}

/// What the page asks of a running call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Mute(bool),
    /// A camera request and its number from [`VideoStore::camera_request`]
    /// (0 when the call has no video store).
    Camera {
        on: bool,
        request: u64,
    },
}

/// Tells the owner of the controller that the state changed.
type Listener = Arc<dyn Fn() + Send + Sync>;

struct Inner {
    host: Arc<dyn CallHost>,
    state: watch::Sender<CallState>,
    slot: Mutex<Slot>,
    admission: Mutex<Admission>,
    listener: Mutex<Option<Listener>>,
    /// Tells the owner that a video picture arrived; see
    /// [`CallController::set_video_listener`].
    video_listener: Mutex<Option<Notifier>>,
    /// The picture store of the running worker, from its start to the end of
    /// the call or join.
    video: Mutex<Option<VideoStore>>,
    next_id: AtomicU64,
}

impl Inner {
    /// Ends the picture store: every picture goes and no later one is kept.
    fn close_video(&self) {
        let store = plain(&self.video).take();
        if let Some(store) = store {
            store.close();
        }
    }

    fn slot(&self) -> MutexGuard<'_, Slot> {
        plain(&self.slot)
    }

    fn admission(&self) -> Admission {
        plain(&self.admission).clone()
    }

    fn publish(&self, state: CallState) {
        let changed = self.state.send_if_modified(|current| {
            if *current == state {
                false
            } else {
                *current = state;
                true
            }
        });
        if changed {
            let listener = plain(&self.listener).clone();
            if let Some(listener) = listener {
                listener();
            }
        }
    }

    /// Frees the slot if call or join `id` still holds it. `false` when the
    /// end of the session already took it.
    fn release(&self, id: u64) -> bool {
        let mut slot = self.slot();
        let mine = match &*slot {
            Slot::Idle => false,
            Slot::Joining(join) => join.id == id,
            Slot::Running(run) => run.id == id,
        };
        if mine {
            *slot = Slot::Idle;
            // Under the slot lock, so a later join cannot meet this close.
            self.close_video();
        }
        mine
    }
}

/// Frees the slot when a join ends without a running call, also when the
/// join future is dropped. A dropped worker link is killed; a scheduled
/// delayed leave then fires on its own.
struct JoinGuard {
    inner: Arc<Inner>,
    id: u64,
    room_id: OwnedRoomId,
    flags: Arc<Flags>,
    kill: Arc<Mutex<Option<Arc<dyn WorkerProcess>>>>,
    armed: bool,
}

impl Drop for JoinGuard {
    fn drop(&mut self) {
        if !self.armed || !self.inner.release(self.id) {
            return;
        }
        if self.flags.ended() {
            // The end of the session already published the state.
        } else if self.flags.cancelled() {
            self.inner.publish(CallState::Ended {
                room_id: self.room_id.clone(),
                reason: EndReason::Left,
            });
        } else {
            self.inner.publish(CallState::Idle);
        }
    }
}

/// The one call of this core.
pub struct CallController {
    inner: Arc<Inner>,
}

impl Drop for CallController {
    fn drop(&mut self) {
        self.stop_for_session_end();
    }
}

impl CallController {
    /// A controller whose call tasks take their SDK access from `admission`.
    pub fn new(host: Arc<dyn CallHost>, admission: Admission) -> Self {
        let (state, _) = watch::channel(CallState::Idle);
        Self {
            inner: Arc::new(Inner {
                host,
                state,
                slot: Mutex::new(Slot::Idle),
                admission: Mutex::new(admission),
                listener: Mutex::new(None),
                video_listener: Mutex::new(None),
                video: Mutex::new(None),
                next_id: AtomicU64::new(0),
            }),
        }
    }

    /// Replaces the admission, when the core gets its session.
    pub fn set_admission(&self, admission: Admission) {
        *plain(&self.inner.admission) = admission;
    }

    /// Calls `listener` after every change of the state, on the thread that
    /// changed it. It must not block and must not call the controller.
    pub fn set_listener(&self, listener: Listener) {
        *plain(&self.inner.listener) = Some(listener);
    }

    /// Calls `listener` after each kept video picture, on the worker read
    /// thread. It must not block and must not call the controller. Pictures
    /// are not part of the call state: fetch them with
    /// [`CallController::video_picture`].
    pub fn set_video_listener(&self, listener: Notifier) {
        *plain(&self.inner.video_listener) = Some(listener);
    }

    /// The newest picture on `tile` with a number above `after`, if any. It
    /// holds the store lock only to copy a pointer.
    pub fn video_picture(&self, tile: u8, after: u64) -> Option<Picture> {
        let store = plain(&self.inner.video).clone();
        store?.newest(tile, after)
    }

    /// The content-free call state.
    pub fn subscribe(&self) -> watch::Receiver<CallState> {
        self.inner.state.subscribe()
    }

    /// The call state now.
    pub fn state(&self) -> CallState {
        self.inner.state.borrow().clone()
    }

    /// Whether this platform has reviewed call containment.
    pub fn supported(&self) -> bool {
        self.inner.host.supported()
    }

    /// The platform's call host, for the device list.
    pub fn host(&self) -> Arc<dyn CallHost> {
        self.inner.host.clone()
    }

    /// Asks, when the profile has a proxy, whether the call may connect
    /// directly (ADR-0043, ADR-0028). Anything but an explicit yes declines.
    /// Blocks until answered, so call it from a thread that holds no lease:
    /// a lock must not wait for a dialog. Refused first while a call or
    /// join holds the controller.
    pub fn confirm_direct(&self, proxy_configured: bool) -> Result<DirectConsent, CallError> {
        if !matches!(*self.inner.slot(), Slot::Idle) {
            return Err(CallError::AlreadyInCall);
        }
        if !proxy_configured || self.inner.host.confirm_direct_connection() {
            Ok(DirectConsent(()))
        } else {
            Err(CallError::Declined)
        }
    }

    fn claim(&self, room_id: &RoomId) -> Result<JoinGuard, CallError> {
        let mut slot = self.inner.slot();
        if !matches!(*slot, Slot::Idle) {
            return Err(CallError::AlreadyInCall);
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let flags = Arc::new(Flags::default());
        let kill = Arc::new(Mutex::new(None));
        *slot = Slot::Joining(Joining {
            id,
            room_id: room_id.to_owned(),
            flags: flags.clone(),
            kill: kill.clone(),
        });
        drop(slot);
        self.inner.publish(CallState::Joining {
            room_id: room_id.to_owned(),
        });
        Ok(JoinGuard {
            inner: self.inner.clone(),
            id,
            room_id: room_id.to_owned(),
            flags,
            kill,
            armed: true,
        })
    }

    /// Joins the call in `room_id`. `client` and `scope` come from the one
    /// lease the caller holds for the whole join. `consent` is from
    /// [`CallController::confirm_direct`].
    ///
    /// Each step fails closed and undoes the earlier ones. [`leave`] or the
    /// end of the session stops the join between steps:
    /// 1. one call at a time;
    /// 2. the room is joined and encrypted;
    /// 3. the homeserver offers an SFU (TURN servers are optional);
    /// 4. the contained worker starts, before any Matrix side effect;
    /// 5. the delayed leave is scheduled (MSC4140) and restarted from now;
    /// 6. own membership is written;
    /// 7. an OpenID token buys an SFU grant;
    /// 8. frame keys are made and the worker joins. The task then sends the
    ///    key to the members that are there.
    ///
    /// [`leave`]: CallController::leave
    pub async fn join(
        &self,
        scope: JoinScope,
        client: &Client,
        room_id: &RoomId,
        consent: DirectConsent,
    ) -> Result<(), CallError> {
        let DirectConsent(()) = consent;
        let mut guard = self.claim(room_id)?;
        let access = Access {
            admit: self.inner.admission(),
            generation: scope.generation,
            flags: guard.flags.clone(),
        };
        let mut setup = Setup::new(client.clone(), guard.flags.clone());
        let prepared = self
            .prepare(&scope, client, room_id, &access, &mut setup, &guard.kill)
            .await;
        let call = match prepared {
            Ok(call) => call,
            Err(error) => {
                setup.undo().await;
                return Err(error);
            }
        };
        let (stop, stop_rx) = oneshot::channel();
        let (commands, command_rx) = mpsc::channel(COMMAND_QUEUE);
        let kill = call.link.kill_handle();
        let camera_wanted = call.camera_wanted.clone();
        let mut call = Some(call);
        let halted = {
            // `leave` and the end of the session set their flag under this
            // lock, so a stop is either seen here or sees the running call.
            let mut slot = self.inner.slot();
            match guard.flags.halt() {
                Ok(()) => {
                    let call = call.take().expect("the call is started once");
                    let task = scope.runtime.spawn(run_call(
                        self.inner.clone(),
                        guard.id,
                        guard.flags.clone(),
                        call,
                        stop_rx,
                        command_rx,
                    ));
                    *slot = Slot::Running(RunHandle {
                        id: guard.id,
                        room_id: room_id.to_owned(),
                        flags: guard.flags.clone(),
                        kill,
                        stop: Some(stop),
                        commands,
                        camera_wanted,
                        task: Some(task),
                    });
                    guard.armed = false;
                    None
                }
                Err(error) => Some(error),
            }
        };
        if let (Some(error), Some(call)) = (halted, call) {
            let ended = guard.flags.ended();
            call.finish(ended).await;
            return Err(error);
        }
        Ok(())
    }

    async fn prepare(
        &self,
        scope: &JoinScope,
        client: &Client,
        room_id: &RoomId,
        access: &Access,
        setup: &mut Setup,
        kill: &Mutex<Option<Arc<dyn WorkerProcess>>>,
    ) -> Result<Call, CallError> {
        let flags = &access.flags;
        flags.halt()?;
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
        flags.halt()?;

        // 3. An SFU; TURN is optional.
        let sfu = match sfu_transport(client).await {
            Ok(Some(service)) => service,
            _ => return Err(CallError::NoSfu),
        };
        let turn = turn_servers(client).await.ok().flatten();
        flags.halt()?;

        // 4. The worker, before any Matrix side effect.
        let link = start_worker(&self.inner.host).await?;
        *plain(kill) = Some(link.kill_handle());
        // Pictures go to the store from now on; the store ends with the call.
        let store = link.video();
        if let Some(notifier) = plain(&self.inner.video_listener).clone() {
            store.set_notifier(notifier);
        }
        *plain(&self.inner.video) = Some(store);
        setup.link = Some(link);
        flags.halt()?;

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
            .push(incoming::register(client, room_id.to_owned(), key_tx));

        // 5. The delayed leave, restarted from now on.
        let state_key = own_state_key(&user_id, &device_id);
        let delayed = DelayedLeave::schedule(client, room_id, &state_key)
            .await
            .map_err(|_| CallError::NoDelayedEvents)?;
        setup.delayed = Some(delayed.clone());
        flags.halt()?;
        let created_ts = now_ms();
        setup.tasks.spawn_on(
            keeper(
                access.clone(),
                delayed.clone(),
                KeeperMembership {
                    room_id: room_id.to_owned(),
                    state_key: state_key.clone(),
                    user_id: user_id.clone(),
                    device_id: device_id.clone(),
                    sfu_url: sfu.advertised.clone(),
                    created_ts,
                    video: scope.video,
                },
                failed_tx,
            ),
            &scope.runtime,
        );

        // 6. Own membership.
        let content = own_membership(
            room_id.as_str(),
            &device_id,
            &user_id,
            &sfu.advertised,
            Some(created_ts),
            scope.video,
        );
        setup.membership_sent = true;
        time::timeout(
            SEND_TIMEOUT,
            room.send_state_event_raw(EVENT_TYPE, &state_key, content),
        )
        .await
        .map_err(|_| CallError::Membership)?
        .map_err(|_| CallError::Membership)?;
        flags.halt()?;

        // 7. The SFU grant.
        let openid = client
            .send(request_openid_token::v3::Request::new(user_id.clone()))
            .await
            .map_err(|_| CallError::OpenId)?;
        let body = sfu::request_body(room_id, &device_id, &OpenIdToken::from(openid));
        let grant = sfu::get_grant(client.http_client(), &sfu.url, &body)
            .await
            .map_err(CallError::Sfu)?;
        flags.halt()?;

        // 8. Keys, then the worker joins.
        let own_keys = OwnKeys::new(Instant::now()).map_err(|_| CallError::Keys)?;
        let own_identity = participant_identity(&user_id, &device_id);
        let link = setup.link.take().ok_or(CallError::Keys)?;
        // On the portal path the worker needs the PipeWire remote before its
        // camera opens. The call task asks the portal first and then turns
        // the camera on (`Call::start_portal`), so the join starts it off.
        let portal_camera = uses_portal(&link);
        let join = ToWorker::Join {
            sfu_url: grant.url.to_string(),
            jwt: grant.jwt.clone(),
            turn: turn_for_worker(turn),
            relay_only: false,
            own_identity: own_identity.clone(),
            own_key: worker_key(own_keys.current()),
            recording_device: scope.devices.recording.clone(),
            playout_device: scope.devices.playout.clone(),
            camera_device: scope.devices.camera.clone(),
            camera_on: scope.video && !portal_camera,
        };
        if link.send(join).is_err() {
            setup.link = Some(link);
            return Err(CallError::Worker(StartError::NotReady));
        }
        Ok(Call {
            access: access.clone(),
            link,
            delayed,
            state_key,
            own_identity,
            own_device: device_id,
            own_keys,
            remote: RemoteKeys::new(),
            view: View::new(room_id.to_owned(), sfu.url, scope.video),
            changes,
            incoming,
            failed,
            handles: std::mem::take(&mut setup.handles),
            tasks: std::mem::take(&mut setup.tasks),
            camera_wanted: Arc::new(AtomicBool::new(scope.video)),
            portal: None,
            portal_capture: false,
            portal_sent: false,
        })
    }

    /// Leaves the call: the worker ends, the leave is written, the handlers
    /// go. Returns when the call is over. During a join it cancels the join,
    /// which then undoes its steps; the call state shows the end.
    pub async fn leave(&self) -> Result<(), CallError> {
        let (stop, task) = {
            let mut slot = self.inner.slot();
            match &mut *slot {
                Slot::Joining(join) => {
                    join.flags.cancelled.store(true, Ordering::SeqCst);
                    return Ok(());
                }
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
                .try_send(Command::Mute(muted))
                .map_err(|_| CallError::NotInCall),
            _ => Err(CallError::NotInCall),
        }
    }

    /// Turns the camera on or off. Refused unless a call is connected. The
    /// state shows `starting` at once when turned on, and the worker's answer
    /// replaces it. Turning the camera off drops the own picture at once.
    pub fn set_camera(&self, on: bool) -> Result<(), CallError> {
        let slot = self.inner.slot();
        let Slot::Running(run) = &*slot else {
            return Err(CallError::NotInCall);
        };
        if !matches!(*self.inner.state.borrow(), CallState::Active { .. }) {
            return Err(CallError::NotInCall);
        }
        // A queue place is taken first, so a full queue changes nothing. The
        // pending state is published before the command is queued, so the
        // task's answer (which may be quick) always comes after it.
        let permit = run
            .commands
            .try_reserve()
            .map_err(|_| CallError::NotInCall)?;
        // Numbered under the store's lock. An off stops the own picture at
        // once, before the task takes the command; only a later camera-on
        // lets a sync allow it again.
        let request = plain(&self.inner.video)
            .clone()
            .map_or(0, |store| store.camera_request(on));
        // The task reads this when a portal request ends, so an off that is
        // accepted here cancels the request even before the task takes it.
        run.camera_wanted.store(on, Ordering::SeqCst);
        let changed = self.inner.state.send_if_modified(|state| {
            let CallState::Active { camera, video, .. } = state else {
                return false;
            };
            let next = match (on, *camera) {
                (true, CallCamera::On | CallCamera::Starting) => *camera,
                (true, _) => CallCamera::Starting,
                (false, _) => CallCamera::Off,
            };
            if next == *camera {
                return false;
            }
            *camera = next;
            video.retain(|tile| !tile.own || next == CallCamera::On);
            true
        });
        permit.send(Command::Camera { on, request });
        drop(slot);
        if changed {
            let listener = plain(&self.inner.listener).clone();
            if let Some(listener) = listener {
                listener();
            }
        }
        Ok(())
    }

    /// The session stops admitting work (lock, sign-out, an ended session,
    /// exit). Call it first in every such path. It does not block: the worker
    /// is killed now, so audio stops; every call task stops after the
    /// operation it is in and starts no new SDK work; a join stops at its
    /// next step; and the state becomes `Ended { SessionEnded }`. No Matrix
    /// leave is sent. The call's event handlers do nothing once its task
    /// ends, and the session's client goes with them; the delayed leave ends
    /// the membership within seconds.
    pub fn stop_for_session_end(&self) {
        let (room_id, kill) = {
            let mut slot = self.inner.slot();
            match std::mem::replace(&mut *slot, Slot::Idle) {
                Slot::Idle => return,
                Slot::Joining(join) => {
                    // The flag is set under the lock a join takes to start
                    // its call, so the join sees it or this sees its call.
                    join.flags.ended.store(true, Ordering::SeqCst);
                    let kill = plain(&join.kill).clone();
                    let room_id = join.room_id.clone();
                    *slot = Slot::Joining(join);
                    (room_id, kill)
                }
                Slot::Running(run) => {
                    run.flags.ended.store(true, Ordering::SeqCst);
                    if let Some(stop) = run.stop {
                        let _ = stop.send(());
                    }
                    (run.room_id, Some(run.kill))
                }
            }
        };
        if let Some(kill) = kill {
            kill.kill();
        }
        self.inner.close_video();
        self.inner.publish(CallState::Ended {
            room_id,
            reason: EndReason::SessionEnded,
        });
    }
}

/// What a join has made so far, so a failure can undo it.
struct Setup {
    client: Client,
    flags: Arc<Flags>,
    link: Option<CallWorkerLink>,
    delayed: Option<DelayedLeave>,
    membership_sent: bool,
    handles: Vec<EventHandlerHandle>,
    tasks: JoinSet<()>,
}

impl Setup {
    fn new(client: Client, flags: Arc<Flags>) -> Self {
        Self {
            client,
            flags,
            link: None,
            delayed: None,
            membership_sent: false,
            handles: Vec::new(),
            tasks: JoinSet::new(),
        }
    }

    /// Runs under the join's own lease. At the end of the session the worker
    /// is killed and no Matrix request is sent: the delayed leave, if it was
    /// scheduled, ends the membership.
    async fn undo(mut self) {
        let ended = self.flags.ended();
        self.tasks.abort_all();
        for handle in self.handles.drain(..) {
            self.client.remove_event_handler(handle);
        }
        if let Some(link) = self.link.take() {
            if ended {
                drop(link);
            } else {
                finish_worker(link).await;
            }
        }
        if ended {
            return;
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
    room_id: OwnedRoomId,
    state_key: String,
    user_id: OwnedUserId,
    device_id: OwnedDeviceId,
    /// As advertised (`SfuService::advertised`).
    sfu_url: String,
    created_ts: u64,
    /// The `m.call.intent` of the join; a refresh keeps it.
    video: bool,
}

/// Restarts the delayed leave and refreshes the membership while the call
/// runs, one lease per operation. It sends on `failed` when the restart
/// failed twice in a row, or when no lease can be had.
async fn keeper(
    access: Access,
    delayed: DelayedLeave,
    membership: KeeperMembership,
    failed: mpsc::Sender<EndReason>,
) {
    let mut beat = time::interval(DELAYED_LEAVE_RESTART);
    beat.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    let mut refresh = time::interval_at(
        time::Instant::now() + MEMBERSHIP_REFRESH,
        MEMBERSHIP_REFRESH,
    );
    refresh.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    let mut health = RestartHealth::default();
    let membership = Arc::new(membership);
    loop {
        tokio::select! {
            _ = beat.tick() => {
                let delayed = delayed.clone();
                let restarted = access
                    .run(|client| async move {
                        matches!(
                            time::timeout(RESTART_TIMEOUT, delayed.restart(&client)).await,
                            Ok(Ok(()))
                        )
                    })
                    .await;
                let Ok(ok) = restarted else {
                    let _ = failed.send(EndReason::SessionEnded).await;
                    return;
                };
                if health.record(ok) {
                    let _ = failed.send(EndReason::MembershipLost).await;
                    return;
                }
            }
            _ = refresh.tick() => {
                let membership = membership.clone();
                let sent = access
                    .run(|client| async move {
                        let Some(room) = client.get_room(&membership.room_id) else {
                            return;
                        };
                        let content = own_membership(
                            membership.room_id.as_str(),
                            &membership.device_id,
                            &membership.user_id,
                            &membership.sfu_url,
                            Some(membership.created_ts),
                            membership.video,
                        );
                        let _ = time::timeout(
                            SEND_TIMEOUT,
                            room.send_state_event_raw(EVENT_TYPE, &membership.state_key, content),
                        )
                        .await;
                    })
                    .await;
                if sent.is_err() {
                    let _ = failed.send(EndReason::SessionEnded).await;
                    return;
                }
            }
        }
    }
}

/// Everything one running call owns. It holds no client.
struct Call {
    access: Access,
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
    failed: mpsc::Receiver<EndReason>,
    handles: Vec<EventHandlerHandle>,
    tasks: JoinSet<()>,
    /// The user's latest camera request. `set_camera` writes it before it
    /// queues the command; the task only reads it.
    camera_wanted: Arc<AtomicBool>,
    /// The camera portal access request in flight (Linux portal path).
    portal: Option<JoinHandle<PortalRemote>>,
    /// A portal remote went to the worker for a capture that may still run.
    /// Kept apart from the shown camera state, which a late worker reply can
    /// set back to `On` after an off: only a camera-on while this holds may
    /// go to the worker without a new portal request.
    portal_capture: bool,
    /// A portal remote went to the worker after the last camera-off command.
    /// A worker `On` then belongs to a capture over that remote and sets
    /// `portal_capture` again: a reply to the old capture (its `Off`) can
    /// arrive after the new remote went and clear the flag.
    portal_sent: bool,
}

/// The result of a camera portal access request: the PipeWire remote, or
/// nothing after a refusal or a failure.
#[cfg(target_os = "linux")]
type PortalRemote = Option<std::os::fd::OwnedFd>;
#[cfg(not(target_os = "linux"))]
type PortalRemote = ();

/// Whether the worker gets its camera through the camera portal (ADR-0049).
fn uses_portal(link: &CallWorkerLink) -> bool {
    link.camera_path() == super::worker::CameraPath::Portal
}

/// Waits for the portal request in `slot`, if any; otherwise it never ends.
/// The request stays in `slot` when the wait is dropped, so the call loop
/// can wait for it again.
async fn portal_done(slot: &mut Option<JoinHandle<PortalRemote>>) -> PortalRemote {
    let Some(task) = slot.as_mut() else {
        return std::future::pending().await;
    };
    let result = task.await;
    *slot = None;
    result.unwrap_or_default()
}

/// Frees the slot when the task ends, also after a panic.
struct EndGuard {
    inner: Arc<Inner>,
    id: u64,
    room_id: OwnedRoomId,
    armed: bool,
}

impl Drop for EndGuard {
    fn drop(&mut self) {
        if !self.armed || !self.inner.release(self.id) {
            return;
        }
        let ended = matches!(*self.inner.state.borrow(), CallState::Ended { .. });
        if !ended {
            self.inner.publish(CallState::Ended {
                room_id: self.room_id.clone(),
                reason: EndReason::Internal,
            });
        }
    }
}

async fn run_call(
    inner: Arc<Inner>,
    id: u64,
    flags: Arc<Flags>,
    mut call: Call,
    mut stop: oneshot::Receiver<()>,
    mut commands: mpsc::Receiver<Command>,
) {
    let room_id = call.view.room_id.clone();
    let mut guard = EndGuard {
        inner: inner.clone(),
        id,
        room_id: room_id.clone(),
        armed: true,
    };
    let reason = match call.run(&inner, &mut stop, &mut commands).await {
        Ok(()) => EndReason::Left,
        Err(reason) => reason,
    };
    if reason == EndReason::SessionEnded {
        flags.ended.store(true, Ordering::SeqCst);
    }
    call.finish(flags.ended()).await;
    guard.armed = false;
    // After the end of the session the slot is free and the state shows it.
    if inner.release(id) {
        inner.publish(CallState::Ended { room_id, reason });
    }
}

impl Call {
    /// Turns the camera on or off as the user asked. On the portal path
    /// (Linux), a camera that is not on or starting first asks the camera
    /// portal for access; [`Call::portal_finished`] then sends the remote and
    /// turns the camera on. The request runs off the call loop, which goes on
    /// with mute, keys and events while the desktop asks the user.
    fn camera_command(&mut self, inner: &Inner, on: bool, request: u64) -> Result<(), EndReason> {
        if on {
            // An earlier `set_camera(false)` blocked the own tile. Only the
            // latest request lifts the block, so this camera-on does nothing
            // there if an off was accepted after it.
            self.link.video().camera_on_taken(request);
        }
        if self.portal_needed(on) {
            self.start_portal();
        } else {
            if !on {
                // The worker stops the capture and closes its remote.
                self.portal_capture = false;
                self.portal_sent = false;
            }
            self.send_worker(ToWorker::SetCamera { on })?;
        }
        let changed = self.view.request_camera(on) == Applied::Changed;
        // A camera-on may have lifted the own-tile block above while the
        // view is already on (the worker reported `On` first), so the store
        // is synced also when the view did not change.
        if changed || on {
            self.sync_video();
        }
        if changed {
            self.publish(inner);
        }
        Ok(())
    }

    /// Whether a camera-on goes through the portal first: on the portal path,
    /// unless a remote already went to the worker for a capture that may
    /// still run (a repeated on then needs no new request). While a request
    /// is in flight, the on joins it.
    fn portal_needed(&self, on: bool) -> bool {
        on && uses_portal(&self.link) && (!self.portal_capture || self.portal.is_some())
    }

    /// Starts a camera portal access request unless one is in flight. The
    /// wait for the user runs off the runtime threads, up to 60 seconds.
    fn start_portal(&mut self) {
        #[cfg(target_os = "linux")]
        if self.portal.is_none() {
            use crate::platform::linux::camera_portal;
            self.portal = Some(tokio::task::spawn_blocking(|| {
                camera_portal::open_remote().ok()
            }));
        }
    }

    /// The portal request ended. If the camera is still wanted, the remote
    /// goes to the worker before the camera starts (ADR-0049). After a
    /// refusal, a failure, or a remote that could not go to the worker, no
    /// camera-on goes to the worker, which may still hold an older remote
    /// that it did not use: the camera is unavailable, and no other camera
    /// path is tried. If the camera was turned off meanwhile, the remote is
    /// closed unused. "Wanted" is the latest request that `set_camera`
    /// accepted, also one whose command is still queued.
    fn portal_finished(&mut self, inner: &Inner, remote: PortalRemote) -> Result<(), EndReason> {
        if !self.camera_wanted.load(Ordering::SeqCst) {
            return Ok(());
        }
        if !self.send_portal_remote(remote) {
            let refused = FromWorker::Camera {
                state: CameraState::Unavailable,
            };
            return self.worker_event(inner, &refused);
        }
        self.portal_capture = true;
        self.portal_sent = true;
        self.send_worker(ToWorker::SetCamera { on: true })
    }

    /// Sends the portal's remote to the worker. Returns whether it went.
    #[cfg(target_os = "linux")]
    fn send_portal_remote(&self, remote: PortalRemote) -> bool {
        use std::os::fd::AsFd;
        remote.is_some_and(|remote| self.link.send_camera_handle(remote.as_fd()).is_ok())
    }

    /// No camera portal off Linux: nothing goes.
    #[cfg(not(target_os = "linux"))]
    fn send_portal_remote(&self, _: PortalRemote) -> bool {
        false
    }

    /// Applies one worker event. For a tile's `VideoStarted` or
    /// `VideoStopped`, the store is told only after the new metadata is
    /// applied, synced and published: the read thread counted the change, and
    /// the tile keeps pictures again only once every counted change is
    /// applied here. So no picture of a new stream shows under the old
    /// participant.
    fn worker_event(&mut self, inner: &Inner, event: &FromWorker) -> Result<(), EndReason> {
        // A camera that is not on has no capture left: its remote is used up.
        // An `On` after a remote went (and no off since) is a capture over it.
        if let FromWorker::Camera { state } = event {
            if *state != CameraState::On {
                self.portal_capture = false;
            } else if self.portal_sent {
                self.portal_capture = true;
            }
        }
        match self.view.apply(event) {
            Applied::Same => {}
            Applied::Changed => {
                self.sync_video();
                self.publish(inner);
            }
            Applied::End(reason) => return Err(reason),
        }
        if let FromWorker::VideoStarted { tile, .. } | FromWorker::VideoStopped { tile } = event {
            self.link.video().stream_applied(*tile);
        }
        Ok(())
    }

    fn send_worker(&self, message: ToWorker) -> Result<(), EndReason> {
        self.link
            .send(message)
            .map_err(|_| EndReason::WorkerStopped)
    }

    fn publish(&self, inner: &Inner) {
        inner.publish(self.view.state());
    }

    /// Tells the picture store which tiles may show now.
    fn sync_video(&self) {
        let (own, remote) = self.view.video_allowed();
        self.link.video().sync(own, remote);
    }

    /// The call members now, from the room's stored state. `None` when they
    /// cannot be read.
    async fn read_members(&self) -> Result<Option<Vec<ActiveMember>>, EndReason> {
        let room_id = self.view.room_id.clone();
        self.access
            .run(|client| async move {
                let room = client.get_room(&room_id)?;
                read_members(&room, now_ms()).await.ok()
            })
            .await
            .map_err(|_| EndReason::SessionEnded)
    }

    /// Sends `outgoing` to `recipients`. A failure is not fatal: a device
    /// that did not get the key cannot hear this user.
    async fn send_keys(
        &self,
        outgoing: &Outgoing,
        recipients: Vec<(OwnedUserId, OwnedDeviceId)>,
    ) -> Result<(), EndReason> {
        if recipients.is_empty() {
            return Ok(());
        }
        let room_id = self.view.room_id.clone();
        let content = encryption_keys::outgoing_content(
            &room_id,
            &self.own_identity,
            &self.own_device,
            &outgoing.key,
        );
        self.access
            .run(|client| async move {
                if let Some(room) = client.get_room(&room_id) {
                    let _ = time::timeout(
                        SEND_TIMEOUT,
                        encryption_keys::send_key(&room, &recipients, content),
                    )
                    .await;
                }
            })
            .await
            .map_err(|_| EndReason::SessionEnded)
    }

    /// Reads the members and gives the current key to all of them.
    async fn start_keys(&mut self, inner: &Inner) -> Result<(), EndReason> {
        if let Some(members) = self.read_members().await? {
            self.view.set_members(others(members, &self.own_identity));
        }
        self.sync_video();
        let outgoing = Outgoing {
            key: self.own_keys.current().clone(),
            use_at: Instant::now(),
            recipients: Recipients::AllMembers,
        };
        let all = recipients(&outgoing, &self.view.members, &[]);
        self.send_keys(&outgoing, all).await?;
        self.publish(inner);
        Ok(())
    }

    async fn run(
        &mut self,
        inner: &Inner,
        stop: &mut oneshot::Receiver<()>,
        commands: &mut mpsc::Receiver<Command>,
    ) -> Result<(), EndReason> {
        let mut parked: Vec<Candidate> = Vec::new();
        let mut pending_own: Option<(Instant, FrameKeyMsg)> = None;
        let mut seen_self = false;
        self.start_keys(inner).await?;
        // A video join on the portal path asks for the camera now (the join
        // sent `camera_on: false`).
        if self.camera_wanted.load(Ordering::SeqCst) && uses_portal(&self.link) {
            self.start_portal();
        }
        loop {
            let due = pending_own.as_ref().map(|(at, _)| *at);
            tokio::select! {
                _ = &mut *stop => return Ok(()),
                command = commands.recv() => match command {
                    Some(Command::Mute(muted)) => self.send_worker(ToWorker::SetMuted { muted })?,
                    Some(Command::Camera { on, request }) => {
                        self.camera_command(inner, on, request)?;
                    }
                    None => return Ok(()),
                },
                remote = portal_done(&mut self.portal) => self.portal_finished(inner, remote)?,
                event = self.link.events.recv() => {
                    let Some(event) = event else {
                        return Err(EndReason::WorkerStopped);
                    };
                    self.worker_event(inner, &event)?;
                }
                Some(()) = self.changes.recv() => {
                    self.members_changed(inner, &mut parked, &mut pending_own, &mut seen_self)
                        .await?;
                }
                Some(candidate) = self.incoming.recv() => {
                    self.key_received(inner, candidate, &mut parked)?;
                }
                Some(reason) = self.failed.recv() => return Err(reason),
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
        let Some(all) = self.read_members().await? else {
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
                // The tile keeps its track: the worker still receives it, and
                // the member filter hides the tile and drops its picture
                // until the membership returns. `ParticipantLeft` and
                // `VideoStopped` end the assignment.
                self.send_worker(ToWorker::RemoveParticipant {
                    identity: identity.clone(),
                })?;
            }
        }
        if let Some(outgoing) = key_action(&diff, &mut self.own_keys, Instant::now())
            .map_err(|_| EndReason::KeysUnavailable)?
        {
            let to = recipients(&outgoing, &self.view.members, &diff.joined);
            self.send_keys(&outgoing, to).await?;
            // A key already in use is not set again.
            if outgoing.recipients == Recipients::AllMembers {
                *pending_own = Some((outgoing.use_at, worker_key(&outgoing.key)));
            }
        }
        // Keys that came before their sender's membership may fit now.
        for candidate in std::mem::take(parked) {
            self.key_received(inner, candidate, parked)?;
        }
        // A video stream may have started before its member was known.
        self.sync_video();
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
            &view.room_id,
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

    /// Ends the call: the keeper, the worker, then (unless the session
    /// ended) the membership and the handlers in one operation. After the end
    /// of the session the worker is killed at once, and no request is sent.
    async fn finish(self, session_ended: bool) {
        let Call {
            access,
            link,
            delayed,
            state_key,
            view,
            handles,
            mut tasks,
            ..
        } = self;
        // No more restarts, so the leave cannot be pushed back.
        tasks.abort_all();
        if session_ended {
            drop(link);
            return;
        }
        finish_worker(link).await;
        let room_id = view.room_id;
        let _ = access
            .run(|client| async move {
                end_membership(&client, &room_id, &delayed, &state_key).await;
                for handle in handles {
                    client.remove_event_handler(handle);
                }
            })
            .await;
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
            false,
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
        assert_eq!(
            view.apply(&FromWorker::Devices {
                recording: Vec::new(),
                playout: Vec::new(),
                cameras: Vec::new()
            }),
            Applied::End(EndReason::WorkerFailed(FailureCode::Protocol))
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

    fn no_admission() -> Admission {
        Arc::new(|| None)
    }

    fn controller(host: Arc<dyn CallHost>) -> CallController {
        CallController::new(host, no_admission())
    }

    /// GIVEN a profile proxy WHEN the user does not confirm THEN the call is
    /// declined; WHEN there is no proxy THEN nobody is asked.
    #[test]
    fn given_a_proxy_without_confirmation_then_the_call_is_declined() {
        let host = FakeHost::new(false, None);
        let calls = controller(host.clone());
        assert_eq!(calls.confirm_direct(true).err(), Some(CallError::Declined));
        assert_eq!(host.confirms.load(Ordering::SeqCst), 1);
        assert!(calls.confirm_direct(false).is_ok());
        assert_eq!(host.confirms.load(Ordering::SeqCst), 1);

        let yes = controller(FakeHost::new(true, None));
        assert!(yes.confirm_direct(true).is_ok());
    }

    /// GIVEN a call in progress WHEN the user is asked about a direct
    /// connection THEN nobody is asked and the join is refused.
    #[tokio::test]
    async fn given_a_call_in_progress_then_nobody_is_asked_about_a_proxy() {
        let host = FakeHost::new(true, None);
        let calls = controller(host.clone());
        let _guard = calls.claim(room_id!("!r:example.org")).unwrap();
        assert_eq!(
            calls.confirm_direct(true).err(),
            Some(CallError::AlreadyInCall)
        );
        assert_eq!(host.confirms.load(Ordering::SeqCst), 0);
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
        let controller = controller(FakeHost::new(true, None));
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
        let controller = controller(FakeHost::new(true, None));
        assert_eq!(controller.leave().await, Err(CallError::NotInCall));
        assert_eq!(controller.set_muted(true), Err(CallError::NotInCall));
    }

    fn killer() -> (Arc<AtomicBool>, Arc<dyn WorkerProcess>) {
        let killed = Arc::new(AtomicBool::new(false));
        (killed.clone(), Arc::new(FakeProcess(killed)))
    }

    /// GIVEN a running call WHEN the session ends THEN the worker is killed
    /// at once, the task is told to stop, its flag forbids new SDK work, the
    /// slot is free and the state is `Ended { SessionEnded }`.
    #[tokio::test]
    async fn given_a_running_call_when_the_session_ends_then_the_worker_is_killed() {
        let calls = controller(FakeHost::new(true, None));
        let room = room_id!("!r:example.org");
        let (killed, kill) = killer();
        let flags = Arc::new(Flags::default());
        let (stop, mut stop_rx) = oneshot::channel();
        let (commands, _commands_rx) = mpsc::channel(1);
        *calls.inner.slot() = Slot::Running(RunHandle {
            id: 7,
            room_id: room.to_owned(),
            flags: flags.clone(),
            kill,
            stop: Some(stop),
            commands,
            camera_wanted: Arc::new(AtomicBool::new(false)),
            task: None,
        });
        calls.stop_for_session_end();
        assert!(killed.load(Ordering::SeqCst), "audio stops at once");
        assert!(flags.ended());
        assert!(stop_rx.try_recv().is_ok(), "the task is told to stop");
        assert!(matches!(*calls.inner.slot(), Slot::Idle));
        assert_eq!(
            calls.state(),
            CallState::Ended {
                room_id: room.to_owned(),
                reason: EndReason::SessionEnded
            }
        );
        assert_eq!(calls.set_muted(true), Err(CallError::NotInCall));
        // The task's own end finds the slot taken and publishes nothing.
        assert!(!calls.inner.release(7));
        // A second stop does nothing.
        calls.stop_for_session_end();
    }

    /// GIVEN a join with a running worker WHEN the session ends THEN the
    /// worker is killed, the join must stop at its next step, and the state
    /// stays `Ended { SessionEnded }` after the join lets go.
    #[tokio::test]
    async fn given_a_join_when_the_session_ends_then_the_join_stops_and_the_worker_dies() {
        let calls = controller(FakeHost::new(true, None));
        let room = room_id!("!r:example.org");
        let guard = calls.claim(room).unwrap();
        let (killed, kill) = killer();
        *plain(&guard.kill) = Some(kill);
        calls.stop_for_session_end();
        assert!(killed.load(Ordering::SeqCst));
        assert_eq!(guard.flags.halt(), Err(CallError::SessionEnded));
        assert_eq!(
            calls.confirm_direct(false).err(),
            Some(CallError::AlreadyInCall)
        );
        drop(guard);
        assert!(matches!(*calls.inner.slot(), Slot::Idle));
        assert_eq!(
            calls.state(),
            CallState::Ended {
                room_id: room.to_owned(),
                reason: EndReason::SessionEnded
            }
        );
    }

    /// GIVEN no call WHEN the session ends THEN nothing changes.
    #[test]
    fn given_no_call_when_the_session_ends_then_the_state_stays_idle() {
        let calls = controller(FakeHost::new(true, None));
        calls.stop_for_session_end();
        assert_eq!(calls.state(), CallState::Idle);
    }

    /// GIVEN a join in progress WHEN the user leaves THEN the join is
    /// cancelled at its next step, and its end shows `Ended { Left }`.
    #[tokio::test]
    async fn given_a_join_in_progress_when_leave_is_called_then_the_join_is_cancelled() {
        let calls = controller(FakeHost::new(true, None));
        let room = room_id!("!r:example.org");
        let guard = calls.claim(room).unwrap();
        assert_eq!(guard.flags.halt(), Ok(()));
        assert_eq!(calls.leave().await, Ok(()));
        assert_eq!(guard.flags.halt(), Err(CallError::Cancelled));
        assert!(
            matches!(calls.state(), CallState::Joining { .. }),
            "the join undoes its steps first"
        );
        drop(guard);
        assert_eq!(
            calls.state(),
            CallState::Ended {
                room_id: room.to_owned(),
                reason: EndReason::Left
            }
        );
        assert!(calls.claim(room).is_ok(), "the slot is free again");
    }

    /// GIVEN a state listener WHEN the state changes THEN it is told once per
    /// change, and not when the state stays the same.
    #[tokio::test]
    async fn given_a_listener_then_each_change_is_reported_once() {
        let calls = controller(FakeHost::new(true, None));
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        calls.set_listener(Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        let room = room_id!("!r:example.org");
        let guard = calls.claim(room).unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        drop(guard);
        assert_eq!(count.load(Ordering::SeqCst), 2);
        calls.stop_for_session_end();
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    /// GIVEN no admission, or a stopped call WHEN an SDK operation is asked
    /// for THEN it does not run.
    #[tokio::test]
    async fn given_no_admission_or_a_stopped_call_then_no_sdk_operation_runs() {
        let ran = Arc::new(AtomicBool::new(false));
        let flags = Arc::new(Flags::default());
        let access = Access {
            admit: no_admission(),
            generation: 0,
            flags: flags.clone(),
        };
        let marker = ran.clone();
        assert!(
            access
                .run(|_client| async move { marker.store(true, Ordering::SeqCst) })
                .await
                .is_err()
        );
        flags.ended.store(true, Ordering::SeqCst);
        let marker = ran.clone();
        assert!(
            access
                .run(|_client| async move { marker.store(true, Ordering::SeqCst) })
                .await
                .is_err()
        );
        assert!(!ran.load(Ordering::SeqCst));
    }

    /// A call task's state with a fake worker: what the task sends to the
    /// worker arrives on `to_worker`.
    struct TestCall {
        call: Call,
        to_worker: mpsc::Receiver<ToWorker>,
        _events: mpsc::Sender<FromWorker>,
        _changes: mpsc::Sender<()>,
        _incoming: mpsc::Sender<Candidate>,
        _failed: mpsc::Sender<EndReason>,
    }

    fn test_call(admission: Admission, flags: Arc<Flags>, killed: Arc<AtomicBool>) -> TestCall {
        let (command_tx, to_worker) = mpsc::channel(8);
        let (events, event_rx) = mpsc::channel(4);
        let link = CallWorkerLink::from_parts(command_tx, event_rx, Box::new(FakeProcess(killed)));
        let (changes_tx, changes) = mpsc::channel(1);
        let (incoming_tx, incoming) = mpsc::channel(1);
        let (failed_tx, failed) = mpsc::channel(1);
        let call = Call {
            access: Access {
                admit: admission,
                generation: 0,
                flags,
            },
            link,
            delayed: DelayedLeave::for_test(),
            state_key: "key".to_owned(),
            own_identity: "@a:example.org:A".to_owned(),
            own_device: owned_device_id!("A"),
            own_keys: OwnKeys::new(Instant::now()).unwrap(),
            remote: RemoteKeys::new(),
            view: view(),
            changes,
            incoming,
            failed,
            handles: Vec::new(),
            tasks: JoinSet::new(),
            camera_wanted: Arc::new(AtomicBool::new(false)),
            portal: None,
            portal_capture: false,
            portal_sent: false,
        };
        TestCall {
            call,
            to_worker,
            _events: events,
            _changes: changes_tx,
            _incoming: incoming_tx,
            _failed: failed_tx,
        }
    }

    /// Ends a portal request that produced no remote.
    fn finish_without_remote(call: &mut Call, inner: &Inner) {
        #[cfg(target_os = "linux")]
        call.portal_finished(inner, None).unwrap();
        #[cfg(not(target_os = "linux"))]
        call.portal_finished(inner, ()).unwrap();
    }

    /// GIVEN a portal request that ends without a remote (a refusal or a
    /// failure) WHEN the user's latest accepted request is off (its command
    /// may still be queued) THEN nothing changes; WHEN it is on THEN no
    /// camera-on goes to the worker, which could still hold an older unused
    /// remote, and the camera shows unavailable.
    #[test]
    fn given_a_refused_portal_request_then_no_camera_on_goes_to_the_worker() {
        let fixture = test_call(
            no_admission(),
            Arc::new(Flags::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let inner = controller(FakeHost::new(true, None)).inner.clone();
        let (mut call, mut to_worker) = (fixture.call, fixture.to_worker);
        call.camera_wanted.store(false, Ordering::SeqCst);
        finish_without_remote(&mut call, &inner);
        assert!(
            to_worker.try_recv().is_err(),
            "an off already accepted wins"
        );
        assert_eq!(call.view.camera, CallCamera::Off);
        call.camera_wanted.store(true, Ordering::SeqCst);
        call.view.request_camera(true);
        finish_without_remote(&mut call, &inner);
        assert!(
            to_worker.try_recv().is_err(),
            "no camera-on after a refusal"
        );
        assert_eq!(call.view.camera, CallCamera::Unavailable);
        assert!(!call.portal_capture);
    }

    /// GIVEN a portal capture turned off and on again WHEN the old capture's
    /// `Off` reply arrives after the new remote went THEN it clears the flag
    /// only until the new capture reports `On`; then a repeated on reuses the
    /// capture without a new portal request.
    #[test]
    fn given_an_old_off_after_a_new_remote_then_the_new_capture_is_reused() {
        let fixture = test_call(
            no_admission(),
            Arc::new(Flags::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let inner = controller(FakeHost::new(true, None)).inner.clone();
        let mut call = fixture.call;
        call.link = std::mem::replace(
            &mut call.link,
            CallWorkerLink::from_parts(
                mpsc::channel(1).0,
                mpsc::channel(1).1,
                Box::new(FakeProcess(Arc::new(AtomicBool::new(false)))),
            ),
        )
        .with_camera_path(crate::calls::worker::CameraPath::Portal);
        let camera = |state| FromWorker::Camera { state };
        // Capture A runs; the camera is turned off.
        call.portal_capture = true;
        call.portal_sent = true;
        call.camera_command(&inner, false, 0).unwrap();
        // The camera is turned on; the new remote went (as `portal_finished`
        // sets it) before A's `Off` arrives.
        call.portal_capture = true;
        call.portal_sent = true;
        call.worker_event(&inner, &camera(CameraState::Off))
            .unwrap();
        assert!(
            call.portal_needed(true),
            "the flag waits for the new capture"
        );
        call.worker_event(&inner, &camera(CameraState::On)).unwrap();
        assert!(!call.portal_needed(true), "the new capture is reused");
        // A late `On` of a capture turned off sets nothing.
        call.camera_command(&inner, false, 0).unwrap();
        call.worker_event(&inner, &camera(CameraState::On)).unwrap();
        assert!(call.portal_needed(true));
    }

    /// GIVEN the worker already reported the camera `On` while the own tile
    /// is blocked by an off WHEN the task takes the latest camera-on (the
    /// view does not change) THEN the store is synced and the own picture is
    /// kept; an older camera-on taken the same way keeps the tile shut.
    #[test]
    fn given_a_camera_on_taken_after_the_on_report_then_own_pictures_show() {
        let fixture = test_call(
            no_admission(),
            Arc::new(Flags::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let inner = controller(FakeHost::new(true, None)).inner.clone();
        let mut call = fixture.call;
        let store = call.link.video();
        call.worker_event(&inner, &FromWorker::Connected).unwrap();
        let stale = store.camera_request(true);
        store.camera_request(false);
        let latest = store.camera_request(true);
        call.view.request_camera(true);
        call.worker_event(
            &inner,
            &FromWorker::Camera {
                state: CameraState::On,
            },
        )
        .unwrap();
        assert_eq!(call.view.camera, CallCamera::On);
        call.camera_command(&inner, true, stale).unwrap();
        assert!(!store.insert(0, 0, 2, 2, vec![1; 16]), "a stale on");
        call.camera_command(&inner, true, latest).unwrap();
        assert!(store.insert(0, 0, 2, 2, vec![2; 16]), "the latest on");
    }

    /// GIVEN a portal capture that runs WHEN the camera is turned on again
    /// THEN no new portal request is needed; WHEN it is turned off and a late
    /// `On` reply of the old capture arrives THEN the next on asks the portal
    /// again, and so does one after the worker reports the camera
    /// unavailable.
    #[test]
    fn given_a_portal_capture_then_only_a_running_one_skips_the_portal() {
        let fixture = test_call(
            no_admission(),
            Arc::new(Flags::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let inner = controller(FakeHost::new(true, None)).inner.clone();
        let mut call = fixture.call;
        let mut to_worker = fixture.to_worker;
        call.link = std::mem::replace(
            &mut call.link,
            CallWorkerLink::from_parts(
                mpsc::channel(1).0,
                mpsc::channel(1).1,
                Box::new(FakeProcess(Arc::new(AtomicBool::new(false)))),
            ),
        )
        .with_camera_path(crate::calls::worker::CameraPath::Portal);
        let on = FromWorker::Camera {
            state: CameraState::On,
        };
        assert!(call.portal_needed(true), "the first on asks the portal");
        assert!(!call.portal_needed(false));
        // The remote went to the worker and the capture runs.
        call.portal_capture = true;
        call.worker_event(&inner, &on).unwrap();
        assert!(!call.portal_needed(true), "a repeated on");
        call.camera_command(&inner, false, 0).unwrap();
        assert!(matches!(
            to_worker.try_recv(),
            Ok(ToWorker::SetCamera { on: false })
        ));
        call.worker_event(&inner, &on).unwrap();
        assert_eq!(call.view.camera, CallCamera::On, "a late reply");
        assert!(call.portal_needed(true), "the old capture is gone");
        call.portal_capture = true;
        call.worker_event(
            &inner,
            &FromWorker::Camera {
                state: CameraState::Unavailable,
            },
        )
        .unwrap();
        assert!(call.portal_needed(true), "an ended capture");
    }

    /// GIVEN a remote tile WHEN its member's membership lapses and returns
    /// with the track still live THEN the tile hides meanwhile and shows
    /// again without a new `VideoStarted`.
    #[test]
    fn given_a_membership_lapse_then_a_live_track_shows_again() {
        let mut view = view();
        view.apply(&started(1, "@a:example.org:A"));
        assert!(view.video_allowed().1[0]);
        let members = view.members.clone();
        view.set_members(
            members
                .iter()
                .filter(|m| m.identity != "@a:example.org:A")
                .cloned()
                .collect(),
        );
        assert!(!view.video_allowed().1[0], "hidden during the lapse");
        view.set_members(members);
        assert!(view.video_allowed().1[0], "shown again");
    }

    /// GIVEN tile 1 shows member A WHEN the read thread has counted A's stop
    /// and B's start THEN no picture is kept until the task applied both,
    /// and once it has, the store and the published tile agree on B.
    #[test]
    fn given_a_tile_handover_then_pictures_follow_the_applied_metadata() {
        let fixture = test_call(
            no_admission(),
            Arc::new(Flags::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let inner = controller(FakeHost::new(true, None)).inner.clone();
        let mut call = fixture.call;
        let store = call.link.video();
        call.worker_event(&inner, &FromWorker::Connected).unwrap();
        store.stream_changed(1);
        call.worker_event(&inner, &started(1, "@a:example.org:A"))
            .unwrap();
        assert!(store.insert(1, 0, 2, 2, vec![1; 16]), "A shows");
        // The read thread reads B's handover before the task applies it.
        store.stream_changed(1);
        store.stream_changed(1);
        assert!(!store.insert(1, 0, 2, 2, vec![2; 16]), "B waits");
        call.worker_event(&inner, &FromWorker::VideoStopped { tile: 1 })
            .unwrap();
        assert!(
            !store.insert(1, 0, 2, 2, vec![2; 16]),
            "still one change left"
        );
        call.worker_event(&inner, &started(1, "@b:example.org:B"))
            .unwrap();
        assert!(store.insert(1, 0, 2, 2, vec![2; 16]), "B shows");
        let tiles = call.view.video_tiles();
        assert_eq!(tiles.len(), 1);
        assert_eq!(
            tiles[0].participant.as_ref().map(|p| p.user_id.as_str()),
            Some("@b:example.org")
        );
    }

    /// GIVEN a running call WHEN the session stops admitting work THEN the
    /// call ends at once: the worker is killed, nothing is sent to Matrix
    /// (the only admission asked for is the one that failed), the state is
    /// `Ended { SessionEnded }` and the slot is free.
    #[tokio::test]
    async fn given_a_refused_admission_when_a_call_runs_then_it_ends_without_a_leave() {
        let asked = Arc::new(AtomicUsize::new(0));
        let counted = asked.clone();
        let admission: Admission = Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            None
        });
        let calls = CallController::new(FakeHost::new(true, None), admission.clone());
        let room = room_id!("!r:example.org").to_owned();
        let killed = Arc::new(AtomicBool::new(false));
        let flags = Arc::new(Flags::default());
        let fixture = test_call(admission, flags.clone(), killed.clone());
        let call = fixture.call;
        let (stop, stop_rx) = oneshot::channel();
        let (commands, command_rx) = mpsc::channel(1);
        *calls.inner.slot() = Slot::Running(RunHandle {
            id: 3,
            room_id: room.clone(),
            flags: flags.clone(),
            kill: Arc::new(FakeProcess(killed.clone())),
            stop: Some(stop),
            commands,
            camera_wanted: call.camera_wanted.clone(),
            task: None,
        });
        run_call(
            calls.inner.clone(),
            3,
            flags.clone(),
            call,
            stop_rx,
            command_rx,
        )
        .await;
        assert!(killed.load(Ordering::SeqCst));
        assert!(flags.ended());
        assert_eq!(asked.load(Ordering::SeqCst), 1, "no leave was tried");
        assert!(matches!(*calls.inner.slot(), Slot::Idle));
        assert_eq!(
            calls.state(),
            CallState::Ended {
                room_id: room,
                reason: EndReason::SessionEnded
            }
        );
    }

    /// GIVEN a joining call WHEN it joins with video THEN the camera is
    /// `starting` until the worker answers; without video it is off.
    #[test]
    fn given_a_join_with_video_then_the_camera_starts_until_the_worker_answers() {
        let url = || Url::parse("https://sfu.example.org").unwrap();
        let room = || room_id!("!r:example.org").to_owned();
        let mut with = View::new(room(), url(), true);
        with.apply(&FromWorker::Connected);
        let CallState::Active { camera, video, .. } = with.state() else {
            panic!("active");
        };
        assert_eq!(camera, CallCamera::Starting);
        assert!(video.is_empty(), "no own tile until the camera is on");
        assert_eq!(
            View::new(room(), url(), false).camera,
            CallCamera::Off,
            "no video, no camera"
        );
    }

    /// GIVEN each camera state the worker reports WHEN applied THEN the page
    /// sees the matching code, the own tile shows only while the camera is
    /// on, and the picture store allows the own tile only then.
    #[test]
    fn given_camera_reports_then_the_state_and_the_own_tile_follow() {
        let mut view = view();
        view.apply(&FromWorker::Connected);
        for (state, camera) in [
            (CameraState::On, CallCamera::On),
            (CameraState::NotFound, CallCamera::NotFound),
            (CameraState::Unavailable, CallCamera::Unavailable),
            (CameraState::Off, CallCamera::Off),
        ] {
            assert_eq!(view.apply(&FromWorker::Camera { state }), Applied::Changed);
            assert_eq!(view.camera, camera);
            let on = camera == CallCamera::On;
            assert_eq!(view.video_allowed().0, on);
            assert_eq!(view.video_tiles().len(), usize::from(on));
            assert_eq!(
                view.apply(&FromWorker::Camera { state }),
                Applied::Same,
                "a repeat changes nothing"
            );
        }
        view.apply(&FromWorker::Camera {
            state: CameraState::On,
        });
        assert_eq!(
            view.video_tiles(),
            [CallVideoTile {
                tile: 0,
                participant: None,
                own: true
            }]
        );
    }

    /// GIVEN the page turns the camera on and off WHEN requested THEN it is
    /// `starting` until the worker says `on`, an `on` camera stays on, and
    /// off is at once.
    #[test]
    fn given_camera_requests_then_the_camera_starts_and_stops() {
        let mut view = view();
        assert_eq!(view.request_camera(false), Applied::Same);
        assert_eq!(view.request_camera(true), Applied::Changed);
        assert_eq!(view.camera, CallCamera::Starting);
        assert_eq!(view.request_camera(true), Applied::Same);
        view.apply(&FromWorker::Camera {
            state: CameraState::On,
        });
        assert_eq!(view.request_camera(true), Applied::Same);
        assert_eq!(view.camera, CallCamera::On);
        assert_eq!(view.request_camera(false), Applied::Changed);
        assert_eq!(view.camera, CallCamera::Off);
        view.apply(&FromWorker::Camera {
            state: CameraState::Unavailable,
        });
        assert_eq!(view.request_camera(true), Applied::Changed);
        assert_eq!(view.camera, CallCamera::Starting, "a retry starts again");
    }

    fn started(tile: u8, identity: &str) -> FromWorker {
        FromWorker::VideoStarted {
            tile,
            identity: identity.to_owned(),
        }
    }

    /// GIVEN video starts on remote tiles WHEN the identity is a member THEN
    /// the tile shows with that member's IDs and its picture is allowed; an
    /// identity that is not a member shows nothing, and shows once it joins.
    #[test]
    fn given_remote_video_then_only_members_show_and_may_keep_pictures() {
        let mut view = view();
        view.apply(&FromWorker::Connected);
        assert_eq!(
            view.apply(&started(1, "@a:example.org:A")),
            Applied::Changed
        );
        assert_eq!(view.apply(&started(1, "@a:example.org:A")), Applied::Same);
        view.apply(&started(2, "@stranger:example.org:S"));
        assert_eq!(view.video_allowed(), (false, [true, false, false, false]));
        let CallState::Active { video, .. } = view.state() else {
            panic!("active");
        };
        assert_eq!(
            video,
            [CallVideoTile {
                tile: 1,
                participant: Some(CallParticipantRef {
                    user_id: "@a:example.org".to_owned(),
                    device_id: "A".to_owned(),
                }),
                own: false,
            }]
        );
        view.set_members(vec![member(
            "@stranger:example.org",
            "S",
            "https://sfu.example.org",
        )]);
        assert_eq!(view.video_allowed().1, [false, true, false, false]);
        assert_eq!(view.video_tiles()[0].tile, 2);
    }

    /// GIVEN video on tiles WHEN a stream stops, a member leaves, or the tile
    /// is out of range THEN the tile goes, and nothing out of range changes.
    #[test]
    fn given_video_ends_then_the_tile_goes_and_bad_tiles_are_ignored() {
        let mut view = view();
        view.apply(&started(1, "@a:example.org:A"));
        view.apply(&started(3, "@b:example.org:B"));
        assert_eq!(view.apply(&started(0, "@a:example.org:A")), Applied::Same);
        assert_eq!(view.apply(&started(5, "@a:example.org:A")), Applied::Same);
        assert_eq!(
            view.apply(&FromWorker::VideoStopped { tile: 0 }),
            Applied::Same
        );
        assert_eq!(
            view.apply(&FromWorker::VideoStopped { tile: 5 }),
            Applied::Same
        );
        assert_eq!(view.video_allowed().1, [true, false, true, false]);
        assert_eq!(
            view.apply(&FromWorker::VideoStopped { tile: 1 }),
            Applied::Changed
        );
        assert_eq!(
            view.apply(&FromWorker::VideoStopped { tile: 1 }),
            Applied::Same
        );
        assert_eq!(
            view.apply(&FromWorker::ParticipantLeft {
                identity: "@b:example.org:B".to_owned()
            }),
            Applied::Changed
        );
        assert_eq!(view.video_allowed().1, [false; 4]);
        view.apply(&started(2, "@a:example.org:A"));
        assert!(view.forget_video("@a:example.org:A"));
        assert!(!view.forget_video("@a:example.org:A"));
    }

    /// GIVEN a bare picture announcement WHEN applied THEN it is a protocol
    /// violation: the reader turns announcements into pictures.
    #[test]
    fn given_a_bare_video_frame_message_then_the_call_ends() {
        assert_eq!(
            view().apply(&FromWorker::VideoFrame {
                tile: 1,
                rotation: 0
            }),
            Applied::End(EndReason::WorkerFailed(FailureCode::Protocol))
        );
    }

    /// A running call with a picture store and a published state, and the
    /// receiver of what the page asked of it.
    fn running_call(
        calls: &CallController,
        state: CallState,
    ) -> (VideoStore, mpsc::Receiver<Command>, Arc<AtomicBool>) {
        let room = room_id!("!r:example.org");
        let (killed, kill) = killer();
        let (commands, command_rx) = mpsc::channel(4);
        let store = VideoStore::new();
        *plain(&calls.inner.video) = Some(store.clone());
        *calls.inner.slot() = Slot::Running(RunHandle {
            id: 5,
            room_id: room.to_owned(),
            flags: Arc::new(Flags::default()),
            kill,
            stop: None,
            commands,
            camera_wanted: Arc::new(AtomicBool::new(false)),
            task: None,
        });
        calls.inner.publish(state);
        (store, command_rx, killed)
    }

    fn active_state() -> CallState {
        CallState::Active {
            room_id: room_id!("!r:example.org").to_owned(),
            muted: false,
            participants: Vec::new(),
            camera: CallCamera::Off,
            video: Vec::new(),
        }
    }

    /// GIVEN no call, a joining call, or a call whose queue is closed WHEN the
    /// camera is turned on THEN it is refused and no state changes.
    #[tokio::test]
    async fn given_no_connected_call_then_the_camera_is_refused() {
        let calls = controller(FakeHost::new(true, None));
        assert_eq!(calls.set_camera(true), Err(CallError::NotInCall));
        let _guard = calls.claim(room_id!("!r:example.org")).unwrap();
        assert_eq!(calls.set_camera(true), Err(CallError::NotInCall));

        let calls = controller(FakeHost::new(true, None));
        let joining = CallState::Joining {
            room_id: room_id!("!r:example.org").to_owned(),
        };
        let (_store, _commands, _) = running_call(&calls, joining.clone());
        assert_eq!(
            calls.set_camera(true),
            Err(CallError::NotInCall),
            "a call that is not connected yet"
        );
        assert_eq!(calls.state(), joining);

        let calls = controller(FakeHost::new(true, None));
        let (_store, commands, _) = running_call(&calls, active_state());
        drop(commands);
        assert_eq!(calls.set_camera(true), Err(CallError::NotInCall));
        assert_eq!(calls.state(), active_state());
    }

    /// GIVEN a connected call WHEN the camera is turned on THEN the task gets
    /// the command and the state shows `starting` at once; WHEN it is turned
    /// off THEN the own picture and tile go at once.
    #[tokio::test]
    async fn given_a_connected_call_then_the_camera_command_reaches_the_task() {
        let calls = controller(FakeHost::new(true, None));
        let (store, mut commands, _) = running_call(&calls, active_state());
        let changes = Arc::new(AtomicUsize::new(0));
        let counted = changes.clone();
        calls.set_listener(Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(calls.set_camera(true), Ok(()));
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::Camera { on: true, .. })
        ));
        let CallState::Active { camera, .. } = calls.state() else {
            panic!("active");
        };
        assert_eq!(camera, CallCamera::Starting);
        assert_eq!(changes.load(Ordering::SeqCst), 1);

        // The worker answered: the own tile shows and holds a picture.
        store.sync(true, [false; 4]);
        assert!(store.insert(0, 0, 2, 2, vec![1; 16]));
        calls.inner.publish(CallState::Active {
            room_id: room_id!("!r:example.org").to_owned(),
            muted: false,
            participants: Vec::new(),
            camera: CallCamera::On,
            video: vec![CallVideoTile {
                tile: 0,
                participant: None,
                own: true,
            }],
        });
        assert!(calls.video_picture(0, 0).is_some());
        assert_eq!(calls.set_camera(false), Ok(()));
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::Camera { on: false, .. })
        ));
        assert!(calls.video_picture(0, 0).is_none(), "the picture is gone");
        assert!(
            !store.insert(0, 0, 2, 2, vec![2; 16]),
            "a late own picture is refused before the task takes the command"
        );
        // An unrelated event makes the task sync while its view still has
        // the camera on: the own tile stays shut.
        store.sync(true, [false; 4]);
        assert!(!store.insert(0, 0, 2, 2, vec![3; 16]));
        assert!(calls.video_picture(0, 0).is_none());
        let CallState::Active { camera, video, .. } = calls.state() else {
            panic!("active");
        };
        assert_eq!(camera, CallCamera::Off);
        assert!(video.is_empty());
    }

    /// GIVEN the camera is on WHEN the user turns it on and then off before
    /// the task takes either command THEN the task's handling of the older on
    /// does not let a late own picture back in, also after a sync from its
    /// view that still has the camera on.
    #[tokio::test]
    async fn given_a_queued_on_then_off_then_the_older_on_cannot_reopen_the_tile() {
        let calls = controller(FakeHost::new(true, None));
        let (store, mut commands, _) = running_call(&calls, active_state());
        store.sync(true, [false; 4]);
        assert!(store.insert(0, 0, 2, 2, vec![1; 16]));
        assert_eq!(calls.set_camera(true), Ok(()));
        assert_eq!(calls.set_camera(false), Ok(()));
        let Ok(Command::Camera { on: true, request }) = commands.try_recv() else {
            panic!("the on comes first");
        };
        // What `camera_command` does to the store for the older on.
        store.camera_on_taken(request);
        store.sync(true, [false; 4]);
        assert!(
            !store.insert(0, 0, 2, 2, vec![2; 16]),
            "the off accepted after the on keeps the tile shut"
        );
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::Camera { on: false, .. })
        ));
        assert_eq!(calls.set_camera(true), Ok(()));
        let Ok(Command::Camera { on: true, request }) = commands.try_recv() else {
            panic!("a new on");
        };
        store.camera_on_taken(request);
        store.sync(true, [false; 4]);
        assert!(
            store.insert(0, 0, 2, 2, vec![3; 16]),
            "a later on reopens it"
        );
    }

    /// GIVEN a connected call whose command queue is full WHEN the camera is
    /// turned on or off THEN it is refused, the state does not change, and
    /// the own picture stays.
    #[tokio::test]
    async fn given_a_full_command_queue_then_the_camera_state_does_not_change() {
        let calls = controller(FakeHost::new(true, None));
        let (store, _commands, _) = running_call(&calls, active_state());
        store.sync(true, [false; 4]);
        assert!(store.insert(0, 0, 2, 2, vec![1; 16]));
        {
            let Slot::Running(run) = &*calls.inner.slot() else {
                panic!("running");
            };
            while run.commands.try_send(Command::Mute(true)).is_ok() {}
        }
        assert_eq!(calls.set_camera(true), Err(CallError::NotInCall));
        assert_eq!(calls.state(), active_state());
        assert_eq!(calls.set_camera(false), Err(CallError::NotInCall));
        assert_eq!(calls.state(), active_state());
        assert!(calls.video_picture(0, 0).is_some(), "the picture stays");
    }

    /// GIVEN a picture in the store WHEN the page asks THEN only a newer
    /// picture is returned; with no call there is none.
    #[tokio::test]
    async fn given_a_stored_picture_then_the_controller_returns_newer_ones_only() {
        let calls = controller(FakeHost::new(true, None));
        assert!(calls.video_picture(1, 0).is_none(), "no call, no picture");
        let (store, _commands, _) = running_call(&calls, active_state());
        store.sync(false, [true, false, false, false]);
        assert!(store.insert(1, 3, 2, 2, vec![5; 16]));
        let picture = calls.video_picture(1, 0).expect("picture");
        assert_eq!((picture.seq, picture.rotation), (1, 3));
        assert!(calls.video_picture(1, 1).is_none());
        assert!(calls.video_picture(9, 0).is_none());
    }

    /// GIVEN pictures in a running call WHEN the session ends THEN every
    /// picture is dropped and later ones are refused; WHEN the call ends by
    /// itself THEN the same holds.
    #[tokio::test]
    async fn given_the_call_ends_then_every_picture_is_dropped() {
        for session_end in [true, false] {
            let calls = controller(FakeHost::new(true, None));
            let (store, _commands, _) = running_call(&calls, active_state());
            store.sync(true, [true; 4]);
            assert!(store.insert(0, 0, 2, 2, vec![1; 16]));
            assert!(store.insert(4, 0, 2, 2, vec![1; 16]));
            if session_end {
                calls.stop_for_session_end();
            } else {
                assert!(calls.inner.release(5));
            }
            assert!(calls.video_picture(0, 0).is_none());
            assert!(calls.video_picture(4, 0).is_none());
            store.sync(true, [true; 4]);
            assert!(!store.insert(0, 0, 2, 2, vec![1; 16]), "{session_end}");
        }
    }

    /// GIVEN a join that fails after the worker started WHEN its guard drops
    /// THEN the picture store of that worker is closed.
    #[tokio::test]
    async fn given_a_join_that_ends_then_its_picture_store_is_closed() {
        let calls = controller(FakeHost::new(true, None));
        let guard = calls.claim(room_id!("!r:example.org")).unwrap();
        let store = VideoStore::new();
        *plain(&calls.inner.video) = Some(store.clone());
        store.sync(true, [true; 4]);
        drop(guard);
        assert!(!store.insert(0, 0, 2, 2, vec![1; 16]));
        assert!(plain(&calls.inner.video).is_none());
    }
}
