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
use super::rings::{self, Plan, Watch};
use super::screen::{ScreenGrant, ScreenRefusal, ScreenSource};
use super::sfu::{self, OpenIdToken, SfuError};
use super::video::{Notifier, Picture, VideoStore};
use super::worker::{
    CallHost, CallWorkerLink, StartError, WorkerExit, WorkerProcess, exit_is_crash,
};
use super::{MAX_CALL_MEMBERS, keys::FrameKey};
use crate::session::Admission;
use matrix_sdk::{
    Client, RoomState,
    event_handler::EventHandlerHandle,
    ruma::{
        OwnedDeviceId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId,
        api::client::account::request_openid_token,
        events::{AnySyncMessageLikeEvent, AnySyncStateEvent},
        serde::Raw,
    },
};
use persephone_call_protocol::{
    CameraState, FailureCode, FrameKeyMsg, FromWorker, ScreenState, ToWorker, TurnServer,
    VIDEO_TILES,
};
use persephone_contracts::{CallCamera, CallParticipantRef, CallScreen, CallVideoTile};
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
    /// The worker ended with an internal failure: a panic, an abort or a
    /// crash.
    WorkerCrashed,
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
    /// It shares its screen (ADR-0053); a display hint from the sender.
    pub sharing: bool,
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
        /// The own screen share (ADR-0053).
        screen: CallScreen,
        /// The active video tiles, at most [`VIDEO_TILES`].
        video: Vec<CallVideoTile>,
        /// The other member of a direct chat declined this call's ring
        /// (ADR-0052). The call goes on.
        ring_declined: bool,
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
    /// The own screen share.
    screen: CallScreen,
    /// The identities that share a screen, as the worker said with
    /// `ScreenSharer`. A display hint only. An identity that is not a member
    /// yet waits here and shows once the membership is read; only members
    /// are shown. At most `MAX_PARTICIPANTS` wait.
    sharing: HashSet<String>,
    /// The identity on each remote tile (tile 1 first), as the worker said
    /// with `VideoStarted`. An identity that is not a member yet waits here
    /// and shows once the membership is read.
    remote_video: [Option<String>; REMOTE_TILES],
    /// The stream number of each tile (tile 0 first), as the picture store
    /// gave it when the controller applied the tile's latest change. The
    /// page asks for pictures of this stream only.
    video_streams: [u64; VIDEO_TILES as usize],
    /// The ring this call sent was declined (ADR-0052).
    ring_declined: bool,
}

/// Remote video tiles: tiles 1 to 4 are cameras, tile 5 is the screen share.
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
            screen: CallScreen::Off,
            sharing: HashSet::new(),
            remote_video: Default::default(),
            video_streams: Default::default(),
            ring_declined: false,
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
        // `sharing` keeps hints of identities that are not members yet: the
        // worker can hear of a share before the membership arrives.
    }

    /// The page asked for the screen share on or off. The state shows
    /// `starting` until the pick and the worker answer.
    fn request_screen(&mut self, on: bool) -> Applied {
        let next = match (on, self.screen) {
            (true, CallScreen::On | CallScreen::Starting) => self.screen,
            (true, _) => CallScreen::Starting,
            (false, _) => CallScreen::Off,
        };
        self.set_screen(next)
    }

    fn set_screen(&mut self, screen: CallScreen) -> Applied {
        if self.screen == screen {
            return Applied::Same;
        }
        self.screen = screen;
        Applied::Changed
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
        let mut changed = self.sharing.remove(identity);
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

    /// Starts the view with the picture store's stream numbers.
    fn with_video_streams(mut self, streams: [u64; VIDEO_TILES as usize]) -> Self {
        self.video_streams = streams;
        self
    }

    /// Records the stream number of `tile`; a tile out of range is ignored.
    fn set_video_stream(&mut self, tile: u8, stream: u64) {
        if let Some(slot) = self.video_streams.get_mut(usize::from(tile)) {
            *slot = stream;
        }
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
                stream: self.video_streams[0],
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
                stream: self.video_streams[index + 1],
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
            FromWorker::Screen { state, .. } => self.set_screen(match state {
                ScreenState::Off => CallScreen::Off,
                ScreenState::Starting => CallScreen::Starting,
                ScreenState::On => CallScreen::On,
                ScreenState::NotFound => CallScreen::NotFound,
                ScreenState::Unavailable => CallScreen::Unavailable,
                ScreenState::Refused => CallScreen::Refused,
            }),
            FromWorker::ScreenSharer { identity, sharing } => {
                // Kept also for an identity that is not a member yet, up to
                // the bound; only a member is ever shown as sharing.
                let changed = if !*sharing {
                    self.sharing.remove(identity)
                } else if self.sharing.len() < persephone_call_protocol::MAX_PARTICIPANTS {
                    self.sharing.insert(identity.clone())
                } else {
                    false
                };
                if changed && self.is_member(identity) {
                    Applied::Changed
                } else {
                    Applied::Same
                }
            }
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
                    sharing: self.sharing.contains(&m.identity),
                })
                .collect(),
            camera: self.camera,
            screen: self.screen,
            video: self.video_tiles(),
            ring_declined: self.ring_declined,
        }
    }
}

pub(crate) fn now_ms() -> u64 {
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
    /// The screen stops accepted and the share's portal session, shared
    /// with the call task.
    screen_control: Arc<Mutex<ScreenControl>>,
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
    /// The screen share on or off (ADR-0053). The page names no source.
    /// `stops` is the stop count when the handle accepted the command;
    /// `request` numbers the screen commands the handle accepted.
    Screen {
        on: bool,
        stops: u64,
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
            self.notify();
        }
    }

    /// Tells the listener that the state changed. Called with no lock held.
    fn notify(&self) {
        let listener = plain(&self.listener).clone();
        if let Some(listener) = listener {
            listener();
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

    /// The newest picture of stream `stream` on `tile` with a number above
    /// `after`, if any. It holds the store lock only to copy a pointer.
    pub fn video_picture(&self, tile: u8, after: u64, stream: u64) -> Option<Picture> {
        let store = plain(&self.inner.video).clone();
        store?.newest(tile, after, stream)
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
        let screen_control = call.screen_control.clone();
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
                        screen_control,
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
        let decline_tx = change_tx.clone();
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

        // 5b. Whether to ring: only when no other member is in the call yet,
        // read before the own membership exists (ADR-0052).
        let plan = ring_plan(&room, &user_id, &device_id).await;
        let ring_watch = match &plan {
            Some(Plan::Ring(contact)) => {
                let watch = Arc::new(Watch::new(contact.clone()));
                setup
                    .handles
                    .push(decline_handler(&room, &watch, decline_tx.clone()));
                Some((watch, decline_tx))
            }
            _ => None,
        };
        flags.halt()?;

        // 6. Own membership.
        let content = own_membership(
            room_id.as_str(),
            &device_id,
            &user_id,
            &sfu.advertised,
            Some(created_ts),
            scope.video,
        );
        // Set before the send: a timeout or a lost answer may hide a write.
        setup.membership_sent = true;
        let membership = time::timeout(
            SEND_TIMEOUT,
            room.send_state_event_raw(EVENT_TYPE, &state_key, content),
        )
        .await
        .map_err(|_| CallError::Membership)?
        .map_err(|error| {
            if membership_refused(&error) {
                // Nothing was written, so the cleanup cancels the delayed
                // leave instead of sending it.
                setup.membership_sent = false;
            }
            CallError::Membership
        })?;
        flags.halt()?;
        // The ring follows the membership. It runs beside the join, and its
        // failure does not end the call.
        if let Some(plan) = plan {
            setup.tasks.spawn_on(
                send_ring(
                    access.clone(),
                    room_id.to_owned(),
                    plan,
                    membership.event_id,
                    scope.video,
                    ring_watch.clone(),
                ),
                &scope.runtime,
            );
        }

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
        let video_streams = link.video().streams();
        Ok(Call {
            access: access.clone(),
            link,
            delayed,
            state_key,
            own_identity,
            own_device: device_id,
            own_keys,
            remote: RemoteKeys::new(),
            view: View::new(room_id.to_owned(), sfu.url, scope.video)
                .with_video_streams(video_streams),
            changes,
            incoming,
            failed,
            handles: std::mem::take(&mut setup.handles),
            tasks: std::mem::take(&mut setup.tasks),
            camera_wanted: Arc::new(AtomicBool::new(scope.video)),
            portal: None,
            portal_capture: false,
            portal_sent: false,
            ring: ring_watch.map(|(watch, _)| watch),
            screens: self.inner.host.screen_source(),
            screen_pick: None,
            screen_wanted: false,
            screen_closed: None,
            screen_sent: false,
            screen_control: Arc::default(),
            screen_pick_stops: 0,
            screen_seen_stops: 0,
            screen_seen_request: 0,
            screen_share: 0,
            screen_stop_pending: None,
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

    /// Starts or stops the screen share (ADR-0053). Refused unless a call is
    /// connected. The page names no screen: on a start the platform's source
    /// shows its own warning or chooser, and a cancel leaves the share off.
    /// The state shows `starting` at once when started, and the answer of the
    /// pick and the worker replaces it.
    pub fn set_screen(&self, on: bool) -> Result<(), CallError> {
        let slot = self.inner.slot();
        let Slot::Running(run) = &*slot else {
            return Err(CallError::NotInCall);
        };
        if !matches!(*self.inner.state.borrow(), CallState::Active { .. }) {
            return Err(CallError::NotInCall);
        }
        // A queue place is taken first, so a full queue changes nothing.
        let permit = run
            .commands
            .try_reserve()
            .map_err(|_| CallError::NotInCall)?;
        // The page's screen changes and a stop is counted under the lock
        // that the call task holds while it publishes (`Call::publish`). So
        // a task publication either comes first, and this `Off` replaces it,
        // or it sees the stop and keeps this `Off`.
        let mut control = plain(&run.screen_control);
        let changed = self.inner.state.send_if_modified(|state| {
            let CallState::Active { screen, .. } = state else {
                return false;
            };
            let next = match (on, *screen) {
                (true, CallScreen::On | CallScreen::Starting) => *screen,
                (true, _) => CallScreen::Starting,
                (false, _) => CallScreen::Off,
            };
            if next == *screen {
                return false;
            }
            *screen = next;
            true
        });
        // A stop is counted before it is queued, under the lock that the
        // call task holds from its check of a finished pick until the
        // pick's start is queued for the worker. So a stop is either seen by
        // that check, and the pick is closed unused, or it comes after the
        // start and stops it. The stop also takes the portal session: it
        // closes here, so capture is revoked at once, also while the call
        // task waits for a key send. `Off` is then true when it shows.
        if !on {
            control.stops = control.stops.wrapping_add(1);
        }
        control.requests = control.requests.wrapping_add(1);
        let revoked = if on { None } else { control.session.take() };
        let (stops, request) = (control.stops, control.requests);
        drop(control);
        // Closing the session talks to the bus; not under the lock.
        drop(revoked);
        permit.send(Command::Screen { on, stops, request });
        drop(slot);
        if changed {
            self.inner.notify();
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

/// How this device rings, or `None` when it must not: another member is in
/// the call, the members cannot be read, or the room cannot take the ring
/// (ADR-0052).
async fn ring_plan(
    room: &matrix_sdk::Room,
    own_user: &matrix_sdk::ruma::UserId,
    own_device: &matrix_sdk::ruma::DeviceId,
) -> Option<Plan> {
    let members = read_members(room, now_ms()).await.ok()?;
    if members
        .iter()
        .any(|m| m.user_id != own_user || m.device_id != own_device)
    {
        return None;
    }
    let contact = crate::contacts::contact_of(&room.client(), room);
    // Without the own member event the power to notify the room is unknown:
    // no notification then.
    let may_notify = match room.get_member_no_sync(own_user).await {
        Ok(Some(member)) => member.can_trigger_room_notification(),
        _ => false,
    };
    rings::plan_ring(contact, room.joined_members_count(), may_notify)
}

/// Sends the ring once, as a normal room event (encrypted in an encrypted
/// room), and tells `watch` the event it created; `wake` wakes the call
/// task when a decline of it arrived first. A failure is dropped.
async fn send_ring(
    access: Access,
    room_id: OwnedRoomId,
    plan: Plan,
    membership_event: OwnedEventId,
    video: bool,
    watch: Option<(Arc<Watch>, mpsc::Sender<()>)>,
) {
    let content = rings::notification_content(&plan, membership_event.as_str(), now_ms(), video);
    let sent = access
        .run(|client| async move {
            let room = client.get_room(&room_id)?;
            time::timeout(
                SEND_TIMEOUT,
                room.send_raw(rings::NOTIFICATION_TYPE, content),
            )
            .await
            .ok()?
            .ok()
            .map(|response| response.response.event_id)
        })
        .await;
    // A decline that came before this answer counts now.
    if let (Ok(Some(event_id)), Some((watch, wake))) = (sent, watch)
        && watch.sent(event_id)
    {
        let _ = wake.try_send(());
    }
}

/// Watches the room for the decline of the ring this call sent. A match
/// sets the watch and wakes the call task.
fn decline_handler(
    room: &matrix_sdk::Room,
    watch: &Arc<Watch>,
    wake: mpsc::Sender<()>,
) -> EventHandlerHandle {
    #[derive(serde::Deserialize)]
    struct Decline {
        #[serde(rename = "type")]
        kind: String,
        sender: OwnedUserId,
        content: serde_json::Value,
    }
    let watch = watch.clone();
    room.add_event_handler(move |raw: Raw<AnySyncMessageLikeEvent>| {
        let watch = watch.clone();
        let wake = wake.clone();
        async move {
            if raw.json().get().len() > rings::MAX_EVENT_BYTES {
                return;
            }
            let Ok(event) = serde_json::from_str::<Decline>(raw.json().get()) else {
                return;
            };
            if event.kind == rings::DECLINE_TYPE && watch.observe(&event.sender, &event.content) {
                // A full queue already holds a pending change.
                let _ = wake.try_send(());
            }
        }
    })
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
    /// Dropping it (the call ends) cancels the request.
    portal: Option<PortalTask>,
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
    /// The ring this call sent, when it sent one to a direct chat.
    ring: Option<Arc<Watch>>,
    /// Who lets the user pick a screen to share (ADR-0053).
    screens: Arc<dyn ScreenSource>,
    /// The pick in flight: the user's answer to the core's own window or the
    /// portal's picker. Dropping it cancels the pick. A stop cancels it but
    /// keeps it here until its task ends, so at most one pick (and one
    /// warning window) exists at a time.
    screen_pick: Option<ScreenPick>,
    /// The latest screen request: `true` from a start until a stop. A pick
    /// that ends after a stop is closed unused.
    screen_wanted: bool,
    /// Fires when the desktop closes the portal session.
    screen_closed: Option<oneshot::Receiver<()>>,
    /// A `SetScreen` start went to the worker and no stop or end since.
    screen_sent: bool,
    /// The screen stops the handle accepted and the share's portal session.
    /// `set_screen` counts a stop and closes the session before it queues
    /// the stop. The task holds the lock from its check of a finished pick
    /// until the pick's start is queued.
    screen_control: Arc<Mutex<ScreenControl>>,
    /// A stop of share `n` that met a full worker queue. It goes as soon as
    /// the queue has room; no screen start goes before it.
    screen_stop_pending: Option<u32>,
    /// The stop count when the start of the pick in flight was accepted. A
    /// pick that ends after a later stop was accepted is closed unused.
    screen_pick_stops: u64,
    /// The stop count of the last screen command this task read. A higher
    /// count is a stop the handle accepted and showed, which this task has
    /// not read yet: until it does, the page's screen state stands.
    screen_seen_stops: u64,
    /// The number of the last screen command this task read. A higher
    /// number is a command the handle accepted and showed (`Off`, or
    /// `Starting` for a start after a stop) that this task has not read:
    /// until it does, the page's screen state stands.
    screen_seen_request: u64,
    /// The number of the last share that went to the worker. A worker state
    /// of another share is an older one's and is ignored.
    screen_share: u32,
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

/// A camera portal access request running on a blocking thread. Dropping it
/// sets the request's cancel flag: the request then closes its portal
/// dialog and ends within a fraction of a second, so the end of a call or a
/// profile lock does not wait for the user's answer.
struct PortalTask {
    task: JoinHandle<PortalRemote>,
    cancel: Arc<AtomicBool>,
}

impl Drop for PortalTask {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

/// Waits for the portal request in `slot`, if any; otherwise it never ends.
/// The request stays in `slot` when the wait is dropped, so the call loop
/// can wait for it again.
async fn portal_done(slot: &mut Option<PortalTask>) -> PortalRemote {
    let Some(portal) = slot.as_mut() else {
        return std::future::pending().await;
    };
    let result = (&mut portal.task).await;
    *slot = None;
    result.unwrap_or_default()
}

/// A screen pick running on a blocking thread. Dropping it sets the pick's
/// cancel flag: the window closes and the wait ends within a fraction of a
/// second, so the end of a call does not wait for the user's answer.
struct ScreenPick {
    task: JoinHandle<Result<ScreenGrant, ScreenRefusal>>,
    cancel: Arc<AtomicBool>,
}

impl Drop for ScreenPick {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

/// What a screen command changes at once, shared by the handle and the
/// call task under one lock (ADR-0053): the count of accepted stops, the
/// number of the last accepted screen command, and the share's open portal
/// session. Dropping the session closes it.
#[derive(Default)]
struct ScreenControl {
    stops: u64,
    requests: u64,
    session: Option<Box<dyn Send + Sync>>,
}

/// Waits for room in the worker's command queue while `sender` is given (a
/// screen stop waits); otherwise it never ends. `None` is a closed queue.
async fn screen_stop_room(
    sender: Option<mpsc::Sender<ToWorker>>,
) -> Option<mpsc::OwnedPermit<ToWorker>> {
    let Some(sender) = sender else {
        return std::future::pending().await;
    };
    sender.reserve_owned().await.ok()
}

/// Waits for the pick in `slot`, if any; otherwise it never ends. Gives the
/// pick's result and whether a stop cancelled the pick.
async fn screen_pick_done(
    slot: &mut Option<ScreenPick>,
) -> (Result<ScreenGrant, ScreenRefusal>, bool) {
    let Some(pick) = slot.as_mut() else {
        return std::future::pending().await;
    };
    let result = (&mut pick.task).await;
    let cancelled = pick.cancel.load(Ordering::SeqCst);
    *slot = None;
    (result.unwrap_or(Err(ScreenRefusal::Unavailable)), cancelled)
}

/// Waits until the desktop closes the portal session, if one is open;
/// otherwise it never ends.
async fn screen_session_closed(slot: &mut Option<oneshot::Receiver<()>>) {
    let Some(closed) = slot.as_mut() else {
        return std::future::pending().await;
    };
    // A dropped sender is a closed session too.
    let _ = closed.await;
    *slot = None;
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

/// Whether the homeserver refused a membership send for certain: it answered
/// with a client error. A timeout, a network failure or a server error may
/// hide a write, so those are not refusals.
fn membership_refused(error: &matrix_sdk::Error) -> bool {
    error
        .as_client_api_error()
        .is_some_and(|api| api.status_code.is_client_error())
}

/// How long the core waits for an ended worker to show its exit status.
const EXIT_STATUS_WAIT: Duration = Duration::from_millis(500);

/// The end reason for a worker that stopped: a crash when its exit status
/// shows an internal failure, else a plain stop.
fn stop_reason(exit: Option<WorkerExit>) -> EndReason {
    match exit {
        Some(exit) if exit_is_crash(exit) => EndReason::WorkerCrashed,
        _ => EndReason::WorkerStopped,
    }
}

async fn refine_worker_stop(call: &Call) -> EndReason {
    let process = call.link.kill_handle();
    let exit = tokio::task::spawn_blocking(move || {
        process
            .wait_exit(EXIT_STATUS_WAIT)
            .then(|| process.exit_status())
            .flatten()
    })
    .await
    .unwrap_or(None);
    stop_reason(exit)
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
    let reason = if reason == EndReason::WorkerStopped {
        refine_worker_stop(&call).await
    } else {
        reason
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
        } else if on && uses_portal(&self.link) {
            // A capture over a remote that went runs or opens: the worker
            // needs no second on. On the portal path every camera-on goes
            // with its own remote (`SetCamera { remote: true }`).
        } else {
            if !on {
                // The worker stops the capture and closes its remote.
                self.portal_capture = false;
                self.portal_sent = false;
            }
            self.send_worker(ToWorker::SetCamera { on, remote: false })?;
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

    /// A screen command from the handle, numbered `request`. The page kept
    /// its screen state while the command was unread; once it is read, the
    /// page is reconciled with the view, also when the command did not
    /// change the view (a start of a share the worker already reports `On`).
    /// `publish` skips an unchanged page.
    fn screen_request(
        &mut self,
        inner: &Inner,
        on: bool,
        stops: u64,
        request: u64,
    ) -> Result<(), EndReason> {
        self.screen_seen_request = request;
        self.screen_command(inner, on, stops)?;
        self.publish(inner);
        Ok(())
    }

    /// Starts or stops the screen share as the user asked (ADR-0053). A start
    /// asks the platform's source off the call loop: the user answers in a
    /// window the core or the desktop draws. A stop cancels a pick in flight,
    /// closes the portal session and stops the worker's capture.
    fn screen_command(&mut self, inner: &Inner, on: bool, stops: u64) -> Result<(), EndReason> {
        self.screen_wanted = on;
        self.screen_seen_stops = stops;
        if on {
            if !self.screens.available() {
                // The handle already shows `Starting`. The refusal is
                // published on every attempt, also when the task's view
                // did not change; `publish` skips an unchanged page.
                self.view.set_screen(CallScreen::Unavailable);
                self.publish(inner);
                return Ok(());
            }
            // A pick in flight or a share that runs is not started twice. A
            // stopped pick that has not ended yet holds the slot: the new
            // pick begins when it ends (`stopped_pick_ended`).
            match &self.screen_pick {
                None if !self.screen_sent => {
                    self.screen_pick_stops = stops;
                    self.begin_screen_pick();
                }
                Some(pick) if pick.cancel.load(Ordering::SeqCst) => {
                    self.screen_pick_stops = stops;
                }
                _ => {}
            }
            if self.view.request_screen(true) == Applied::Changed {
                self.publish(inner);
            }
            return Ok(());
        }
        // A stop: no pick, no session, no capture. The pick is cancelled; it
        // stays until its task ends, which closes its window.
        if let Some(pick) = &self.screen_pick {
            pick.cancel.store(true, Ordering::SeqCst);
        }
        self.screen_closed = None;
        self.close_screen_session();
        if self.screen_sent {
            self.screen_sent = false;
            self.send_screen_stop()?;
        }
        if self.view.request_screen(false) == Applied::Changed {
            self.publish(inner);
        }
        Ok(())
    }

    /// Asks the platform's source off the call loop.
    fn begin_screen_pick(&mut self) {
        let source = self.screens.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        self.screen_pick = Some(ScreenPick {
            task: tokio::task::spawn_blocking(move || source.choose(&flag)),
            cancel,
        });
    }

    /// A pick that a stop cancelled has ended. Its grant, if any, is dropped
    /// unused, which closes its session. A start that came while it ended
    /// begins its own pick now: the user answers a new warning.
    fn stopped_pick_ended(&mut self, picked: Result<ScreenGrant, ScreenRefusal>) {
        drop(picked);
        if self.screen_wanted && !self.screen_sent {
            self.begin_screen_pick();
        }
    }

    /// The pick ended. A grant goes to the worker: on Linux the remote first,
    /// then the command. A cancel, a refusal or a remote that could not go
    /// leaves the share off or unavailable; no other source is tried.
    fn screen_picked(
        &mut self,
        inner: &Inner,
        picked: Result<ScreenGrant, ScreenRefusal>,
    ) -> Result<(), EndReason> {
        // A stop accepted after this pick's start, even one still queued,
        // makes the pick stale. The lock stays held until the start is
        // queued, so no stop is accepted between the check and the handoff.
        // It is released before any publish: `set_screen` takes the slot
        // first and this lock second.
        let control = self.screen_control.clone();
        let mut accepted = plain(&control);
        let current = self.screen_wanted && self.screen_pick_stops == accepted.stops;
        let grant = match picked {
            Ok(grant) if current => grant,
            // A stop came meanwhile: the grant (and its session) is dropped.
            Ok(_) => return Ok(()),
            Err(refusal) => {
                drop(accepted);
                if current && self.view.set_screen(refusal.shown()) == Applied::Changed {
                    self.publish(inner);
                }
                self.screen_wanted = false;
                return Ok(());
            }
        };
        // A full command queue is a worker that has not read its commands
        // yet, not one that stopped: the share alone fails, the call runs on.
        // A stop still waiting for room goes first, so no start overtakes
        // it: the share fails too. The grant drops here, which closes its
        // session.
        if self.screen_stop_pending.is_some() || self.link.queue_full() {
            drop(accepted);
            self.screen_failed(inner);
            return Ok(());
        }
        let ScreenGrant {
            target,
            remote,
            session,
            closed,
        } = grant;
        // The remote goes first and the command says so by its target: the
        // worker takes the oldest screen remote with it.
        if !self.send_screen_remote(remote, &target) {
            drop(accepted);
            self.screen_failed(inner);
            return Ok(());
        }
        accepted.session = session;
        self.screen_closed = closed;
        self.screen_sent = true;
        self.screen_share = self.screen_share.wrapping_add(1);
        let sent = self.send_worker(ToWorker::SetScreen {
            target: Some(target),
            share: self.screen_share,
        });
        drop(accepted);
        sent
    }

    /// A share that could not start: unavailable, and the call runs on.
    fn screen_failed(&mut self, inner: &Inner) {
        self.screen_wanted = false;
        if self.view.set_screen(CallScreen::Unavailable) == Applied::Changed {
            self.publish(inner);
        }
    }

    /// Sends the portal's remote when the target needs one. Returns whether
    /// the command may follow.
    #[cfg(target_os = "linux")]
    fn send_screen_remote(
        &self,
        remote: Option<super::screen::Remote>,
        target: &persephone_call_protocol::ScreenTarget,
    ) -> bool {
        use persephone_call_protocol::ScreenTarget;
        use std::os::fd::AsFd;
        match (target, remote) {
            (ScreenTarget::Portal { .. }, Some(remote)) => {
                self.link.send_screen_handle(remote.as_fd()).is_ok()
            }
            (ScreenTarget::Portal { .. }, None) => false,
            (ScreenTarget::Monitor { .. }, _) => true,
        }
    }

    /// Off Linux there is no remote: a target that needs one is refused.
    #[cfg(not(target_os = "linux"))]
    fn send_screen_remote(
        &self,
        _remote: Option<super::screen::Remote>,
        target: &persephone_call_protocol::ScreenTarget,
    ) -> bool {
        matches!(
            target,
            persephone_call_protocol::ScreenTarget::Monitor { .. }
        )
    }

    /// The desktop closed the portal session (the user stopped the share
    /// from the desktop). The worker's capture ends with it: say so.
    fn screen_ended_by_desktop(&mut self, inner: &Inner) -> Result<(), EndReason> {
        self.close_screen_session();
        self.screen_wanted = false;
        if self.screen_sent {
            self.screen_sent = false;
            self.send_screen_stop()?;
        }
        if self.view.set_screen(CallScreen::Off) == Applied::Changed {
            self.publish(inner);
        }
        Ok(())
    }

    /// The worker reports the state of share `share`. Returns whether the
    /// page's state takes it.
    ///
    /// A state of an older share than the last one sent is ignored: it must
    /// not close the newer share's portal session or show over it. For the
    /// last share, any state but `Starting` and `On` ends it and closes its
    /// session. After the last share's stop, only its end state shows, and
    /// only while no new start waits for its pick: a late `Starting` or `On`
    /// of a stopped share never shows.
    fn screen_state_from_worker(&mut self, state: ScreenState, share: u32) -> bool {
        if share != self.screen_share {
            return false;
        }
        if !self.screen_sent {
            return !self.screen_wanted
                && !matches!(state, ScreenState::Starting | ScreenState::On);
        }
        let running = matches!(state, ScreenState::Starting | ScreenState::On);
        // The handle accepted a stop that this task has not read yet: the
        // share is about to stop, so the view does not take a running state.
        // (`publish` is what keeps the page's `Off`; this keeps the view
        // from holding a stale `On`.) A later start shows again.
        if running && plain(&self.screen_control).stops != self.screen_seen_stops {
            return false;
        }
        if !running {
            self.screen_sent = false;
            self.screen_closed = None;
            self.close_screen_session();
        }
        true
    }

    /// Closes the share's portal session, if open, outside the lock.
    fn close_screen_session(&self) {
        let session = plain(&self.screen_control).session.take();
        drop(session);
    }

    /// Sends the stop of the current share. A full queue keeps it as the one
    /// pending stop, which goes as soon as the worker reads its queue. Only a
    /// closed queue ends the call.
    fn send_screen_stop(&mut self) -> Result<(), EndReason> {
        if self.screen_stop_pending.is_some() {
            self.screen_stop_pending = Some(self.screen_share);
            return Ok(());
        }
        let stop = ToWorker::SetScreen {
            target: None,
            share: self.screen_share,
        };
        match self.link.try_queue(stop) {
            Ok(true) => Ok(()),
            Ok(false) => {
                self.screen_stop_pending = Some(self.screen_share);
                Ok(())
            }
            Err(_) => Err(EndReason::WorkerStopped),
        }
    }

    /// The queue has room for the pending stop (`permit`), or it closed.
    fn screen_stop_room_ready(
        &mut self,
        permit: Option<mpsc::OwnedPermit<ToWorker>>,
    ) -> Result<(), EndReason> {
        let permit = permit.ok_or(EndReason::WorkerStopped)?;
        if let Some(share) = self.screen_stop_pending.take() {
            permit.send(ToWorker::SetScreen {
                target: None,
                share,
            });
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
            let cancel = Arc::new(AtomicBool::new(false));
            let flag = cancel.clone();
            self.portal = Some(PortalTask {
                task: tokio::task::spawn_blocking(move || camera_portal::open_remote(&flag).ok()),
                cancel,
            });
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
    ///
    /// A capture over an earlier remote that reported `On` while this request
    /// ran (after a late `Off` of an older capture let the request start)
    /// keeps its state: the result belongs to no transition, and its remote
    /// is closed unused.
    fn portal_finished(&mut self, inner: &Inner, remote: PortalRemote) -> Result<(), EndReason> {
        if !self.camera_wanted.load(Ordering::SeqCst) || self.portal_capture {
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
        // The command says that its remote went first, so the worker takes
        // that remote with it and with no other command.
        self.send_worker(ToWorker::SetCamera {
            on: true,
            remote: true,
        })
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
    /// applied and synced: the read thread counted the change, and the tile
    /// keeps pictures again only once every counted change is applied here.
    /// The new stream number is then published, also when the metadata did
    /// not change, and the page asks for pictures of that stream only. So no
    /// picture of a new stream shows under the old participant, even on a
    /// page whose status is older.
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
        if let FromWorker::Screen { state, share } = event
            && !self.screen_state_from_worker(*state, *share)
        {
            return Ok(());
        }
        let changed = match self.view.apply(event) {
            Applied::Same => false,
            Applied::Changed => {
                self.sync_video();
                true
            }
            Applied::End(reason) => return Err(reason),
        };
        let stream = match event {
            FromWorker::VideoStarted { tile, .. } | FromWorker::VideoStopped { tile } => {
                let stream = self.link.video().stream_applied(*tile);
                self.view.set_video_stream(*tile, stream);
                true
            }
            _ => false,
        };
        if changed || stream {
            self.publish(inner);
        }
        Ok(())
    }

    fn send_worker(&self, message: ToWorker) -> Result<(), EndReason> {
        self.link
            .send(message)
            .map_err(|_| EndReason::WorkerStopped)
    }

    /// Publishes the view. The check for an unread screen command and the
    /// page write are one step under the lock that `set_screen` holds while
    /// it shows a command and counts it. While the handle has a screen
    /// command this task has not read, the page keeps its screen state
    /// (`Off` for a stop, `Starting` for a start): a stale `On` never shows
    /// over a stop, and the stop's `Off` never shows over a later start,
    /// also when an unrelated change publishes. Never called with that lock
    /// held.
    fn publish(&self, inner: &Inner) {
        let mut next = self.view.state();
        let changed = {
            let control = plain(&self.screen_control);
            let unread = control.stops != self.screen_seen_stops
                || control.requests != self.screen_seen_request;
            inner.state.send_if_modified(move |current| {
                if unread
                    && let (
                        CallState::Active { screen: shown, .. },
                        CallState::Active { screen, .. },
                    ) = (&*current, &mut next)
                {
                    *screen = *shown;
                }
                if *current == next {
                    return false;
                }
                *current = next;
                true
            })
        };
        if changed {
            inner.notify();
        }
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
                    Some(Command::Screen { on, stops, request }) => {
                        self.screen_request(inner, on, stops, request)?;
                    }
                    None => return Ok(()),
                },
                remote = portal_done(&mut self.portal) => self.portal_finished(inner, remote)?,
                (picked, cancelled) = screen_pick_done(&mut self.screen_pick) => {
                    if cancelled {
                        self.stopped_pick_ended(picked);
                    } else {
                        self.screen_picked(inner, picked)?;
                    }
                }
                permit = screen_stop_room(
                    self.screen_stop_pending.map(|_| self.link.command_sender()),
                ) => self.screen_stop_room_ready(permit)?,
                () = screen_session_closed(&mut self.screen_closed) => {
                    self.screen_ended_by_desktop(inner)?;
                }
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
        // A decline of the ring shows even when the members cannot be read.
        if self.ring.as_ref().is_some_and(|ring| ring.declined()) && !self.view.ring_declined {
            self.view.ring_declined = true;
            self.publish(inner);
        }
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
        // The video filter follows the members at once, so a later error
        // cannot leave a departed member's picture allowed. A video stream
        // may also have started before its member was known.
        self.sync_video();
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
            portal,
            screen_pick,
            screen_control,
            screen_closed,
            ..
        } = self;
        // A portal request still waiting for the user is cancelled first, so
        // its dialog does not stay open while the call is torn down. The
        // screen pick and the share's portal session end with the call.
        drop(portal);
        drop(screen_pick);
        drop(screen_closed);
        let session = plain(&screen_control).session.take();
        drop(session);
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

    /// GIVEN a homeserver answer to a request WHEN the failure is classified
    /// THEN a 4xx refusal is certain (the cleanup cancels the delayed leave),
    /// and a 5xx may hide a write (the cleanup sends it).
    #[tokio::test]
    async fn given_membership_failures_then_only_a_client_error_is_a_refusal() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        for (status, errcode, refused) in [
            (403, "M_FORBIDDEN", true),
            (400, "M_BAD_JSON", true),
            (429, "M_LIMIT_EXCEEDED", true),
            (500, "M_UNKNOWN", false),
        ] {
            let server = MockServer::start().await;
            let client = crate::calls::readiness::tests::signed_in(&server).await;
            Mock::given(method("GET"))
                .and(path("/_matrix/client/v3/voip/turnServer"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .set_body_json(serde_json::json!({ "errcode": errcode, "error": "no" })),
                )
                .mount(&server)
                .await;
            let error = client
                .send(matrix_sdk::ruma::api::client::voip::get_turn_server_info::v3::Request::new())
                .await
                .unwrap_err();
            assert_eq!(
                membership_refused(&matrix_sdk::Error::from(error)),
                refused,
                "{status}"
            );
        }
    }

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

    /// GIVEN a connected call WHEN its ring was declined THEN the active state
    /// shows it and the call goes on.
    #[test]
    fn given_a_declined_ring_then_the_call_shows_it_and_goes_on() {
        let mut view = view();
        view.apply(&FromWorker::Connected);
        let CallState::Active { ring_declined, .. } = view.state() else {
            panic!("active");
        };
        assert!(!ring_declined);
        view.ring_declined = true;
        let CallState::Active { ring_declined, .. } = view.state() else {
            panic!("active");
        };
        assert!(ring_declined);
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

    /// GIVEN a stopped worker WHEN its exit status is read THEN a crash status
    /// ends the call as crashed, and a clean exit, a kill or an unknown status
    /// ends it as stopped.
    #[test]
    fn given_worker_exit_then_the_stop_reason_names_crashes() {
        assert_eq!(
            stop_reason(Some(WorkerExit::Code(101))),
            EndReason::WorkerCrashed
        );
        assert_eq!(
            stop_reason(Some(WorkerExit::Code(0))),
            EndReason::WorkerStopped
        );
        assert_eq!(
            stop_reason(Some(WorkerExit::Code(1))),
            EndReason::WorkerStopped
        );
        assert_eq!(stop_reason(None), EndReason::WorkerStopped);
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
            screen_control: Arc::default(),
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
        let video_streams = link.video().streams();
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
            view: view().with_video_streams(video_streams),
            changes,
            incoming,
            failed,
            handles: Vec::new(),
            tasks: JoinSet::new(),
            camera_wanted: Arc::new(AtomicBool::new(false)),
            portal: None,
            portal_capture: false,
            portal_sent: false,
            ring: None,
            screens: crate::calls::screen::unavailable(),
            screen_pick: None,
            screen_wanted: false,
            screen_closed: None,
            screen_sent: false,
            screen_control: Arc::default(),
            screen_pick_stops: 0,
            screen_seen_stops: 0,
            screen_seen_request: 0,
            screen_share: 0,
            screen_stop_pending: None,
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

    /// GIVEN a granted portal request WHEN the camera is still wanted THEN
    /// the remote goes over the camera channel first, and the camera-on that
    /// follows says it carries a remote, so the worker takes that one.
    #[cfg(target_os = "linux")]
    #[test]
    fn given_a_granted_portal_request_then_the_camera_on_carries_its_remote() {
        use std::io::Read;
        let fixture = test_call(
            no_admission(),
            Arc::new(Flags::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let inner = controller(FakeHost::new(true, None)).inner.clone();
        let (mut call, mut to_worker) = (fixture.call, fixture.to_worker);
        let (core_end, mut worker_end) = std::os::unix::net::UnixStream::pair().unwrap();
        call.link = std::mem::replace(
            &mut call.link,
            CallWorkerLink::from_parts(
                mpsc::channel(1).0,
                mpsc::channel(1).1,
                Box::new(FakeProcess(Arc::new(AtomicBool::new(false)))),
            ),
        )
        .with_camera_path(crate::calls::worker::CameraPath::Portal)
        .with_camera_channel(core_end);
        call.camera_wanted.store(true, Ordering::SeqCst);
        let remote = std::os::fd::OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
        call.portal_finished(&inner, Some(remote)).unwrap();
        let mut byte = [0_u8; 1];
        assert_eq!(worker_end.read(&mut byte).unwrap(), 1, "the remote went");
        assert!(matches!(
            to_worker.try_recv(),
            Ok(ToWorker::SetCamera {
                on: true,
                remote: true
            })
        ));
        assert!(call.portal_capture && call.portal_sent);
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

    /// GIVEN a portal capture B that reports `On` while a second portal
    /// request runs (the user turned the camera on after a late `Off` of an
    /// older capture) WHEN that request fails THEN B stays `On`, and nothing
    /// goes to the worker; WHEN the camera is turned on again THEN no
    /// camera-on goes to the worker without its own remote.
    #[test]
    fn given_a_live_capture_then_a_failed_portal_request_keeps_its_state() {
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
        call.camera_wanted.store(true, Ordering::SeqCst);
        // B's remote went; A's late `Off` cleared the flag.
        call.portal_sent = true;
        call.worker_event(
            &inner,
            &FromWorker::Camera {
                state: CameraState::Off,
            },
        )
        .unwrap();
        assert!(call.portal_needed(true), "a new request may start");
        // B reports `On`, then the request fails.
        call.worker_event(
            &inner,
            &FromWorker::Camera {
                state: CameraState::On,
            },
        )
        .unwrap();
        finish_without_remote(&mut call, &inner);
        assert_eq!(call.view.camera, CallCamera::On, "B's state is kept");
        assert!(to_worker.try_recv().is_err());
        // A repeated on reuses B; no camera-on goes without a remote.
        call.camera_command(&inner, true, 0).unwrap();
        assert!(to_worker.try_recv().is_err(), "no on without its remote");
        assert_eq!(call.view.camera, CallCamera::On);
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
            Ok(ToWorker::SetCamera { on: false, .. })
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
        let a_stream = store.stream(1);
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
        // A page that still holds A's stream number gets no picture of B.
        assert_eq!(tiles[0].stream, store.stream(1));
        assert!(
            store.newest(1, 0, a_stream).is_none(),
            "A's stream: nothing"
        );
        assert!(
            store.newest(1, 0, tiles[0].stream).is_some(),
            "B's stream: B"
        );
        let CallState::Active { video, .. } = inner.state.borrow().clone() else {
            panic!("an active call");
        };
        assert_eq!(video, tiles, "the page sees B's stream number");
    }

    /// GIVEN tile 2 shows member A WHEN a second `VideoStarted` for A
    /// arrives (a new stream, the same metadata) THEN the new stream number
    /// is still published, so the page asks for the new stream's pictures.
    #[test]
    fn given_a_stream_change_without_new_metadata_then_its_number_is_published() {
        let fixture = test_call(
            no_admission(),
            Arc::new(Flags::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let inner = controller(FakeHost::new(true, None)).inner.clone();
        let mut call = fixture.call;
        call.worker_event(&inner, &FromWorker::Connected).unwrap();
        let mut seen = Vec::new();
        for _ in 0..2 {
            call.worker_event(&inner, &started(2, "@a:example.org:A"))
                .unwrap();
            let CallState::Active { video, .. } = inner.state.borrow().clone() else {
                panic!("an active call");
            };
            assert_eq!(video.len(), 1);
            assert_eq!(video[0].tile, 2);
            assert_eq!(video[0].stream, call.link.video().stream(2));
            seen.push(video[0].stream);
        }
        assert_ne!(seen[0], seen[1], "the second stream has a new number");
    }

    /// GIVEN a portal request waiting for the user WHEN the call drops its
    /// task (the call or the session ends) THEN the request's cancel flag is
    /// set, so the blocking wait ends and the runtime's shutdown does not
    /// wait for the user.
    #[tokio::test]
    async fn given_a_portal_request_when_its_task_is_dropped_then_it_is_cancelled() {
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let (ended, ended_rx) = std::sync::mpsc::channel();
        let portal = PortalTask {
            task: tokio::task::spawn_blocking(move || {
                // The deadline only keeps a regression from hanging the
                // runtime's shutdown; the test reports it.
                let deadline = Instant::now() + Duration::from_secs(5);
                while !flag.load(Ordering::SeqCst) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                if flag.load(Ordering::SeqCst) {
                    let _ = ended.send(());
                }
                PortalRemote::default()
            }),
            cancel: cancel.clone(),
        };
        drop(portal);
        assert!(cancel.load(Ordering::SeqCst));
        assert!(
            ended_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "the wait ended"
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
            screen_control: call.screen_control.clone(),
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
                own: true,
                stream: 0,
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
        assert_eq!(
            view.video_allowed(),
            (false, [true, false, false, false, false])
        );
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
                stream: 0,
            }]
        );
        view.set_members(vec![member(
            "@stranger:example.org",
            "S",
            "https://sfu.example.org",
        )]);
        assert_eq!(view.video_allowed().1, [false, true, false, false, false]);
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
        assert_eq!(view.apply(&started(6, "@a:example.org:A")), Applied::Same);
        assert_eq!(
            view.apply(&FromWorker::VideoStopped { tile: 0 }),
            Applied::Same
        );
        assert_eq!(
            view.apply(&FromWorker::VideoStopped { tile: 6 }),
            Applied::Same
        );
        assert_eq!(view.video_allowed().1, [true, false, true, false, false]);
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
        assert_eq!(view.video_allowed().1, [false; 5]);
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
            screen_control: Arc::default(),
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
            screen: CallScreen::Off,
            video: Vec::new(),
            ring_declined: false,
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
        store.sync(true, [false; 5]);
        assert!(store.insert(0, 0, 2, 2, vec![1; 16]));
        calls.inner.publish(CallState::Active {
            room_id: room_id!("!r:example.org").to_owned(),
            muted: false,
            participants: Vec::new(),
            camera: CallCamera::On,
            screen: CallScreen::Off,
            video: vec![CallVideoTile {
                tile: 0,
                participant: None,
                own: true,
                stream: 0,
            }],
            ring_declined: false,
        });
        assert!(calls.video_picture(0, 0, store.stream(0)).is_some());
        assert_eq!(calls.set_camera(false), Ok(()));
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::Camera { on: false, .. })
        ));
        assert!(
            calls.video_picture(0, 0, store.stream(0)).is_none(),
            "the picture is gone"
        );
        assert!(
            !store.insert(0, 0, 2, 2, vec![2; 16]),
            "a late own picture is refused before the task takes the command"
        );
        // An unrelated event makes the task sync while its view still has
        // the camera on: the own tile stays shut.
        store.sync(true, [false; 5]);
        assert!(!store.insert(0, 0, 2, 2, vec![3; 16]));
        assert!(calls.video_picture(0, 0, store.stream(0)).is_none());
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
        store.sync(true, [false; 5]);
        assert!(store.insert(0, 0, 2, 2, vec![1; 16]));
        assert_eq!(calls.set_camera(true), Ok(()));
        assert_eq!(calls.set_camera(false), Ok(()));
        let Ok(Command::Camera { on: true, request }) = commands.try_recv() else {
            panic!("the on comes first");
        };
        // What `camera_command` does to the store for the older on.
        store.camera_on_taken(request);
        store.sync(true, [false; 5]);
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
        store.sync(true, [false; 5]);
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
        store.sync(true, [false; 5]);
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
        assert!(
            calls.video_picture(0, 0, store.stream(0)).is_some(),
            "the picture stays"
        );
    }

    /// GIVEN a picture in the store WHEN the page asks THEN only a newer
    /// picture is returned; with no call there is none.
    #[tokio::test]
    async fn given_a_stored_picture_then_the_controller_returns_newer_ones_only() {
        let calls = controller(FakeHost::new(true, None));
        assert!(
            calls.video_picture(1, 0, 0).is_none(),
            "no call, no picture"
        );
        let (store, _commands, _) = running_call(&calls, active_state());
        store.sync(false, [true, false, false, false, false]);
        assert!(store.insert(1, 3, 2, 2, vec![5; 16]));
        let picture = calls.video_picture(1, 0, store.stream(1)).expect("picture");
        assert_eq!((picture.seq, picture.rotation), (1, 3));
        assert!(calls.video_picture(1, 1, store.stream(1)).is_none());
        assert!(
            calls.video_picture(1, 0, store.stream(1) + 1).is_none(),
            "another stream number gets nothing"
        );
        assert!(calls.video_picture(9, 0, store.stream(9)).is_none());
    }

    /// GIVEN pictures in a running call WHEN the session ends THEN every
    /// picture is dropped and later ones are refused; WHEN the call ends by
    /// itself THEN the same holds.
    #[tokio::test]
    async fn given_the_call_ends_then_every_picture_is_dropped() {
        for session_end in [true, false] {
            let calls = controller(FakeHost::new(true, None));
            let (store, _commands, _) = running_call(&calls, active_state());
            store.sync(true, [true; 5]);
            assert!(store.insert(0, 0, 2, 2, vec![1; 16]));
            assert!(store.insert(4, 0, 2, 2, vec![1; 16]));
            if session_end {
                calls.stop_for_session_end();
            } else {
                assert!(calls.inner.release(5));
            }
            assert!(calls.video_picture(0, 0, store.stream(0)).is_none());
            assert!(calls.video_picture(4, 0, store.stream(4)).is_none());
            store.sync(true, [true; 5]);
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
        store.sync(true, [true; 5]);
        drop(guard);
        assert!(!store.insert(0, 0, 2, 2, vec![1; 16]));
        assert!(plain(&calls.inner.video).is_none());
    }

    // ----- Screen share (ADR-0053) -----

    /// A drop guard for the portal session: it flips a flag when the session
    /// closes.
    struct SessionGuard(Arc<AtomicBool>);

    impl Drop for SessionGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn monitor() -> persephone_call_protocol::ScreenTarget {
        persephone_call_protocol::ScreenTarget::Monitor {
            index: 1,
            width: 1920,
            height: 1080,
        }
    }

    /// A grant of one monitor, with a session guard. `dropped` becomes true
    /// when the session closes.
    fn grant(dropped: &Arc<AtomicBool>) -> ScreenGrant {
        ScreenGrant {
            target: monitor(),
            remote: None,
            session: Some(Box::new(SessionGuard(dropped.clone()))),
            closed: None,
        }
    }

    fn is_monitor_start(message: Option<ToWorker>) -> bool {
        matches!(
            message,
            Some(ToWorker::SetScreen {
                target: Some(persephone_call_protocol::ScreenTarget::Monitor {
                    index: 1,
                    width: 1920,
                    height: 1080
                }),
                ..
            })
        )
    }

    /// A source that answers every pick with a cancel and counts the picks.
    struct Scripted {
        available: bool,
        picks: AtomicUsize,
    }

    impl ScreenSource for Scripted {
        fn available(&self) -> bool {
            self.available
        }

        fn choose(&self, _cancel: &Arc<AtomicBool>) -> Result<ScreenGrant, ScreenRefusal> {
            self.picks.fetch_add(1, Ordering::SeqCst);
            Err(ScreenRefusal::Cancelled)
        }
    }

    /// A source whose window stays open until the pick is cancelled, like a
    /// warning nobody answers. Counts the picks.
    #[derive(Default)]
    struct Unanswered {
        picks: AtomicUsize,
    }

    impl ScreenSource for Unanswered {
        fn available(&self) -> bool {
            true
        }

        fn choose(&self, cancel: &Arc<AtomicBool>) -> Result<ScreenGrant, ScreenRefusal> {
            self.picks.fetch_add(1, Ordering::SeqCst);
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !cancel.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(ScreenRefusal::Cancelled)
        }
    }

    /// GIVEN a pick whose window is open WHEN the share stops and starts
    /// again before that window closes THEN the stopped pick is cancelled
    /// and keeps the slot, no second pick runs beside it, and the new pick
    /// begins only once the stopped one has ended.
    #[tokio::test]
    async fn given_a_stopped_pick_still_open_when_a_start_comes_then_it_waits_for_the_end() {
        let (mut fixture, inner, _) = call_with_source(true);
        let source = Arc::new(Unanswered::default());
        fixture.call.screens = source.clone();
        fixture.call.screen_command(&inner, true, 0).unwrap();
        fixture.call.screen_command(&inner, false, 1).unwrap();
        let first = fixture
            .call
            .screen_pick
            .as_ref()
            .expect("the stopped pick stays")
            .cancel
            .clone();
        assert!(first.load(Ordering::SeqCst), "the stop cancelled it");
        fixture.call.screen_command(&inner, true, 1).unwrap();
        assert!(
            Arc::ptr_eq(&first, &fixture.call.screen_pick.as_ref().unwrap().cancel),
            "no second pick beside the stopped one"
        );
        assert_eq!(fixture.call.view.screen, CallScreen::Starting);

        let (picked, cancelled) = screen_pick_done(&mut fixture.call.screen_pick).await;
        assert!(cancelled);
        fixture.call.stopped_pick_ended(picked);
        let second = fixture
            .call
            .screen_pick
            .as_ref()
            .expect("the waiting start's pick")
            .cancel
            .clone();
        assert!(!Arc::ptr_eq(&first, &second));
        assert!(!second.load(Ordering::SeqCst), "it needs its own answer");
        second.store(true, Ordering::SeqCst);
        let _ = screen_pick_done(&mut fixture.call.screen_pick).await;
        assert_eq!(source.picks.load(Ordering::SeqCst), 2);
        assert!(fixture.to_worker.try_recv().is_err());
    }

    /// GIVEN a pick that a stop cancelled WHEN it ends with a grant, and no
    /// start came THEN the grant is closed unused and no new pick begins.
    #[tokio::test]
    async fn given_a_stopped_pick_that_ends_with_a_grant_then_it_is_closed_unused() {
        let (mut fixture, inner, source) = call_with_source(true);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        fixture.call.screen_command(&inner, false, 1).unwrap();
        let _ = screen_pick_done(&mut fixture.call.screen_pick).await;
        let dropped = Arc::new(AtomicBool::new(false));
        fixture.call.stopped_pick_ended(Ok(grant(&dropped)));
        assert!(dropped.load(Ordering::SeqCst), "the session closed");
        assert!(fixture.call.screen_pick.is_none());
        assert_eq!(source.picks.load(Ordering::SeqCst), 1);
        assert!(fixture.to_worker.try_recv().is_err());
        assert!(!fixture.call.screen_sent);
    }

    fn call_with_source(available: bool) -> (TestCall, Arc<Inner>, Arc<Scripted>) {
        let mut fixture = test_call(
            no_admission(),
            Arc::new(Flags::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let source = Arc::new(Scripted {
            available,
            picks: AtomicUsize::new(0),
        });
        fixture.call.screens = source.clone();
        let inner = controller(FakeHost::new(true, None)).inner.clone();
        (fixture, inner, source)
    }

    /// GIVEN a platform without screen sharing WHEN the page starts a share
    /// THEN the share is unavailable, nobody is asked, and nothing goes to
    /// the worker.
    #[tokio::test]
    async fn given_no_screen_source_when_a_share_starts_then_it_is_unavailable() {
        let (mut fixture, inner, source) = call_with_source(false);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        assert_eq!(fixture.call.view.screen, CallScreen::Unavailable);
        assert_eq!(source.picks.load(Ordering::SeqCst), 0);
        assert!(fixture.to_worker.try_recv().is_err());
        assert!(fixture.call.screen_pick.is_none());
    }

    /// GIVEN a share that starts WHEN the user cancels the chooser THEN the
    /// share is off and nothing goes to the worker; WHEN the pick is refused
    /// THEN the share shows refused; neither falls back to another source.
    #[tokio::test]
    async fn given_a_cancelled_or_refused_pick_then_the_share_stays_off() {
        let (mut fixture, inner, _) = call_with_source(true);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        assert_eq!(fixture.call.view.screen, CallScreen::Starting);
        assert!(fixture.call.screen_pick.is_some());
        fixture
            .call
            .screen_picked(&inner, Err(ScreenRefusal::Cancelled))
            .unwrap();
        assert_eq!(fixture.call.view.screen, CallScreen::Off);
        assert!(fixture.to_worker.try_recv().is_err());

        fixture.call.screen_command(&inner, true, 0).unwrap();
        fixture
            .call
            .screen_picked(&inner, Err(ScreenRefusal::Refused))
            .unwrap();
        assert_eq!(fixture.call.view.screen, CallScreen::Refused);
        assert!(fixture.to_worker.try_recv().is_err());
        assert!(!fixture.call.screen_sent);
    }

    /// GIVEN a pick in flight WHEN a second start comes THEN no second pick
    /// runs; WHEN the user picks THEN the worker gets one command with the
    /// chosen screen and the session stays open.
    #[tokio::test]
    async fn given_a_grant_then_the_worker_gets_the_command_with_the_chosen_screen() {
        let (mut fixture, inner, source) = call_with_source(true);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        fixture.call.screen_command(&inner, true, 0).unwrap();
        // The scripted source answers by cancel; wait for the one pick.
        let mut pick = fixture.call.screen_pick.take().expect("a pick");
        let _ = (&mut pick.task).await;
        assert_eq!(source.picks.load(Ordering::SeqCst), 1, "one pick only");
        let dropped = Arc::new(AtomicBool::new(false));
        fixture
            .call
            .screen_picked(&inner, Ok(grant(&dropped)))
            .unwrap();
        assert!(is_monitor_start(fixture.to_worker.try_recv().ok()));
        assert!(fixture.to_worker.try_recv().is_err(), "one command only");
        assert!(fixture.call.screen_sent);
        assert!(!dropped.load(Ordering::SeqCst), "the session stays open");
    }

    /// GIVEN a running share WHEN the page stops it THEN the worker is told,
    /// the session closes and the state is off; a second stop sends nothing.
    #[tokio::test]
    async fn given_a_running_share_when_it_is_stopped_then_the_session_closes() {
        let (mut fixture, inner, _) = call_with_source(true);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        fixture
            .call
            .screen_picked(&inner, Ok(grant(&dropped)))
            .unwrap();
        let _ = fixture.to_worker.try_recv();
        fixture.call.screen_command(&inner, false, 0).unwrap();
        assert!(matches!(
            fixture.to_worker.try_recv(),
            Ok(ToWorker::SetScreen { target: None, .. })
        ));
        assert!(dropped.load(Ordering::SeqCst), "the session closed");
        assert_eq!(fixture.call.view.screen, CallScreen::Off);
        fixture.call.screen_command(&inner, false, 0).unwrap();
        assert!(fixture.to_worker.try_recv().is_err());
    }

    /// GIVEN a pick that ends after the user stopped THEN the grant is
    /// dropped (its session closes) and nothing goes to the worker.
    #[tokio::test]
    async fn given_a_pick_that_ends_after_a_stop_then_it_is_closed_unused() {
        let (mut fixture, inner, _) = call_with_source(true);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        fixture.call.screen_command(&inner, false, 0).unwrap();
        assert!(
            fixture
                .call
                .screen_pick
                .as_ref()
                .is_some_and(|pick| pick.cancel.load(Ordering::SeqCst)),
            "the stop cancelled the pick"
        );
        let dropped = Arc::new(AtomicBool::new(false));
        fixture
            .call
            .screen_picked(&inner, Ok(grant(&dropped)))
            .unwrap();
        assert!(dropped.load(Ordering::SeqCst));
        assert!(fixture.to_worker.try_recv().is_err());
        assert!(!fixture.call.screen_sent);
    }

    /// GIVEN a running share WHEN the worker reports each state THEN the
    /// share's portal session closes for every end state and the shown state
    /// follows; `On` and `Starting` keep the session.
    #[tokio::test]
    async fn given_the_worker_reports_a_share_state_then_the_session_follows() {
        for (state, shown, closes) in [
            (ScreenState::Starting, CallScreen::Starting, false),
            (ScreenState::On, CallScreen::On, false),
            (ScreenState::NotFound, CallScreen::NotFound, true),
            (ScreenState::Unavailable, CallScreen::Unavailable, true),
            (ScreenState::Refused, CallScreen::Refused, true),
            (ScreenState::Off, CallScreen::Off, true),
        ] {
            let (mut fixture, inner, _) = call_with_source(true);
            fixture.call.screen_command(&inner, true, 0).unwrap();
            let dropped = Arc::new(AtomicBool::new(false));
            fixture
                .call
                .screen_picked(&inner, Ok(grant(&dropped)))
                .unwrap();
            fixture
                .call
                .worker_event(
                    &inner,
                    &FromWorker::Screen {
                        state,
                        share: fixture.call.screen_share,
                    },
                )
                .unwrap();
            assert_eq!(fixture.call.view.screen, shown, "{state:?}");
            assert_eq!(dropped.load(Ordering::SeqCst), closes, "{state:?}");
            assert_eq!(fixture.call.screen_sent, !closes, "{state:?}");
        }
    }

    /// GIVEN a pick in flight WHEN the handle accepted a stop that the task
    /// has not read yet and the pick then ends THEN the grant is closed
    /// unused and nothing goes to the worker.
    #[tokio::test]
    async fn given_a_stop_accepted_before_the_pick_ends_then_the_grant_is_closed_unused() {
        let (mut fixture, inner, _) = call_with_source(true);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        // `set_screen(false)` counts the stop before it queues it.
        plain(&fixture.call.screen_control).stops += 1;
        let dropped = Arc::new(AtomicBool::new(false));
        fixture
            .call
            .screen_picked(&inner, Ok(grant(&dropped)))
            .unwrap();
        assert!(dropped.load(Ordering::SeqCst), "the session closed");
        assert!(fixture.to_worker.try_recv().is_err());
        assert!(!fixture.call.screen_sent);
    }

    /// GIVEN a stop that the handle is accepting (it holds the stop lock)
    /// WHEN a pick ends at that moment THEN the pick's check waits for the
    /// stop, sees it, and closes the grant unused: nothing goes to the
    /// worker.
    #[tokio::test]
    async fn given_a_stop_accepted_while_a_pick_ends_then_the_check_waits_and_sees_it() {
        let (mut fixture, inner, _) = call_with_source(true);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        let control = fixture.call.screen_control.clone();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let stopper = std::thread::spawn(move || {
            let mut control = plain(&control);
            held_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            control.stops += 1;
        });
        held_rx.recv().unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        fixture
            .call
            .screen_picked(&inner, Ok(grant(&dropped)))
            .unwrap();
        stopper.join().unwrap();
        assert!(dropped.load(Ordering::SeqCst), "the session closed");
        assert!(fixture.to_worker.try_recv().is_err());
        assert!(!fixture.call.screen_sent);
    }

    /// GIVEN a share whose start went to the worker WHEN a stop is accepted
    /// just after THEN the stop names that share and closes its session, and
    /// the share's late `Starting` and `On` do not show over the stop; its
    /// end state does.
    #[tokio::test]
    async fn given_a_stop_just_after_the_handoff_then_late_states_do_not_show_it_running() {
        let (mut fixture, inner, _) = call_with_source(true);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        fixture
            .call
            .screen_picked(&inner, Ok(grant(&dropped)))
            .unwrap();
        let share = fixture.call.screen_share;
        assert!(is_monitor_start(fixture.to_worker.try_recv().ok()));
        plain(&fixture.call.screen_control).stops += 1;
        fixture.call.screen_command(&inner, false, 1).unwrap();
        assert!(matches!(
            fixture.to_worker.try_recv(),
            Ok(ToWorker::SetScreen { target: None, share: stopped }) if stopped == share
        ));
        assert!(dropped.load(Ordering::SeqCst), "the session closed");
        for state in [ScreenState::Starting, ScreenState::On] {
            fixture
                .call
                .worker_event(&inner, &FromWorker::Screen { state, share })
                .unwrap();
            assert_eq!(fixture.call.view.screen, CallScreen::Off, "{state:?}");
        }
        fixture
            .call
            .worker_event(
                &inner,
                &FromWorker::Screen {
                    state: ScreenState::Unavailable,
                    share,
                },
            )
            .unwrap();
        assert_eq!(fixture.call.view.screen, CallScreen::Unavailable);
    }

    /// GIVEN a share whose start went to the worker WHEN the handle accepts a
    /// stop (the page shows `Off`) and the worker reports `On` before the
    /// task reads the stop THEN `On` does not show over `Off`; WHEN a later
    /// share starts THEN its `On` shows.
    #[tokio::test]
    async fn given_an_accepted_stop_when_on_arrives_first_then_off_stays_and_a_later_share_shows() {
        let (mut fixture, inner, _) = call_with_source(true);
        fixture.call.view.apply(&FromWorker::Connected);
        let shown = |inner: &Inner| match &*inner.state.borrow() {
            CallState::Active { screen, .. } => *screen,
            other => panic!("{other:?}"),
        };
        fixture.call.screen_command(&inner, true, 0).unwrap();
        let first = Arc::new(AtomicBool::new(false));
        fixture
            .call
            .screen_picked(&inner, Ok(grant(&first)))
            .unwrap();
        let share = fixture.call.screen_share;
        assert!(is_monitor_start(fixture.to_worker.try_recv().ok()));

        // What `set_screen(false)` does before the task reads the stop.
        plain(&fixture.call.screen_control).stops += 1;
        inner.state.send_modify(|state| {
            if let CallState::Active { screen, .. } = state {
                *screen = CallScreen::Off;
            }
        });
        fixture
            .call
            .worker_event(
                &inner,
                &FromWorker::Screen {
                    state: ScreenState::On,
                    share,
                },
            )
            .unwrap();
        assert_ne!(fixture.call.view.screen, CallScreen::On);
        assert_eq!(shown(&inner), CallScreen::Off);

        fixture.call.screen_command(&inner, false, 1).unwrap();
        assert_eq!(shown(&inner), CallScreen::Off);
        fixture.call.screen_command(&inner, true, 1).unwrap();
        let second = Arc::new(AtomicBool::new(false));
        fixture
            .call
            .screen_picked(&inner, Ok(grant(&second)))
            .unwrap();
        let later = fixture.call.screen_share;
        assert_ne!(later, share);
        fixture
            .call
            .worker_event(
                &inner,
                &FromWorker::Screen {
                    state: ScreenState::On,
                    share: later,
                },
            )
            .unwrap();
        assert_eq!(fixture.call.view.screen, CallScreen::On);
        assert_eq!(shown(&inner), CallScreen::On);
    }

    /// What `set_screen(false)` does under the control lock: the page shows
    /// `Off` and the stop is counted.
    fn accept_stop(control: &Mutex<ScreenControl>, inner: &Inner) {
        let mut control = plain(control);
        inner.state.send_modify(|state| {
            if let CallState::Active { screen, .. } = state {
                *screen = CallScreen::Off;
            }
        });
        control.stops += 1;
    }

    /// GIVEN a running share the page shows `On` WHEN a stop is accepted
    /// after that publication THEN the page shows `Off`; WHEN an unrelated
    /// change (mute) and a late `On` publish before the task reads the stop
    /// THEN the mute shows and `Off` stays; WHEN the task reads the stop and
    /// a later share starts THEN its `On` shows.
    #[tokio::test]
    async fn given_an_unread_stop_when_unrelated_changes_publish_then_off_stays() {
        let (mut fixture, inner, _) = call_with_source(true);
        fixture.call.view.apply(&FromWorker::Connected);
        let shown = |inner: &Inner| match &*inner.state.borrow() {
            CallState::Active { screen, muted, .. } => (*screen, *muted),
            other => panic!("{other:?}"),
        };
        fixture.call.screen_command(&inner, true, 0).unwrap();
        let first = Arc::new(AtomicBool::new(false));
        fixture
            .call
            .screen_picked(&inner, Ok(grant(&first)))
            .unwrap();
        let share = fixture.call.screen_share;
        let on = FromWorker::Screen {
            state: ScreenState::On,
            share,
        };
        fixture.call.worker_event(&inner, &on).unwrap();
        assert_eq!(shown(&inner), (CallScreen::On, false));

        accept_stop(&fixture.call.screen_control, &inner);
        assert_eq!(shown(&inner), (CallScreen::Off, false));
        fixture
            .call
            .worker_event(&inner, &FromWorker::Muted { muted: true })
            .unwrap();
        assert_eq!(
            shown(&inner),
            (CallScreen::Off, true),
            "mute shows, Off stays"
        );
        fixture.call.worker_event(&inner, &on).unwrap();
        assert_eq!(shown(&inner), (CallScreen::Off, true));

        fixture.call.screen_command(&inner, false, 1).unwrap();
        assert_eq!(shown(&inner), (CallScreen::Off, true));
        fixture.call.screen_command(&inner, true, 1).unwrap();
        let second = Arc::new(AtomicBool::new(false));
        fixture
            .call
            .screen_picked(&inner, Ok(grant(&second)))
            .unwrap();
        let later = fixture.call.screen_share;
        fixture
            .call
            .worker_event(
                &inner,
                &FromWorker::Screen {
                    state: ScreenState::On,
                    share: later,
                },
            )
            .unwrap();
        assert_eq!(shown(&inner), (CallScreen::On, true));
    }

    /// GIVEN a stop accepted and then a new start accepted (the page shows
    /// `Starting`) WHEN the task publishes before it reads either THEN the
    /// page keeps `Starting`: neither the stale `On` nor an `Off` shows.
    #[tokio::test]
    async fn given_an_unread_stop_and_start_when_the_task_publishes_then_starting_stays() {
        let (mut fixture, inner, _) = call_with_source(true);
        fixture.call.view.apply(&FromWorker::Connected);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        let first = Arc::new(AtomicBool::new(false));
        fixture
            .call
            .screen_picked(&inner, Ok(grant(&first)))
            .unwrap();
        let share = fixture.call.screen_share;
        fixture
            .call
            .worker_event(
                &inner,
                &FromWorker::Screen {
                    state: ScreenState::On,
                    share,
                },
            )
            .unwrap();
        accept_stop(&fixture.call.screen_control, &inner);
        inner.state.send_modify(|state| {
            if let CallState::Active { screen, .. } = state {
                *screen = CallScreen::Starting;
            }
        });
        fixture
            .call
            .worker_event(&inner, &FromWorker::Muted { muted: true })
            .unwrap();
        assert!(matches!(
            &*inner.state.borrow(),
            CallState::Active {
                screen: CallScreen::Starting,
                muted: true,
                ..
            }
        ));
    }

    /// What `set_screen` does under the control lock: the page shows the
    /// command (a start keeps `Starting` or `On`) and the command is counted.
    /// Returns the stop count and the command's number.
    fn accept_command(control: &Mutex<ScreenControl>, inner: &Inner, on: bool) -> (u64, u64) {
        let mut control = plain(control);
        inner.state.send_modify(|state| {
            if let CallState::Active { screen, .. } = state {
                *screen = match (on, *screen) {
                    (true, CallScreen::On | CallScreen::Starting) => *screen,
                    (true, _) => CallScreen::Starting,
                    (false, _) => CallScreen::Off,
                };
            }
        });
        if !on {
            control.stops += 1;
        }
        control.requests += 1;
        (control.stops, control.requests)
    }

    /// GIVEN a share starting (the page shows `Starting`) WHEN the handle
    /// accepts a second start and the worker reports `On` before the task
    /// reads it THEN the page keeps `Starting`; WHEN the task reads the
    /// start, which does not change its view (already `On`) THEN the page
    /// shows `On`.
    #[tokio::test]
    async fn given_a_second_start_read_after_on_then_the_page_shows_on() {
        let (mut fixture, inner, _) = call_with_source(true);
        fixture.call.view.apply(&FromWorker::Connected);
        let shown = |inner: &Inner| match &*inner.state.borrow() {
            CallState::Active { screen, .. } => *screen,
            other => panic!("{other:?}"),
        };
        let (stops, request) = accept_command(&fixture.call.screen_control, &inner, true);
        fixture
            .call
            .screen_request(&inner, true, stops, request)
            .unwrap();
        let grant_dropped = Arc::new(AtomicBool::new(false));
        fixture
            .call
            .screen_picked(&inner, Ok(grant(&grant_dropped)))
            .unwrap();
        let share = fixture.call.screen_share;
        assert_eq!(shown(&inner), CallScreen::Starting);

        let (stops, second) = accept_command(&fixture.call.screen_control, &inner, true);
        fixture
            .call
            .worker_event(
                &inner,
                &FromWorker::Screen {
                    state: ScreenState::On,
                    share,
                },
            )
            .unwrap();
        assert_eq!(shown(&inner), CallScreen::Starting, "the start is unread");
        fixture
            .call
            .screen_request(&inner, true, stops, second)
            .unwrap();
        assert_eq!(shown(&inner), CallScreen::On);
    }

    /// GIVEN a running share WHEN the handle accepts a stop and then a start
    /// (the page shows `Starting`) and the task reads only the stop THEN the
    /// stop's publication keeps `Starting`; WHEN the task reads the start
    /// THEN the page follows the task again.
    #[tokio::test]
    async fn given_a_stop_then_a_start_when_only_the_stop_is_read_then_starting_stays() {
        let (mut fixture, inner, _) = call_with_source(true);
        fixture.call.view.apply(&FromWorker::Connected);
        let shown = |inner: &Inner| match &*inner.state.borrow() {
            CallState::Active { screen, .. } => *screen,
            other => panic!("{other:?}"),
        };
        let accept = accept_command;
        let (stops, request) = accept(&fixture.call.screen_control, &inner, true);
        fixture
            .call
            .screen_request(&inner, true, stops, request)
            .unwrap();
        let first = Arc::new(AtomicBool::new(false));
        fixture
            .call
            .screen_picked(&inner, Ok(grant(&first)))
            .unwrap();
        let share = fixture.call.screen_share;
        fixture
            .call
            .worker_event(
                &inner,
                &FromWorker::Screen {
                    state: ScreenState::On,
                    share,
                },
            )
            .unwrap();
        assert_eq!(shown(&inner), CallScreen::On);

        let (stops, stop) = accept(&fixture.call.screen_control, &inner, false);
        let (_, start) = accept(&fixture.call.screen_control, &inner, true);
        assert_eq!(shown(&inner), CallScreen::Starting);
        fixture
            .call
            .screen_request(&inner, false, stops, stop)
            .unwrap();
        assert_eq!(shown(&inner), CallScreen::Starting, "the start stays shown");
        fixture
            .call
            .worker_event(&inner, &FromWorker::Muted { muted: true })
            .unwrap();
        assert_eq!(shown(&inner), CallScreen::Starting);

        fixture
            .call
            .screen_request(&inner, true, stops, start)
            .unwrap();
        assert_eq!(shown(&inner), CallScreen::Starting);
        fixture
            .call
            .screen_picked(&inner, Err(ScreenRefusal::Refused))
            .unwrap();
        assert_ne!(
            shown(&inner),
            CallScreen::Starting,
            "the task's answer shows"
        );
    }

    /// GIVEN a call whose task holds the screen control lock (it publishes)
    /// WHEN the page stops the share THEN `Off` does not show until the lock
    /// is released, and then shows with the stop counted: page change and
    /// count are one step for the task's publication.
    #[tokio::test]
    async fn given_a_task_publication_when_the_page_stops_then_off_waits_for_the_lock() {
        let calls = Arc::new(controller(FakeHost::new(true, None)));
        let (_store, _commands, _) = running_call(&calls, active_state());
        let control = {
            let Slot::Running(run) = &*calls.inner.slot() else {
                panic!("running");
            };
            run.screen_control.clone()
        };
        assert_eq!(calls.set_screen(true), Ok(()));
        let shown = |calls: &CallController| match &*calls.inner.state.borrow() {
            CallState::Active { screen, .. } => *screen,
            other => panic!("{other:?}"),
        };
        assert_eq!(shown(&calls), CallScreen::Starting);

        let held = plain(&control);
        let stopper = {
            let calls = calls.clone();
            std::thread::spawn(move || calls.set_screen(false))
        };
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            shown(&calls),
            CallScreen::Starting,
            "Off waits for the lock"
        );
        assert_eq!(held.stops, 0);
        drop(held);
        assert_eq!(stopper.join().unwrap(), Ok(()));
        assert_eq!(shown(&calls), CallScreen::Off);
        assert_eq!(plain(&control).stops, 1);
    }

    /// GIVEN a share whose portal session is open WHEN the page stops it
    /// THEN the handle closes the session at once, before the call task reads
    /// the stop (it may be in a key send), and counts the stop.
    #[tokio::test]
    async fn given_a_share_when_the_page_stops_it_then_the_session_closes_before_the_task_runs() {
        let calls = controller(FakeHost::new(true, None));
        let (_store, mut commands, _) = running_call(&calls, active_state());
        let control = {
            let Slot::Running(run) = &*calls.inner.slot() else {
                panic!("running");
            };
            run.screen_control.clone()
        };
        let dropped = Arc::new(AtomicBool::new(false));
        plain(&control).session = Some(Box::new(SessionGuard(dropped.clone())));
        assert_eq!(calls.set_screen(true), Ok(()));
        assert!(!dropped.load(Ordering::SeqCst), "a start keeps it");
        assert_eq!(calls.set_screen(false), Ok(()));
        assert!(dropped.load(Ordering::SeqCst), "capture is revoked at once");
        assert!(plain(&control).session.is_none());
        assert_eq!(plain(&control).stops, 1);
        let _ = commands.try_recv();
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::Screen {
                on: false,
                stops: 1,
                ..
            })
        ));
    }

    /// GIVEN a running share and a full worker queue WHEN the page stops the
    /// share THEN the call runs on, the session closes and the stop waits;
    /// a new pick then fails rather than overtake it; WHEN the queue has
    /// room THEN the waiting stop goes, naming its share.
    #[tokio::test]
    async fn given_a_full_queue_when_a_share_stops_then_the_stop_waits_and_the_call_runs_on() {
        let (mut fixture, inner, _) = call_with_source(true);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        let first = Arc::new(AtomicBool::new(false));
        fixture
            .call
            .screen_picked(&inner, Ok(grant(&first)))
            .unwrap();
        let share = fixture.call.screen_share;
        while fixture
            .call
            .link
            .send(ToWorker::SetMuted { muted: true })
            .is_ok()
        {}

        plain(&fixture.call.screen_control).stops += 1;
        assert_eq!(fixture.call.screen_command(&inner, false, 1), Ok(()));
        assert!(first.load(Ordering::SeqCst), "the session closed");
        assert_eq!(fixture.call.screen_stop_pending, Some(share));
        assert_eq!(fixture.call.view.screen, CallScreen::Off);

        fixture.call.screen_command(&inner, true, 1).unwrap();
        let second = Arc::new(AtomicBool::new(false));
        fixture
            .call
            .screen_picked(&inner, Ok(grant(&second)))
            .unwrap();
        assert!(second.load(Ordering::SeqCst), "no start before the stop");
        assert_eq!(fixture.call.view.screen, CallScreen::Unavailable);
        assert_eq!(fixture.call.screen_share, share);

        let mut last = None;
        while let Ok(message) = fixture.to_worker.try_recv() {
            last = Some(message);
        }
        assert!(matches!(last, Some(ToWorker::SetMuted { .. })));
        let permit = screen_stop_room(Some(fixture.call.link.command_sender())).await;
        fixture.call.screen_stop_room_ready(permit).unwrap();
        assert_eq!(fixture.call.screen_stop_pending, None);
        assert!(matches!(
            fixture.to_worker.try_recv(),
            Ok(ToWorker::SetScreen { target: None, share: stopped }) if stopped == share
        ));
    }

    /// GIVEN a platform without screen sharing WHEN the page starts a share
    /// twice (the handle shows `Starting` each time) THEN each attempt
    /// publishes `Unavailable` again, so the page never stays on `Starting`.
    #[tokio::test]
    async fn given_no_screen_source_when_started_again_then_unavailable_is_published_again() {
        let (mut fixture, inner, _) = call_with_source(false);
        fixture.call.view.apply(&FromWorker::Connected);
        let shown = |inner: &Inner| match &*inner.state.borrow() {
            CallState::Active { screen, .. } => *screen,
            other => panic!("{other:?}"),
        };
        for _ in 0..2 {
            fixture.call.screen_command(&inner, true, 0).unwrap();
            assert_eq!(shown(&inner), CallScreen::Unavailable);
            // What `set_screen(true)` shows before the task runs.
            inner.state.send_modify(|state| {
                if let CallState::Active { screen, .. } = state {
                    *screen = CallScreen::Starting;
                }
            });
        }
        fixture.call.screen_command(&inner, true, 0).unwrap();
        assert_eq!(shown(&inner), CallScreen::Unavailable);
    }

    /// GIVEN a worker whose command queue is full WHEN a pick ends THEN the
    /// share is unavailable, its session closes and the call runs on.
    #[tokio::test]
    async fn given_a_full_worker_queue_when_a_pick_ends_then_only_the_share_fails() {
        let (mut fixture, inner, _) = call_with_source(true);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        while fixture
            .call
            .link
            .send(ToWorker::SetMuted { muted: true })
            .is_ok()
        {}
        let dropped = Arc::new(AtomicBool::new(false));
        assert_eq!(
            fixture.call.screen_picked(&inner, Ok(grant(&dropped))),
            Ok(())
        );
        assert_eq!(fixture.call.view.screen, CallScreen::Unavailable);
        assert!(dropped.load(Ordering::SeqCst), "the session closed");
        assert!(!fixture.call.screen_sent);
        while let Ok(message) = fixture.to_worker.try_recv() {
            assert!(!matches!(message, ToWorker::SetScreen { .. }));
        }
    }

    /// GIVEN share A stopped and share B started WHEN A's late state comes
    /// THEN B's session stays open and the state is not shown; B's own
    /// states follow, and B's stop names B.
    #[tokio::test]
    async fn given_a_late_state_of_an_older_share_then_the_newer_share_keeps_its_session() {
        let (mut fixture, inner, _) = call_with_source(true);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        let first = Arc::new(AtomicBool::new(false));
        fixture
            .call
            .screen_picked(&inner, Ok(grant(&first)))
            .unwrap();
        let _ = fixture.to_worker.try_recv();
        let older = fixture.call.screen_share;
        // As `set_screen(false)` does: counted, then queued.
        plain(&fixture.call.screen_control).stops += 1;
        fixture.call.screen_command(&inner, false, 1).unwrap();
        let _ = fixture.to_worker.try_recv();
        fixture.call.screen_command(&inner, true, 1).unwrap();
        let second = Arc::new(AtomicBool::new(false));
        fixture
            .call
            .screen_picked(&inner, Ok(grant(&second)))
            .unwrap();
        let newer = fixture.call.screen_share;
        assert_ne!(newer, older);
        assert!(matches!(
            fixture.to_worker.try_recv(),
            Ok(ToWorker::SetScreen {
                target: Some(_),
                share
            }) if share == newer
        ));
        for state in [ScreenState::On, ScreenState::Off] {
            fixture
                .call
                .worker_event(
                    &inner,
                    &FromWorker::Screen {
                        state,
                        share: older,
                    },
                )
                .unwrap();
        }
        assert!(!second.load(Ordering::SeqCst), "B's session stays open");
        assert!(fixture.call.screen_sent);
        assert_eq!(fixture.call.view.screen, CallScreen::Starting);
        fixture
            .call
            .worker_event(
                &inner,
                &FromWorker::Screen {
                    state: ScreenState::On,
                    share: newer,
                },
            )
            .unwrap();
        assert_eq!(fixture.call.view.screen, CallScreen::On);
        plain(&fixture.call.screen_control).stops += 1;
        fixture.call.screen_command(&inner, false, 2).unwrap();
        assert!(matches!(
            fixture.to_worker.try_recv(),
            Ok(ToWorker::SetScreen { target: None, share }) if share == newer
        ));
        assert!(second.load(Ordering::SeqCst));
    }

    /// GIVEN a running share WHEN the desktop closes the portal session THEN
    /// the worker is told to stop and the share is off.
    #[tokio::test]
    async fn given_the_desktop_closes_the_session_then_the_share_ends() {
        let (mut fixture, inner, _) = call_with_source(true);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let (closed_tx, closed_rx) = oneshot::channel();
        let mut with_signal = grant(&dropped);
        with_signal.closed = Some(closed_rx);
        fixture.call.screen_picked(&inner, Ok(with_signal)).unwrap();
        let _ = fixture.to_worker.try_recv();
        closed_tx.send(()).unwrap();
        screen_session_closed(&mut fixture.call.screen_closed).await;
        fixture.call.screen_ended_by_desktop(&inner).unwrap();
        assert!(matches!(
            fixture.to_worker.try_recv(),
            Ok(ToWorker::SetScreen { target: None, .. })
        ));
        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(fixture.call.view.screen, CallScreen::Off);
    }

    /// GIVEN the end of the call WHEN the call is torn down THEN a pick in
    /// flight is cancelled and the session closes with it.
    #[tokio::test]
    async fn given_the_call_ends_then_the_pick_is_cancelled_and_the_session_closes() {
        let (mut fixture, inner, _) = call_with_source(true);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        let cancel = fixture
            .call
            .screen_pick
            .as_ref()
            .expect("a pick")
            .cancel
            .clone();
        let dropped = Arc::new(AtomicBool::new(false));
        plain(&fixture.call.screen_control).session = Some(Box::new(SessionGuard(dropped.clone())));
        fixture.call.finish(true).await;
        assert!(cancel.load(Ordering::SeqCst), "the pick was cancelled");
        assert!(dropped.load(Ordering::SeqCst), "the session closed");
    }

    /// GIVEN screen sharers and screen video WHEN the worker says so THEN a
    /// member shows as sharing, the screen tile (5) carries the sharer, a
    /// stranger is not marked, and a member that leaves loses both.
    #[test]
    fn given_remote_screen_shares_then_members_show_as_sharing_and_tile_five_is_theirs() {
        let mut view = view();
        let share = |identity: &str, sharing| FromWorker::ScreenSharer {
            identity: identity.to_owned(),
            sharing,
        };
        assert_eq!(
            view.apply(&share("@a:example.org:A", true)),
            Applied::Changed
        );
        assert_eq!(view.apply(&share("@a:example.org:A", true)), Applied::Same);
        assert_eq!(view.apply(&share("@x:example.org:X", true)), Applied::Same);
        view.apply(&FromWorker::Connected);
        let CallState::Active { participants, .. } = view.state() else {
            panic!("active");
        };
        assert_eq!(
            participants.iter().map(|p| p.sharing).collect::<Vec<_>>(),
            [true, false]
        );
        view.apply(&started(5, "@a:example.org:A"));
        let tiles = view.video_tiles();
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].tile, 5);
        assert_eq!(view.video_allowed().1, [false, false, false, false, true]);
        assert_eq!(
            view.apply(&share("@a:example.org:A", false)),
            Applied::Changed
        );
        view.apply(&share("@a:example.org:A", true));
        assert_eq!(
            view.apply(&FromWorker::ParticipantLeft {
                identity: "@a:example.org:A".to_owned()
            }),
            Applied::Changed
        );
        assert!(!view.sharing.contains("@a:example.org:A"));
        assert!(view.video_tiles().is_empty());
    }

    /// GIVEN a share hint for an identity whose membership has not arrived
    /// WHEN the membership arrives THEN the member shows as sharing; a
    /// stranger is never shown, and hints beyond the bound are not kept.
    #[test]
    fn given_a_share_hint_before_the_membership_then_it_shows_once_the_member_arrives() {
        let mut view = view();
        view.apply(&FromWorker::Connected);
        let early = FromWorker::ScreenSharer {
            identity: "@c:example.org:C".to_owned(),
            sharing: true,
        };
        assert_eq!(view.apply(&early), Applied::Same, "nothing shown yet");
        let sharing = |view: &View| {
            let CallState::Active { participants, .. } = view.state() else {
                panic!("active");
            };
            participants
                .iter()
                .filter(|p| p.sharing)
                .map(|p| format!("{}:{}", p.user_id, p.device_id))
                .collect::<Vec<_>>()
        };
        assert!(sharing(&view).is_empty());
        view.set_members(vec![
            member("@a:example.org", "A", "https://sfu.example.org"),
            member("@c:example.org", "C", "https://sfu.example.org"),
        ]);
        assert_eq!(sharing(&view), ["@c:example.org:C"]);

        for index in 0..persephone_call_protocol::MAX_PARTICIPANTS + 4 {
            view.apply(&FromWorker::ScreenSharer {
                identity: format!("@s{index}:example.org:S"),
                sharing: true,
            });
        }
        assert_eq!(
            view.sharing.len(),
            persephone_call_protocol::MAX_PARTICIPANTS
        );
        assert_eq!(sharing(&view), ["@c:example.org:C"], "strangers hidden");
    }

    /// GIVEN each worker screen state WHEN applied THEN the page's state
    /// follows, and a screen problem never ends the call.
    #[test]
    fn given_worker_screen_states_then_the_view_follows_and_the_call_continues() {
        let mut view = view();
        view.apply(&FromWorker::Connected);
        for (state, shown) in [
            (ScreenState::Starting, CallScreen::Starting),
            (ScreenState::On, CallScreen::On),
            (ScreenState::NotFound, CallScreen::NotFound),
            (ScreenState::Unavailable, CallScreen::Unavailable),
            (ScreenState::Refused, CallScreen::Refused),
            (ScreenState::Off, CallScreen::Off),
        ] {
            let applied = view.apply(&FromWorker::Screen { state, share: 1 });
            assert!(
                matches!(applied, Applied::Changed | Applied::Same),
                "{state:?}"
            );
            assert_eq!(view.screen, shown);
        }
    }

    /// GIVEN no call, a joining call or a closed queue WHEN the page starts a
    /// share THEN it is refused; GIVEN a connected call THEN the task gets
    /// the command with no source and the state shows `starting` at once.
    #[tokio::test]
    async fn given_a_connected_call_then_the_screen_command_reaches_the_task() {
        let calls = controller(FakeHost::new(true, None));
        assert_eq!(calls.set_screen(true), Err(CallError::NotInCall));
        let calls = controller(FakeHost::new(true, None));
        let joining = CallState::Joining {
            room_id: room_id!("!r:example.org").to_owned(),
        };
        let (_store, _commands, _) = running_call(&calls, joining);
        assert_eq!(calls.set_screen(true), Err(CallError::NotInCall));

        let calls = controller(FakeHost::new(true, None));
        let (_store, mut commands, _) = running_call(&calls, active_state());
        assert_eq!(calls.set_screen(true), Ok(()));
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::Screen {
                on: true,
                stops: 0,
                request: 1
            })
        ));
        let CallState::Active { screen, .. } = calls.state() else {
            panic!("active");
        };
        assert_eq!(screen, CallScreen::Starting);
        assert_eq!(calls.set_screen(false), Ok(()));
        // The stop is counted before the task reads it.
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::Screen {
                on: false,
                stops: 1,
                ..
            })
        ));
        let CallState::Active { screen, .. } = calls.state() else {
            panic!("active");
        };
        assert_eq!(screen, CallScreen::Off);
    }

    /// GIVEN a Linux grant with a remote WHEN it goes to the worker THEN the
    /// remote goes over the camera channel first, with the byte `s`, and the
    /// command follows; a portal grant without a remote is unavailable.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn given_a_portal_grant_then_the_screen_remote_goes_first_with_the_byte_s() {
        use persephone_call_protocol::ScreenTarget;
        use std::io::Read;
        let (mut fixture, inner, _) = call_with_source(true);
        let (core_end, mut worker_end) = std::os::unix::net::UnixStream::pair().unwrap();
        let (command_tx, mut commands) = mpsc::channel(4);
        fixture.call.link = CallWorkerLink::from_parts(
            command_tx,
            mpsc::channel(1).1,
            Box::new(FakeProcess(Arc::new(AtomicBool::new(false)))),
        )
        .with_camera_channel(core_end);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        let remote = std::os::fd::OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
        let dropped = Arc::new(AtomicBool::new(false));
        let portal = ScreenGrant {
            target: ScreenTarget::Portal { node: 42 },
            remote: Some(remote),
            session: Some(Box::new(SessionGuard(dropped.clone()))),
            closed: None,
        };
        fixture.call.screen_picked(&inner, Ok(portal)).unwrap();
        let mut byte = [0_u8; 1];
        assert_eq!(worker_end.read(&mut byte).unwrap(), 1, "the remote went");
        assert_eq!(byte, [b's']);
        assert!(matches!(
            commands.try_recv(),
            Ok(ToWorker::SetScreen {
                target: Some(ScreenTarget::Portal { node: 42 }),
                share: 1
            })
        ));
        assert!(!dropped.load(Ordering::SeqCst));

        fixture.call.screen_command(&inner, false, 0).unwrap();
        fixture.call.screen_command(&inner, true, 0).unwrap();
        let no_remote = ScreenGrant {
            target: ScreenTarget::Portal { node: 43 },
            remote: None,
            session: None,
            closed: None,
        };
        fixture.call.screen_picked(&inner, Ok(no_remote)).unwrap();
        assert_eq!(fixture.call.view.screen, CallScreen::Unavailable);
    }
}
