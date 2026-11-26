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

use super::alone::Alone;
use super::delayed::{DelayedLeave, RestartHealth};
use super::encryption_keys::{self, IncomingKey};
use super::incoming::{self, Candidate};
use super::keys::{KeyError, Outgoing, OwnKeys, Recipients, RemoteKeys};
use super::membership::{
    ActiveMember, DELAYED_LEAVE_RESTART, EVENT_TYPE, MEMBERSHIP_REFRESH, hashed_identity,
    leave_content, own_membership, own_state_key, participant_identity, read_members,
};
use super::other_sfu::{Home, MAX_OTHER_SFUS, OtherSfus, Plan, home, is_other_host};
use super::readiness::{Discovery, Flow, TurnServers, sfu_transport, turn_servers};
use super::rings::{self, Plan as RingPlan, Watch};
use super::screen::{ScreenGrant, ScreenRefusal, ScreenSource};
use super::screen_helper::{HelperShare, HelperStop};
use super::sfu::{self, OpenIdToken, SfuError, SfuGrant};
use super::status::can_send_member_state;
use super::video::{Notifier, Picture, VideoStore};
use super::worker::{
    CallHost, CallWorkerLink, EVENT_QUEUE, StartError, WorkerAudio, WorkerExit, WorkerProcess,
    exit_is_crash,
};
use super::{MAX_CALL_MEMBERS, device_test, keys::FrameKey};
use crate::session::Admission;
use matrix_sdk::{
    Client, RoomState,
    event_handler::EventHandlerHandle,
    ruma::{
        OwnedDeviceId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId,
        api::client::account::request_openid_token,
        api::error::ErrorKind,
        events::{AnySyncMessageLikeEvent, AnySyncStateEvent},
        serde::Raw,
    },
};
use persephone_call_protocol::{
    CameraState, FailureCode, FrameKeyMsg, FromWorker, MAX_VOLUME_PERCENT, ScreenState, ToWorker,
    TurnServer, VIDEO_TILES, valid_mic_gain,
};
use persephone_contracts::{
    CALL_PARTICIPANTS_MAX, CallCamera, CallDeviceChange, CallDeviceTestKind, CallDeviceTestState,
    CallDeviceTestStatus, CallParticipantLevel, CallParticipantRef, CallScreen, CallVideoTile,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    future::Future,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    runtime::Handle,
    sync::{Notify, mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
    time,
};
use url::Url;
use zeroize::Zeroizing;

/// How long the worker has to leave on its own before it is ended.
const WORKER_EXIT: Duration = Duration::from_secs(5);
/// The command slots a screen start takes at once: on Windows the frame
/// pipe's `ScreenFrames` and the `SetScreen`, elsewhere the `SetScreen` (the
/// Linux remote goes over the camera channel).
#[cfg(windows)]
const SCREEN_START_COMMANDS: usize = 2;
#[cfg(not(windows))]
const SCREEN_START_COMMANDS: usize = 1;
/// How long the end of a call waits for a cancelled screen pick (ADR-0067).
/// The question box closes within about 100 ms of the cancel and the idle
/// wait looks at the cancel every 50 ms, so this only bounds a fault.
const PICK_END_WAIT: Duration = Duration::from_secs(2);
/// How long the end of a call waits for the helpers that earlier stops left
/// to their reaper threads: a stop wait and some margin.
const REAPED_HELPER_WAIT: Duration = Duration::from_secs(5);

/// Longest wait for one delayed-event restart. It is shorter than the
/// restart interval.
const RESTART_TIMEOUT: Duration = Duration::from_secs(4);

/// Longest wait for one key send or membership write.
const SEND_TIMEOUT: Duration = Duration::from_secs(20);

/// Most keys held back for a sender whose membership event is not here yet.
const MAX_PARKED: usize = 8;

/// How long such a key is held back.
const PARK_TIME: Duration = Duration::from_secs(30);

/// Wait before a key that did not fit in the worker queue goes again.
const UNSENT_RETRY: Duration = Duration::from_millis(200);

/// Most remote keys of one member waiting for room in the worker queue: every
/// stored key under each of at most two backend identities.
const MAX_UNSENT_PER_MEMBER: usize = 2 * crate::calls::keys::MAX_KEYS_PER_MEMBER;

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
    /// The homeserver offers no SFU, or could not be asked.
    NoSfu,
    /// The homeserver offers SFUs, but none that Persephone can use
    /// (ADR-0056).
    SfuUnsupported,
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
    /// The room's power levels, or the homeserver's `M_FORBIDDEN`, do not let
    /// this account send the call membership state event.
    NotPermitted,
    OpenId,
    Sfu(SfuError),
    /// No random key, or the worker queue refused it.
    Keys,
    NotInCall,
    /// The running call's command queue is full: the command was not taken
    /// and will not happen. The caller may try again.
    Busy,
    /// `leave` ended this join before the call started.
    Cancelled,
    /// The session stopped admitting work during the join.
    SessionEnded,
    /// A call setting outside its range (ADR-0062). Nothing changed.
    InvalidValue,
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
    /// No other participant was on any SFU connection for
    /// [`ALONE_LIMIT`](super::alone::ALONE_LIMIT).
    Alone,
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
    /// The worker is connected to the SFU it publishes to: the own SFU, or
    /// another SFU the user agreed to connect to (ADR-0054). Otherwise it
    /// cannot be heard or seen.
    pub reachable: bool,
    /// This device holds a key for it. Without one it cannot be heard.
    pub keyed: bool,
    /// It shares its screen (ADR-0053); a display hint from the sender.
    pub sharing: bool,
    /// Its microphone is muted or it has none (ADR-0059); a hint from the
    /// sender.
    pub muted: bool,
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
        /// The microphone is muted, as the worker last said. Deafened implies
        /// muted.
        muted: bool,
        /// Remote audio is off, as the worker last said (ADR-0055).
        deafened: bool,
        /// The playout volume of remote audio, 0 to 100 percent.
        volume: u8,
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
        /// The last device change of this call failed (ADR-0059).
        device_change: Option<CallDeviceChange>,
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

/// One membership: the device (`{user}:{device}`) and its `membershipID`.
/// Two devices may share a membership ID (ADR-0056), so the ID alone does not
/// name a member; a device that joins again with a new ID is a new
/// membership.
type MemberKey = (String, String);

fn member_key(member: &ActiveMember) -> MemberKey {
    (
        participant_identity(&member.user_id, &member.device_id),
        member.identity.clone(),
    )
}

/// Who joined and left since the last look, by membership.
#[derive(Debug, Default, PartialEq, Eq)]
struct MemberDiff {
    joined: Vec<MemberKey>,
    left: Vec<MemberKey>,
}

/// The change from `known` to `current`.
fn diff_members(known: &BTreeSet<MemberKey>, current: &[ActiveMember]) -> MemberDiff {
    let now: BTreeSet<MemberKey> = current.iter().map(member_key).collect();
    MemberDiff {
        joined: current
            .iter()
            .map(member_key)
            .filter(|key| !known.contains(key))
            .collect(),
        left: known.difference(&now).cloned().collect(),
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
    joined: &[MemberKey],
) -> Vec<(OwnedUserId, OwnedDeviceId)> {
    members
        .iter()
        .filter(|m| match outgoing.recipients {
            Recipients::AllMembers => true,
            Recipients::NewMembersOnly => joined.contains(&member_key(m)),
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

/// What the user asked of the microphone and the speakers (ADR-0055).
/// Deafened turns off all remote audio and mutes the microphone; the mute
/// the user chose is kept apart, so undeafening restores it. Unmuting while
/// deafened also undeafens, so the user never talks without hearing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AudioWants {
    muted: bool,
    deafened: bool,
}

impl AudioWants {
    fn microphone_muted(self) -> bool {
        self.muted || self.deafened
    }

    /// The user mutes or unmutes. Returns the worker commands, in order.
    fn set_muted(&mut self, muted: bool) -> Vec<ToWorker> {
        self.muted = muted;
        let mut out = Vec::with_capacity(2);
        if !muted && self.deafened {
            self.deafened = false;
            out.push(ToWorker::SetDeafened { deafened: false });
        }
        out.push(ToWorker::SetMuted {
            muted: self.microphone_muted(),
        });
        out
    }

    /// The user deafens or undeafens. Returns the worker commands, in order:
    /// the microphone goes off before remote audio, and remote audio comes
    /// back before the microphone.
    fn set_deafened(&mut self, deafened: bool) -> Vec<ToWorker> {
        self.deafened = deafened;
        let microphone = ToWorker::SetMuted {
            muted: self.microphone_muted(),
        };
        let speakers = ToWorker::SetDeafened { deafened };
        if deafened {
            vec![microphone, speakers]
        } else {
            vec![speakers, microphone]
        }
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
    deafened: bool,
    /// 0 to [`MAX_VOLUME_PERCENT`].
    volume: u8,
    /// Other devices only.
    members: Vec<ActiveMember>,
    speaking: HashSet<String>,
    /// The connection and identity of each remote microphone that is muted
    /// or absent (ADR-0059), as the worker said with `ParticipantMuted`. A
    /// display hint only, kept like `sharing`: an identity that is not a
    /// member yet waits here and shows once the membership is read, and only
    /// while the connection is the one that speaks for the member. At most
    /// `MAX_PARTICIPANTS` are kept.
    mic_muted: HashSet<(u8, String)>,
    keyed: HashSet<String>,
    camera: CallCamera,
    /// The own screen share.
    screen: CallScreen,
    /// The connection and identity of each screen sharer, as the worker said
    /// with `ScreenSharer`. A display hint only. An identity that is not a
    /// member yet waits here and shows once the membership is read, and only
    /// while the connection is the one that speaks for the member
    /// (ADR-0054). At most `MAX_PARTICIPANTS` are kept.
    sharing: HashSet<(u8, String)>,
    /// The connection and identity on each remote tile (tile 1 first), as the
    /// worker said with `VideoStarted`. An identity that is not a member yet
    /// waits here and shows once the membership is read, and only while the
    /// connection is the one that speaks for the member (ADR-0054).
    remote_video: [Option<(u8, String)>; REMOTE_TILES],
    /// The connections to other SFUs (ADR-0054).
    other: OtherSfus,
    /// The stream number of each tile (tile 0 first), as the picture store
    /// gave it when the controller applied the tile's latest change. The
    /// page asks for pictures of this stream only.
    video_streams: [u64; VIDEO_TILES as usize],
    /// The ring this call sent was declined (ADR-0052).
    ring_declined: bool,
    /// The last device change failed (ADR-0059).
    device_change: Option<CallDeviceChange>,
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
            deafened: false,
            volume: MAX_VOLUME_PERCENT,
            members: Vec::new(),
            speaking: HashSet::new(),
            mic_muted: HashSet::new(),
            keyed: HashSet::new(),
            camera: if video {
                CallCamera::Starting
            } else {
                CallCamera::Off
            },
            screen: CallScreen::Off,
            sharing: HashSet::new(),
            remote_video: Default::default(),
            other: OtherSfus::default(),
            video_streams: Default::default(),
            ring_declined: false,
            device_change: None,
        }
    }

    /// The member whose device `identity` names, as frame keys name a
    /// member: `{user}:{device}` of the authenticated sender (ADR-0056).
    fn key_member(&self, identity: &str) -> Option<&ActiveMember> {
        self.members
            .iter()
            .find(|m| participant_identity(&m.user_id, &m.device_id) == identity)
    }

    /// Whether `identity` names a current member's device (see
    /// [`View::key_member`]).
    fn is_member(&self, identity: &str) -> bool {
        self.key_member(identity).is_some()
    }

    /// The member that a worker participant identity stands for: one of its
    /// backend identities (ADR-0056).
    fn backend_member(&self, participant: &str) -> Option<&ActiveMember> {
        self.members
            .iter()
            .find(|m| m.backend_identities.iter().any(|id| id == participant))
    }

    /// Whether `member` shares a screen: a connection that speaks for the
    /// member said so for one of its backend identities.
    fn is_sharing(&self, member: &ActiveMember) -> bool {
        self.sharing.iter().any(|(connection, id)| {
            member.backend_identities.contains(id) && self.accepts(*connection, id)
        })
    }

    /// Whether the participant `identity` of this worker connection may
    /// stand for a member (ADR-0054). Connection 0 speaks for the members on
    /// the own SFU. Connection `n` (1 to 3) speaks only for members whose
    /// membership names the SFU of that connection. Anything else is
    /// ignored: a member on another SFU also shows as a silent participant
    /// on the own SFU, and an SFU can name any identity.
    fn accepts(&self, connection: u8, identity: &str) -> bool {
        let Some(member) = self.backend_member(identity) else {
            return false;
        };
        match home(member, &self.own_sfu) {
            Home::Own => connection == 0,
            Home::Other(url) => connection != 0 && self.other.index_of(url) == Some(connection),
            Home::Nowhere => false,
        }
    }

    fn set_members(&mut self, members: Vec<ActiveMember>) {
        self.members = members;
        // `speaking` holds worker participant identities, `keyed` holds
        // devices as keys name them.
        let present: HashSet<String> = self
            .members
            .iter()
            .map(|m| participant_identity(&m.user_id, &m.device_id))
            .collect();
        let backends: HashSet<&str> = self
            .members
            .iter()
            .flat_map(|m| m.backend_identities.iter().map(String::as_str))
            .collect();
        self.speaking.retain(|id| backends.contains(id.as_str()));
        self.keyed.retain(|id| present.contains(id.as_str()));
        // `sharing` and `mic_muted` keep hints of identities that are not
        // members yet: the worker can hear of a share or a muted microphone
        // before the membership arrives. `ParticipantLeft` removes them.
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

    /// Forgets the video and the sharing mark that a connection gave a
    /// member that left it. The view changes only when that connection
    /// speaks for the member.
    fn forget_video(&mut self, connection: u8, identity: &str) -> bool {
        let removed = self.sharing.remove(&(connection, identity.to_owned()));
        let mut changed = removed && self.accepts(connection, identity);
        for slot in &mut self.remote_video {
            if slot
                .as_ref()
                .is_some_and(|(known, id)| *known == connection && id == identity)
            {
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
                .as_ref()
                .is_some_and(|(connection, identity)| self.accepts(*connection, identity))
        });
        (self.camera == CallCamera::On, remote)
    }

    /// Starts the view with the saved playout volume.
    fn with_volume(mut self, volume: u8) -> Self {
        self.volume = volume.min(MAX_VOLUME_PERCENT);
        self
    }

    /// The volume changed; the worker does not answer a volume change.
    fn set_volume(&mut self, volume: u8) -> Applied {
        let volume = volume.min(MAX_VOLUME_PERCENT);
        if self.volume == volume {
            return Applied::Same;
        }
        self.volume = volume;
        Applied::Changed
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
        for (index, slot) in self.remote_video.iter().enumerate() {
            let Some(member) = slot
                .as_ref()
                .filter(|(connection, identity)| self.accepts(*connection, identity))
                .and_then(|(_, identity)| self.backend_member(identity))
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

    /// The member whose sound level the worker reports, as the page names
    /// it: only a current member, and only from the connection that speaks
    /// for it.
    fn level_member(&self, connection: u8, identity: &str) -> Option<CallParticipantLevelKey> {
        if !self.accepts(connection, identity) {
            return None;
        }
        self.backend_member(identity)
            .map(|member| (member.user_id.to_string(), member.device_id.to_string()))
    }

    fn apply(&mut self, event: &FromWorker) -> Applied {
        match event {
            // A record goes to the diagnostic log, never to the view.
            // A sound level goes to the level meters, never to the view.
            FromWorker::Ready { .. }
            | FromWorker::ParticipantJoined { .. }
            | FromWorker::Diagnostic { .. }
            | FromWorker::SoundLevel { .. }
            | FromWorker::ParticipantLevel { .. } => Applied::Same,
            FromWorker::ParticipantMuted {
                connection,
                identity,
                muted,
            } => {
                let key = (*connection, identity.clone());
                let changed = if *muted {
                    self.mic_muted.len() < persephone_call_protocol::MAX_PARTICIPANTS
                        && self.mic_muted.insert(key)
                } else {
                    self.mic_muted.remove(&key)
                };
                // A hint for someone who is not a member changes no row.
                if changed && self.accepts(*connection, identity) {
                    Applied::Changed
                } else {
                    Applied::Same
                }
            }
            FromWorker::DevicesSet { failure } => {
                let change = failure.map(|code| match code {
                    FailureCode::DeviceNotFound => CallDeviceChange::NotFound,
                    FailureCode::Unsupported => CallDeviceChange::Unsupported,
                    FailureCode::DeviceKept => CallDeviceChange::Kept,
                    _ => CallDeviceChange::Failed,
                });
                if self.device_change == change {
                    return Applied::Same;
                }
                self.device_change = change;
                Applied::Changed
            }
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
            FromWorker::ScreenSharer {
                connection,
                identity,
                sharing,
            } => {
                // A member is taken only from the connection that speaks for
                // it. An identity that is not a member yet is kept with its
                // connection, up to the bound; it shows once its membership
                // names that connection's SFU.
                let member = self.backend_member(identity).is_some();
                if member && !self.accepts(*connection, identity) {
                    return Applied::Same;
                }
                let key = (*connection, identity.clone());
                let changed = if !*sharing {
                    self.sharing.remove(&key)
                } else if self.sharing.len() < persephone_call_protocol::MAX_PARTICIPANTS {
                    self.sharing.insert(key)
                } else {
                    false
                };
                if changed && member {
                    Applied::Changed
                } else {
                    Applied::Same
                }
            }
            FromWorker::VideoStarted {
                tile,
                connection,
                identity,
            } => {
                let Some(slot) = usize::from(*tile)
                    .checked_sub(1)
                    .and_then(|index| self.remote_video.get_mut(index))
                else {
                    return Applied::Same;
                };
                if slot
                    .as_ref()
                    .is_some_and(|(known, id)| known == connection && id == identity)
                {
                    return Applied::Same;
                }
                *slot = Some((*connection, identity.clone()));
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
            FromWorker::Connection { connection, up } => {
                if self.other.worker_connection(*connection, *up) {
                    Applied::Changed
                } else {
                    Applied::Same
                }
            }
            FromWorker::ParticipantLeft {
                connection,
                identity,
            } => {
                let video = self.forget_video(*connection, identity);
                let accepted = self.accepts(*connection, identity);
                let speaking = accepted && self.speaking.remove(identity);
                let muted = self.mic_muted.remove(&(*connection, identity.clone())) && accepted;
                if speaking || video || muted {
                    Applied::Changed
                } else {
                    Applied::Same
                }
            }
            FromWorker::Speaking {
                connection,
                identity,
                speaking,
            } => {
                // Only a current member can be shown as speaking, and only
                // by the connection that speaks for it.
                if !self.accepts(*connection, identity) {
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
            FromWorker::Deafened { deafened } => {
                let changed = self.deafened != *deafened;
                self.deafened = *deafened;
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
            deafened: self.deafened,
            volume: self.volume,
            participants: self
                .members
                .iter()
                .take(MAX_CALL_MEMBERS)
                .map(|m| Participant {
                    user_id: m.user_id.clone(),
                    device_id: m.device_id.clone(),
                    speaking: m
                        .backend_identities
                        .iter()
                        .any(|id| self.speaking.contains(id)),
                    reachable: match home(m, &self.own_sfu) {
                        Home::Own => true,
                        Home::Other(url) => self.other.is_connected(url),
                        Home::Nowhere => false,
                    },
                    keyed: self
                        .keyed
                        .contains(&participant_identity(&m.user_id, &m.device_id)),
                    sharing: self.is_sharing(m),
                    muted: self.mic_muted.iter().any(|(connection, id)| {
                        m.backend_identities.contains(id) && self.accepts(*connection, id)
                    }),
                })
                .collect(),
            camera: self.camera,
            screen: self.screen,
            video: self.video_tiles(),
            ring_declined: self.ring_declined,
            device_change: self.device_change,
        }
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Writes one call worker record to the diagnostic log (ADR-0058). The
/// text is the worker's, untrusted: the log redacts it like any other.
fn worker_record(level: u8, target: &str, message: &str) {
    use crate::diagnostics::{Level, Source, global};
    let level = match level {
        1 => Level::Error,
        2 => Level::Warn,
        3 => Level::Info,
        _ => Level::Debug,
    };
    global().record(Source::CallWorker, level, target, message, &[]);
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
async fn start_worker(
    host: &Arc<dyn CallHost>,
    audio: WorkerAudio,
) -> Result<CallWorkerLink, CallError> {
    let host = host.clone();
    match tokio::task::spawn_blocking(move || host.start(&audio)).await {
        Ok(Ok(link)) => Ok(link),
        Ok(Err(error)) => Err(CallError::Worker(error)),
        Err(_) => Err(CallError::Worker(StartError::Launch)),
    }
}

/// Ends the worker; blocks in a blocking task.
async fn finish_worker(link: CallWorkerLink) {
    let _ = tokio::task::spawn_blocking(move || link.finish(WORKER_EXIT)).await;
}

/// Ends the screen pick and the capture helpers of a call (ADR-0067). A pick
/// that still runs is cancelled. On Windows its task is waited for, at most
/// [`PICK_END_WAIT`], because it may start a helper: that helper is stopped
/// and joined with the running share's one. Then the wait goes on until no
/// helper is active, at most [`REAPED_HELPER_WAIT`], so a helper that an
/// earlier stop left to its reaper has ended too. A launch under way counts
/// as active (`LaunchGuard`): a pick that outlived [`PICK_END_WAIT`] after
/// its last cancel check is waited for here, and it stops its helper
/// itself when it sees the cancel. Both waits are bounded, so a stuck launch
/// cannot hold the end of the call or of the session. A launch that is
/// still under way after them gets no worker: its pick reads the cancel
/// after the launch and drops the helper (its reaper stops it), the frame
/// pipe's read end goes with the grant, and the next share starts only
/// when no helper is active.
async fn end_screen_helpers(pick: Option<ScreenPick>, helper: Option<HelperShare>) {
    let mut helpers: Vec<HelperShare> = helper.into_iter().collect();
    if let Some(mut pick) = pick {
        pick.cancel.store(true, Ordering::SeqCst);
        // Only the Windows pick starts a helper; elsewhere the cancel is
        // enough. A grant that the pick made anyway drops here, except its
        // helper.
        if cfg!(windows)
            && let Ok(Ok(Ok(grant))) = time::timeout(PICK_END_WAIT, &mut pick.task).await
        {
            helpers.extend(grant.helper);
        }
    }
    if helpers.is_empty() && super::screen_helper::helpers_active() == 0 {
        return;
    }
    let _ = tokio::task::spawn_blocking(move || {
        for helper in helpers {
            helper.stop_and_join();
        }
        super::screen_helper::wait_until_idle(REAPED_HELPER_WAIT);
    })
    .await;
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
/// makes one, so a join cannot skip the question. It also keeps whether the
/// profile has a proxy, for the question about another SFU (ADR-0054).
#[derive(Debug)]
pub struct DirectConsent(bool);

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
    /// A microphone and speaker choice made while the call joins
    /// (ADR-0059). The call task gets the same channel and sends the
    /// newest choice once the worker is connected.
    devices: watch::Sender<Option<DeviceChoice>>,
    /// The microphone level and gain set while the call joins (ADR-0062).
    /// `None` until one is set: the join sends the saved settings. The call
    /// task gets the same channel, like `devices`.
    microphone: watch::Sender<Option<MicSettings>>,
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
    /// The screen stops accepted and the share's session (its portal
    /// session, or its [`WorkerStop`]), shared with the call task.
    screen_control: Arc<Mutex<ScreenControl>>,
    /// The latest playout volume. A watch keeps only the newest value, so a
    /// fast slider never fills the command queue.
    volume: watch::Sender<u8>,
    /// The latest microphone settings (ADR-0062) set since the join
    /// started, kept like the volume. `None` until one is set.
    microphone: watch::Sender<Option<MicSettings>>,
    /// The latest microphone and speaker choice made during the call
    /// (ADR-0059). `None` until one is made.
    devices: watch::Sender<Option<DeviceChoice>>,
    task: Option<JoinHandle<()>>,
}

/// The automatic microphone level and the manual microphone gain of a
/// joining or running call (ADR-0062). The gain is a whole percent, 0 to 400 in steps of
/// 5; `set_microphone` refuses any other value, so a value in a channel is
/// always valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MicSettings {
    auto_level: bool,
    gain: u16,
}

/// A microphone, speaker and camera choice; `None` is the system default.
/// The camera applies from the call's next camera start (ADR-0059).
#[derive(Clone, Debug, PartialEq, Eq)]
struct DeviceChoice {
    recording: Option<String>,
    playout: Option<String>,
    camera: Option<String>,
}

/// What the page asks of a running call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Mute(bool),
    /// Deafen or undeafen (ADR-0055).
    Deafen(bool),
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
/// Takes the call's sound levels (ADR-0055, ADR-0059).
pub type SoundListener = Arc<dyn Fn(SoundUpdate) + Send + Sync>;

/// A member as the page names it: user ID and device ID.
type CallParticipantLevelKey = (String, String);

/// A change of the level meters. Not call state: the page gets the newest
/// one only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SoundUpdate {
    /// The own microphone, 0 to `MAX_SOUND_LEVEL`, as it is sent. In a
    /// device test (ADR-0059) the tested microphone.
    Own(u8),
    /// Every other device that is not silent now, at most
    /// [`MAX_CALL_MEMBERS`]. Empty when the call ends.
    Participants(Vec<CallParticipantLevel>),
}

struct Inner {
    host: Arc<dyn CallHost>,
    state: watch::Sender<CallState>,
    slot: Mutex<Slot>,
    admission: Mutex<Admission>,
    listener: Mutex<Option<Listener>>,
    /// Tells the owner that a video picture arrived; see
    /// [`CallController::set_video_listener`].
    video_listener: Mutex<Option<Notifier>>,
    /// Takes each sound level of the running call; see
    /// [`CallController::set_sound_listener`].
    sound_listener: Mutex<Option<SoundListener>>,
    /// The picture store of the running worker, from its start to the end of
    /// the call or join, or of a camera test while no call runs (ADR-0059).
    video: Mutex<Option<VideoStore>>,
    next_id: AtomicU64,
    /// The newest device test (ADR-0059).
    device_test: Mutex<DeviceTestSlot>,
    /// Tells the owner that the device test changed.
    test_listener: Mutex<Option<Listener>>,
}

/// One member's sound level and the worker participant that sent it, so a
/// change of membership can check that the source still stands for it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LevelSample {
    level: u8,
    connection: u8,
    identity: String,
}

/// The newest device test and what ends it.
struct DeviceTestSlot {
    status: CallDeviceTestStatus,
    /// Numbers the tests; a test changes the status only while it is the
    /// newest.
    generation: u64,
    /// Set to end the running test.
    stop: Arc<AtomicBool>,
    /// The thread of the newest test. The next test waits for it, so two
    /// tests never hold a device at once.
    thread: Option<std::thread::JoinHandle<()>>,
    /// The threads that the next test thread waits for. A new thread takes
    /// them all when it starts; when no thread could start, they stay here
    /// for the next one. A handle is never dropped while its thread runs,
    /// so no test thread is ever left unwaited. Ended threads are pruned,
    /// so this holds at most the threads that still run.
    previous: Vec<std::thread::JoinHandle<()>>,
    /// The test that the newest thread runs once the test before it ended.
    /// `Some` while that thread still waits: a newer test then replaces
    /// this job and starts no thread, so at most two test threads exist.
    job: Option<DeviceTestJob>,
}

/// A device test that waits for its thread.
struct DeviceTestJob {
    generation: u64,
    test: CallDeviceTestKind,
    device: Option<String>,
    stop: Arc<AtomicBool>,
}

impl Default for DeviceTestSlot {
    fn default() -> Self {
        Self {
            status: CallDeviceTestStatus {
                id: 0,
                test: None,
                state: CallDeviceTestState::Idle,
                stream: 0,
            },
            generation: 0,
            stop: Arc::new(AtomicBool::new(true)),
            thread: None,
            previous: Vec::new(),
            job: None,
        }
    }
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

    /// Sets the status of device test `generation`, if it is still the
    /// newest, and tells the test listener. Called with no lock held.
    fn set_test(&self, generation: u64, change: impl FnOnce(&mut CallDeviceTestStatus)) {
        let changed = {
            let mut slot = plain(&self.device_test);
            if slot.generation != generation {
                return;
            }
            let before = slot.status.clone();
            change(&mut slot.status);
            slot.status != before
        };
        if changed && let Some(listener) = plain(&self.test_listener).clone() {
            listener();
        }
    }

    /// Gives the sound listener a level of the own side.
    fn own_level(&self, level: u8) {
        if let Some(listener) = plain(&self.sound_listener).clone() {
            listener(SoundUpdate::Own(level));
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
    devices: watch::Sender<Option<DeviceChoice>>,
    microphone: watch::Sender<Option<MicSettings>>,
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
                sound_listener: Mutex::new(None),
                video: Mutex::new(None),
                next_id: AtomicU64::new(0),
                device_test: Mutex::new(DeviceTestSlot::default()),
                test_listener: Mutex::new(None),
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

    /// Calls `listener` with each sound level the worker reports for the
    /// level meter (ADR-0055), on the call task. It must not block and must
    /// not call the controller. The level is not part of the call state.
    pub fn set_sound_listener(&self, listener: SoundListener) {
        *plain(&self.inner.sound_listener) = Some(listener);
    }

    /// Calls `listener` after each change of the device test's status, on
    /// the test's thread. It must not block and must not call the controller.
    pub fn set_test_listener(&self, listener: Listener) {
        *plain(&self.inner.test_listener) = Some(listener);
    }

    /// The newest device test (ADR-0059).
    pub fn device_test_status(&self) -> CallDeviceTestStatus {
        plain(&self.inner.device_test).status.clone()
    }

    /// Ends the device test that runs, if any. Its status follows when the
    /// worker has ended.
    pub fn stop_device_test(&self) {
        self.stop_device_test_if(None);
    }

    /// Ends the device test `id` if it is still the newest; `None` ends any.
    /// The check and the stop happen under the test lock, so a late stop
    /// from a closed page never ends a newer test (ADR-0059).
    pub fn stop_device_test_if(&self, id: Option<u64>) {
        let slot = plain(&self.inner.device_test);
        if id.is_none_or(|id| id == slot.generation) {
            slot.stop.store(true, Ordering::SeqCst);
        }
    }

    /// Starts a device test (ADR-0059) and returns its first status. A test
    /// that runs is stopped, and the new one waits for its end. While a call
    /// runs or joins the test is refused (`in_call`); where calls are not
    /// supported it is `unsupported`. The worker connects to nothing.
    pub fn start_device_test(
        &self,
        test: CallDeviceTestKind,
        device: Option<String>,
    ) -> CallDeviceTestStatus {
        // The test lock first, then the call check: a join that starts after
        // the check stops this test, because its stop takes the same lock
        // after the join holds the call slot. No path holds the call slot
        // while it waits for the test lock.
        let mut slot = plain(&self.inner.device_test);
        let refused = if !self.inner.host.supported() {
            Some(CallDeviceTestState::Unsupported)
        } else if !matches!(*self.inner.slot(), Slot::Idle) {
            Some(CallDeviceTestState::InCall)
        } else {
            None
        };
        // A refusal is the reply only. It is not the test's status, so it
        // does not stay after the call ends, and nothing that runs stops.
        if let Some(state) = refused {
            return CallDeviceTestStatus {
                id: 0,
                test: Some(test),
                state,
                stream: 0,
            };
        }
        slot.stop.store(true, Ordering::SeqCst);
        slot.generation += 1;
        slot.status = CallDeviceTestStatus {
            id: slot.generation,
            test: Some(test),
            state: CallDeviceTestState::Starting,
            stream: 0,
        };
        let stop = Arc::new(AtomicBool::new(false));
        slot.stop = stop.clone();
        let job = DeviceTestJob {
            generation: slot.generation,
            test,
            device,
            stop,
        };
        // A thread that still waits takes the newest job when it starts.
        if slot.job.replace(job).is_some() {
            return slot.status.clone();
        }
        start_test_thread(&mut slot, &self.inner, spawn_device_test);
        slot.status.clone()
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
            Ok(DirectConsent(proxy_configured))
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
        let devices = watch::channel(None).0;
        let microphone = watch::channel(None).0;
        *slot = Slot::Joining(Joining {
            id,
            room_id: room_id.to_owned(),
            flags: flags.clone(),
            kill: kill.clone(),
            devices: devices.clone(),
            microphone: microphone.clone(),
        });
        drop(slot);
        // The call needs the devices: a device test ends.
        self.stop_device_test();
        self.inner.publish(CallState::Joining {
            room_id: room_id.to_owned(),
        });
        Ok(JoinGuard {
            inner: self.inner.clone(),
            id,
            room_id: room_id.to_owned(),
            flags,
            kill,
            devices,
            microphone,
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
        let DirectConsent(proxy) = consent;
        let mut guard = self.claim(room_id)?;
        let access = Access {
            admit: self.inner.admission(),
            generation: scope.generation,
            flags: guard.flags.clone(),
        };
        let mut setup = Setup::new(client.clone(), guard.flags.clone(), proxy);
        let prepared = self
            .prepare(&scope, client, room_id, &access, &mut setup, &guard)
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
        let volume = call.volume.clone();
        let microphone = call.microphone.clone();
        let devices = call.devices.clone();
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
                        volume,
                        microphone,
                        devices,
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
        // The join's worker slot and device channel.
        guard: &JoinGuard,
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
        // The room's power levels must let this account send the call
        // membership. The check reads the local store and comes before any
        // request, delayed event or worker (ADR-0063).
        let levels = room.power_levels_or_default().await;
        if !can_send_member_state(&levels, &user_id) {
            return Err(CallError::NotPermitted);
        }
        flags.halt()?;

        // 3. An SFU; TURN is optional.
        let sfu = match sfu_transport(client).await {
            Ok(Discovery::Offered(service)) => service,
            Ok(Discovery::Unsupported) => return Err(CallError::SfuUnsupported),
            Ok(Discovery::NoneOffered) | Err(_) => return Err(CallError::NoSfu),
        };
        let focus = sfu.focus(room_id.as_str());
        let turn = turn_servers(client).await.ok().flatten();
        flags.halt()?;

        // 4. The worker, before any Matrix side effect.
        // The chosen sound server devices go to the launch (Linux).
        let audio = WorkerAudio::for_choice(
            scope.devices.recording.as_deref(),
            scope.devices.playout.as_deref(),
        );
        let link = start_worker(&self.inner.host, audio).await?;
        *plain(&guard.kill) = Some(link.kill_handle());
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
                    focus: focus.clone(),
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
            Some(RingPlan::Ring(contact)) => {
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
        let content = own_membership(&device_id, &user_id, &focus, Some(created_ts), scope.video);
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
            membership_error(&error)
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

        // 7. The SFU grant: from the JWT service, or from the homeserver
        // (ADR-0056). The LiveKit identity follows the flow.
        let own_identity = participant_identity(&user_id, &device_id);
        let (grant, own_backend) = match sfu.flow {
            Flow::OpenId => {
                let openid = client
                    .send(request_openid_token::v3::Request::new(user_id.clone()))
                    .await
                    .map_err(|_| CallError::OpenId)?;
                let body = sfu::request_body(room_id, &device_id, &OpenIdToken::from(openid));
                let grant = sfu::get_grant(client.http_client(), &sfu.url, &body)
                    .await
                    .map_err(CallError::Sfu)?;
                (grant, own_identity.clone())
            }
            Flow::Homeserver => {
                let server_name = user_id.server_name().to_string();
                let grant = sfu::get_homeserver_grant(
                    client,
                    sfu::TokenFor {
                        sfu: &sfu.url,
                        advertised: &sfu.advertised,
                        server_name: &server_name,
                        room_id,
                        member_id: &own_identity,
                    },
                )
                .await
                .map_err(CallError::Sfu)?;
                (grant, hashed_identity(&user_id, &own_identity))
            }
        };
        flags.halt()?;

        // 8. Keys, then the worker joins.
        let own_keys = OwnKeys::new(Instant::now()).map_err(|_| CallError::Keys)?;
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
            own_identity: own_backend,
            own_key: worker_key(own_keys.current()),
            recording_device: scope.devices.recording.clone(),
            playout_device: scope.devices.playout.clone(),
            camera_device: scope.devices.camera.clone(),
            camera_on: scope.video && !portal_camera,
            volume: scope.devices.volume(),
            auto_level: scope.devices.auto_level(),
            mic_gain: scope.devices.mic_gain(),
        };
        // Before the join, so the worker's connection records are kept
        // (ADR-0058). The call task sends the switch again when it starts.
        if crate::diagnostics::global().is_on() {
            let _ = link.send(ToWorker::SetDiagnostics { on: true });
        }
        if link.send(join).is_err() {
            setup.link = Some(link);
            return Err(CallError::Worker(StartError::NotReady));
        }
        let video_streams = link.video().streams();
        let (grants_tx, grants) = mpsc::channel(MAX_OTHER_SFUS);
        Ok(Call {
            access: access.clone(),
            link,
            delayed,
            state_key,
            own_identity,
            own_device: device_id,
            own_keys,
            remote: RemoteKeys::new(),
            unsent: BTreeMap::new(),
            unsent_retry: None,
            alone: Alone::default(),
            view: View::new(room_id.to_owned(), sfu.url, scope.video)
                .with_video_streams(video_streams)
                .with_volume(scope.devices.volume()),
            changes,
            incoming,
            failed,
            handles: std::mem::take(&mut setup.handles),
            tasks: std::mem::take(&mut setup.tasks),
            camera_wanted: Arc::new(AtomicBool::new(scope.video)),
            audio: AudioWants::default(),
            volume: watch::channel(scope.devices.volume()).0,
            microphone: guard.microphone.clone(),
            devices: guard.devices.clone(),
            levels: BTreeMap::new(),
            portal: None,
            portal_capture: false,
            portal_sent: false,
            ring: ring_watch.map(|(watch, _)| watch),
            screens: self.inner.host.screen_source(),
            screen_pick: None,
            screen_wanted: false,
            screen_last_on: false,
            screen_closed: None,
            screen_helper: None,
            screen_sent: false,
            proxy: setup.proxy,
            consent: None,
            grants,
            grants_tx,
            screen_control: Arc::default(),
            screen_pick_stops: 0,
            screen_seen_stops: 0,
            screen_seen_request: 0,
            screen_share: 0,
            screen_stop_pending: None,
            screen_stop_settled: None,
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
        self.queue_command(Command::Mute(muted))
    }

    /// Deafens or undeafens (ADR-0055): all remote audio goes off and the
    /// microphone mutes, or remote audio comes back and the microphone
    /// returns to the user's own mute choice. The state shows the worker's
    /// answers.
    pub fn set_deafened(&self, deafened: bool) -> Result<(), CallError> {
        self.queue_command(Command::Deafen(deafened))
    }

    /// Gives the running call task a command. A full queue is `Busy`, not
    /// `NotInCall`, so the caller can tell a dropped command from no call;
    /// a task that has ended is no call.
    fn queue_command(&self, command: Command) -> Result<(), CallError> {
        match &*self.inner.slot() {
            Slot::Running(run) => run.commands.try_send(command).map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => CallError::Busy,
                mpsc::error::TrySendError::Closed(_) => CallError::NotInCall,
            }),
            _ => Err(CallError::NotInCall),
        }
    }

    /// Sets the playout volume of the running call, 0 to 100 percent. Only
    /// the newest value is applied. The caller saves it for later calls.
    /// The value is kept even before the call task watches it, which then
    /// applies it when it starts.
    pub fn set_volume(&self, percent: u8) -> Result<(), CallError> {
        match &*self.inner.slot() {
            Slot::Running(run) => {
                run.volume.send_replace(percent.min(MAX_VOLUME_PERCENT));
                Ok(())
            }
            _ => Err(CallError::NotInCall),
        }
    }

    /// Sets the automatic microphone level and the manual microphone gain of
    /// the running or joining call (ADR-0062). A gain over 400 percent or off
    /// the 5 percent step is refused. Only the newest value is applied. The
    /// caller saves it for later calls. The value is kept even before the
    /// call task watches it, which then applies it when it starts, so a value
    /// set during the join replaces the saved one that the join sent.
    pub fn set_microphone(&self, auto_level: bool, gain: u16) -> Result<(), CallError> {
        if !valid_mic_gain(gain) {
            return Err(CallError::InvalidValue);
        }
        let settings = Some(MicSettings { auto_level, gain });
        match &*self.inner.slot() {
            Slot::Running(run) => {
                run.microphone.send_replace(settings);
                Ok(())
            }
            Slot::Joining(join) => {
                join.microphone.send_replace(settings);
                Ok(())
            }
            Slot::Idle => Err(CallError::NotInCall),
        }
    }

    /// Moves the running or joining call to another microphone and speaker,
    /// and gives it the camera for its next camera start (ADR-0059). Only
    /// the newest choice is sent, once the worker is
    /// connected; the worker's answer shows as the state's `device_change`.
    /// After a failed change the same choice is sent again, so choosing a
    /// device again (a headset plugged back in) reaches the worker.
    /// `NotInCall` without a call: the choice then applies to the next one.
    pub fn set_devices(
        &self,
        recording: Option<&str>,
        playout: Option<&str>,
        camera: Option<&str>,
    ) -> Result<(), CallError> {
        let choice = Some(DeviceChoice {
            recording: recording.map(str::to_owned),
            playout: playout.map(str::to_owned),
            camera: camera.map(str::to_owned),
        });
        // Read before the call slot is taken, so no lock waits on the other.
        let again = matches!(
            &*self.inner.state.borrow(),
            CallState::Active {
                device_change: Some(_),
                ..
            }
        );
        match &*self.inner.slot() {
            Slot::Running(run) => {
                offer_devices(&run.devices, choice, again);
                Ok(())
            }
            Slot::Joining(join) => {
                offer_devices(&join.devices, choice, again);
                Ok(())
            }
            Slot::Idle => Err(CallError::NotInCall),
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
        let changed = {
            let slot = self.inner.slot();
            let Slot::Running(run) = &*slot else {
                return Err(CallError::NotInCall);
            };
            if !matches!(*self.inner.state.borrow(), CallState::Active { .. }) {
                return Err(CallError::NotInCall);
            }
            self.accept_screen(run, on)?
        };
        if changed {
            self.inner.notify();
        }
        Ok(())
    }

    /// Accepts a screen command for the running call `run`, with the slot
    /// lock held. Returns whether the page changed.
    fn accept_screen(&self, run: &RunHandle, on: bool) -> Result<bool, CallError> {
        // A queue place is taken first: a full queue refuses a start and
        // changes nothing. A stop never waits for queue room: it does not go
        // through the queue then, but waits in the screen control and wakes
        // the call task, so capture never outlives the user's stop.
        let permit = match run.commands.try_reserve() {
            Ok(permit) => Some(permit),
            Err(mpsc::error::TrySendError::Full(())) if !on => None,
            Err(_) => return Err(CallError::NotInCall),
        };
        // One step under the screen control lock, which the call task holds
        // from its check of a finished pick until the pick's start is
        // queued, while it publishes, and while it closes the session:
        // 1. The command is counted, so a pick that ends after a stop is
        //    closed unused and a task publication keeps the page's state.
        // 2. A stop closes the share's session (`close_session`), so capture
        //    is revoked before `Off` shows: the portal session on Linux; on
        //    Windows and macOS the worker's stop is queued here, not after
        //    the call task's network work. A close by the task that is under
        //    way has finished: it holds the lock until `Close` is sent.
        // 3. The page shows the command.
        let (stops, request, changed) = {
            let mut control = plain(&run.screen_control);
            if !on {
                control.stops = control.stops.wrapping_add(1);
                close_session(&mut control);
            }
            control.requests = control.requests.wrapping_add(1);
            let (stops, request) = (control.stops, control.requests);
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
            if permit.is_none() {
                control.stop_waiting = Some((stops, request));
                control.wake.notify_one();
            }
            (stops, request, changed)
        };
        if let Some(permit) = permit {
            permit.send(Command::Screen { on, stops, request });
        }
        Ok(changed)
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
        // A device test ends too, with or without a call. The test lock is
        // taken and dropped before the call slot.
        self.stop_device_test();
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
    /// The profile has a SOCKS5 proxy (ADR-0054 asks about it).
    proxy: bool,
    link: Option<CallWorkerLink>,
    delayed: Option<DelayedLeave>,
    membership_sent: bool,
    handles: Vec<EventHandlerHandle>,
    tasks: JoinSet<()>,
}

impl Setup {
    fn new(client: Client, flags: Arc<Flags>, proxy: bool) -> Self {
        Self {
            client,
            flags,
            proxy,
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
/// (ADR-0055).
async fn ring_plan(
    room: &matrix_sdk::Room,
    own_user: &matrix_sdk::ruma::UserId,
    own_device: &matrix_sdk::ruma::DeviceId,
) -> Option<RingPlan> {
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
    plan: RingPlan,
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
    /// The `foci_preferred` entry (`SfuService::focus`).
    focus: serde_json::Value,
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
                            &membership.device_id,
                            &membership.user_id,
                            &membership.focus,
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
    /// Remote keys the worker queue had no room for, by member device
    /// (`{user}:{device}`): each a backend identity and a key index, oldest
    /// first. Only current members go in, each with at most
    /// [`MAX_UNSENT_PER_MEMBER`] keys.
    unsent: BTreeMap<String, VecDeque<(String, u8)>>,
    /// When the keys of `unsent` go again.
    unsent_retry: Option<Instant>,
    view: View,
    /// Who else is in the call, and the 30 second rule (ADR-0063).
    alone: Alone,
    changes: mpsc::Receiver<()>,
    incoming: mpsc::Receiver<Candidate>,
    failed: mpsc::Receiver<EndReason>,
    handles: Vec<EventHandlerHandle>,
    tasks: JoinSet<()>,
    /// The user's latest camera request. `set_camera` writes it before it
    /// queues the command; the task only reads it.
    camera_wanted: Arc<AtomicBool>,
    /// The user's mute and deafen choice (ADR-0055).
    audio: AudioWants,
    /// The playout volume. The controller sends; the task subscribes.
    volume: watch::Sender<u8>,
    /// The microphone level and gain (ADR-0062) set since the join started;
    /// `None` until one is set, as the Join carried the saved ones. The
    /// controller sends; the task subscribes.
    microphone: watch::Sender<Option<MicSettings>>,
    /// A device change during the call (ADR-0059). The controller sends;
    /// the task subscribes.
    devices: watch::Sender<Option<DeviceChoice>>,
    /// The level of each other member that is not silent (ADR-0059), at
    /// most [`MAX_CALL_MEMBERS`], with the worker participant it came from.
    levels: BTreeMap<CallParticipantLevelKey, LevelSample>,
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
    /// The last screen command read was a start. A failed start clears
    /// `screen_wanted` but not this: the end state of the share that the
    /// stop before it ended then does not show over the failure.
    screen_last_on: bool,
    /// Fires when the desktop closes the portal session.
    screen_closed: Option<oneshot::Receiver<()>>,
    /// Windows (ADR-0067): the capture helper of the running share. Dropping
    /// it stops it. It ends with the share, the call and the session.
    screen_helper: Option<HelperShare>,
    /// A `SetScreen` start went to the worker and no stop or end since.
    screen_sent: bool,
    /// The profile has a SOCKS5 proxy, which the connection to another SFU
    /// does not use (ADR-0054).
    proxy: bool,
    /// The question about another SFU's host that waits for the user. The
    /// call goes on meanwhile.
    consent: Option<ConsentTask>,
    /// A token request for another SFU reports here, with its connection
    /// number (at most [`MAX_OTHER_SFUS`] are in flight).
    grants: mpsc::Receiver<(u8, Option<SfuGrant>)>,
    grants_tx: mpsc::Sender<(u8, Option<SfuGrant>)>,
    /// The screen stops the handle accepted and the share's portal session.
    /// `set_screen` counts a stop and closes the session before it queues
    /// the stop. The task holds the lock from its check of a finished pick
    /// until the pick's start is queued.
    screen_control: Arc<Mutex<ScreenControl>>,
    /// A stop of share `n` that met a full worker queue. It goes as soon as
    /// the queue has room; no screen start goes before it.
    screen_stop_pending: Option<u32>,
    /// The `settled` flag of the running share's [`WorkerStop`] (Windows and
    /// macOS): set when that stop went to the worker, and set here when the
    /// worker ended the share, so no stop goes twice.
    screen_stop_settled: Option<Arc<AtomicBool>>,
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

/// The core's window that asks whether to connect to another SFU, running on
/// a blocking thread.
struct ConsentTask {
    host: String,
    answer: oneshot::Receiver<bool>,
}

/// Waits for the answer in `slot`, if a question is open; otherwise it never
/// ends. A window that ended without an answer counts as "Not now".
async fn consent_done(slot: &mut Option<ConsentTask>) -> (String, bool) {
    let Some(task) = slot.as_mut() else {
        return std::future::pending().await;
    };
    let answer = (&mut task.answer).await.unwrap_or(false);
    let host = std::mem::take(&mut task.host);
    *slot = None;
    (host, answer)
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
/// number of the last accepted screen command, and the share's open
/// session: the portal session on Linux, a [`WorkerStop`] on Windows and
/// macOS. Dropping the session closes it and revokes the capture.
#[derive(Default)]
struct ScreenControl {
    stops: u64,
    requests: u64,
    session: Option<Box<dyn Send + Sync>>,
    /// A stop (its stop count and number) that met a full command queue.
    /// The call task takes it when `wake` fires, or before it reads a later
    /// command. A newer waiting stop replaces an older one: it stops the
    /// same share or a later one.
    stop_waiting: Option<(u64, u64)>,
    /// Wakes the call task for a waiting stop. A wake with no task waiting
    /// is kept until it waits.
    wake: Arc<Notify>,
}

/// Closes the share's session, if open, with the screen control lock held:
/// a portal session sends `Close`, a [`WorkerStop`] queues the worker's stop.
/// Neither waits for a reply, so the lock is held only briefly. A stop that
/// comes meanwhile waits for the lock: its `Off` never shows before the
/// close has gone out.
fn close_session(control: &mut ScreenControl) {
    control.session = None;
}

/// The session of a Windows or macOS share (ADR-0053, ADR-0064), kept in
/// [`ScreenControl`] like a portal session. Such a share has no portal
/// session that the desktop revokes: only the worker's stop ends its copy.
/// Dropping this queues that stop at once, so a stop that the handle accepts
/// reaches the worker's queue before `Off` shows, also while the call task
/// waits for network work (a key send can take [`SEND_TIMEOUT`]). It queues
/// nothing once `settled` is set (the share ended, or its stop went). A full
/// queue leaves `settled` unset: the call task then sends the stop when it
/// reads the command, through its pending stop.
///
/// On Windows it also asks the share's capture helper to end (ADR-0067):
/// the helper stops capturing and closes the frame pipe, so the screen is
/// no longer copied even while the stop waits for queue room. The call task
/// still stops and joins the helper, and still sends the worker's stop.
struct WorkerStop {
    commands: mpsc::Sender<ToWorker>,
    share: u32,
    settled: Arc<AtomicBool>,
    helper: Option<HelperStop>,
}

impl Drop for WorkerStop {
    fn drop(&mut self) {
        if let Some(helper) = &self.helper {
            helper.stop();
        }
        if self.settled.load(Ordering::SeqCst) {
            return;
        }
        let stop = ToWorker::SetScreen {
            target: None,
            share: self.share,
        };
        if self.commands.try_send(stop).is_ok() {
            self.settled.store(true, Ordering::SeqCst);
        }
    }
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

/// Waits until the share's capture helper exits by itself, if one runs;
/// otherwise it never ends. Gives the share state that its exit means.
async fn screen_helper_exited(slot: &mut Option<HelperShare>) -> ScreenState {
    let Some(helper) = slot.as_mut() else {
        return std::future::pending().await;
    };
    helper.exited().await
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

/// Whether the homeserver answered the membership send with 403
/// `M_FORBIDDEN`: this account may not send it.
fn membership_forbidden(error: &matrix_sdk::Error) -> bool {
    error.as_client_api_error().is_some_and(|api| {
        api.status_code.as_u16() == 403
            && matches!(error.client_api_error_kind(), Some(ErrorKind::Forbidden))
    })
}

/// The join error for a failed membership send.
fn membership_error(error: &matrix_sdk::Error) -> CallError {
    if membership_forbidden(error) {
        CallError::NotPermitted
    } else {
        CallError::Membership
    }
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

/// Starts the thread for the job in `slot` with `spawn`. The newest thread
/// joins the kept threads, which ended threads are first pruned from. The
/// handle is added, never put in place of one, so no running thread is
/// detached whatever order the threads take their work in. A failed start
/// keeps them all for the next thread and ends the job as `failed`.
fn start_test_thread(
    slot: &mut DeviceTestSlot,
    inner: &Arc<Inner>,
    spawn: impl FnOnce(Arc<Inner>) -> std::io::Result<std::thread::JoinHandle<()>>,
) {
    slot.previous.retain(|thread| !thread.is_finished());
    if let Some(thread) = slot.thread.take() {
        slot.previous.push(thread);
    }
    match spawn(inner.clone()) {
        Ok(thread) => slot.thread = Some(thread),
        Err(_) => {
            slot.job = None;
            slot.status.state = CallDeviceTestState::Failed;
        }
    }
}

/// A device test thread: it waits for every thread before it, then runs
/// the newest job, if one is left.
fn spawn_device_test(inner: Arc<Inner>) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("call-device-test".to_owned())
        .spawn(move || {
            let previous = std::mem::take(&mut plain(&inner.device_test).previous);
            for thread in previous {
                let _ = thread.join();
            }
            let job = plain(&inner.device_test).job.take();
            if let Some(job) = job {
                let limit = device_test::TEST_LIMIT;
                run_device_test(
                    &inner,
                    job.generation,
                    job.test,
                    job.device,
                    &job.stop,
                    limit,
                );
            }
        })
}

/// Replaces a kept device choice with a newer one that waits unread in
/// `devices`, so a retry sends the newest choice only.
fn take_newer_choice(
    devices: &mut watch::Receiver<Option<DeviceChoice>>,
    pending: &mut Option<(Instant, DeviceChoice)>,
) {
    if devices.has_changed().unwrap_or(false)
        && let Some(choice) = devices.borrow_and_update().clone()
    {
        *pending = Some((Instant::now(), choice));
    }
}

/// Gives the call a microphone and speaker choice. The same choice as the
/// last one changes nothing, so saving only the camera does not restart the
/// sound of a running call; with `again` (the last change failed) it is
/// sent again.
fn offer_devices(
    devices: &watch::Sender<Option<DeviceChoice>>,
    choice: Option<DeviceChoice>,
    again: bool,
) {
    devices.send_if_modified(|current| {
        if *current == choice && !again {
            false
        } else {
            *current = choice;
            true
        }
    });
}

/// Runs device test `generation` on its own thread (ADR-0059): starts a
/// worker, sends `TestDevice` and follows the answers until the worker ends,
/// `stop` is set or `limit` ([`device_test::TEST_LIMIT`]) passes. Only the
/// user's stop ends a test that has not run as idle; the limit is no user
/// stop, so it ends that test as failed. A camera test's store
/// is the controller's picture store while no call runs. The worker gets the
/// diagnostic log's switch before the test and on each change, and its
/// records go to the log (ADR-0058), so a failing test leaves a record.
fn run_device_test(
    inner: &Arc<Inner>,
    generation: u64,
    test: CallDeviceTestKind,
    device: Option<String>,
    stop: &AtomicBool,
    limit: Duration,
) {
    let end = |state| inner.set_test(generation, |status| status.state = state);
    if stop.load(Ordering::SeqCst) {
        end(CallDeviceTestState::Idle);
        return;
    }
    // The limit counts from here, so it covers the worker start and the
    // Linux portal wait too.
    let limit = std::time::Instant::now() + limit;
    // A `pulse:` choice goes to the launch as for a call (Linux, ADR-0049).
    let audio = device_test::worker_audio(test, device.as_deref());
    let Ok(mut link) = inner.host.start(&audio) else {
        end(CallDeviceTestState::Failed);
        return;
    };
    let store = link.video();
    let camera = test == CallDeviceTestKind::Camera;
    if camera {
        store.sync(true, [false; REMOTE_TILES]);
        if let Some(notifier) = plain(&inner.video_listener).clone() {
            store.set_notifier(notifier);
        }
        // Under the slot lock, so a join that starts now replaces it.
        let slot = inner.slot();
        let mut video = plain(&inner.video);
        if matches!(*slot, Slot::Idle) && video.is_none() {
            *video = Some(store.clone());
        }
        drop(video);
        drop(slot);
        let stream = store.stream(0);
        inner.set_test(generation, |status| status.stream = stream);
    }
    // Portal path (Linux): the camera test gets one remote before
    // `TestDevice`, as a listing does. The desktop may ask the user; the
    // user's stop ends that wait. Without access nothing goes, and the
    // worker reports the camera unavailable.
    #[cfg(target_os = "linux")]
    if camera {
        super::devices::send_portal_remote(&link, stop);
    }
    let mut diagnostics = crate::diagnostics::global().watch();
    let on = *diagnostics.borrow_and_update();
    // The order rule: a stop recorded while the worker started (its ready
    // wait can take seconds) ends the test here, before `TestDevice`, so no
    // device opens. A later stop reaches the worker as `Leave` after
    // `TestDevice`; the worker may open the device, and closes it when it
    // reads `Leave`.
    let stopped_early = stop.load(Ordering::SeqCst);
    let sent = if stopped_early {
        Ok(())
    } else {
        link.send(ToWorker::SetDiagnostics { on }).and_then(|()| {
            link.send(ToWorker::TestDevice {
                test: device_test::worker_test(test),
                device,
            })
        })
    };
    let mut state = if stopped_early {
        CallDeviceTestState::Idle
    } else {
        CallDeviceTestState::Starting
    };
    let mut leave_by: Option<std::time::Instant> = None;
    // Whether the `Leave` was the user's stop, not the limit.
    let mut user_stopped = false;
    if sent.is_err() {
        state = CallDeviceTestState::Failed;
    }
    while !device_test::is_final(state) {
        let now = std::time::Instant::now();
        if leave_by.is_none() && (stop.load(Ordering::SeqCst) || now >= limit) {
            user_stopped = stop.load(Ordering::SeqCst);
            let _ = link.send(ToWorker::Leave);
            leave_by = Some(now + device_test::LEAVE_WAIT);
        }
        if leave_by.is_some_and(|by| now >= by) {
            state = CallDeviceTestState::Failed;
            break;
        }
        if diagnostics.has_changed().unwrap_or(false) {
            let on = *diagnostics.borrow_and_update();
            let _ = link.send(ToWorker::SetDiagnostics { on });
        }
        match link.events.try_recv() {
            // After the user's stop the meter shows nothing more.
            Ok(FromWorker::SoundLevel { level }) => {
                if !user_stopped {
                    inner.own_level(level);
                }
            }
            Ok(FromWorker::Diagnostic {
                level,
                target,
                message,
            }) => worker_record(level, &target, &message),
            Ok(message) => {
                if let Some(next) = device_test::next_state(state, &message, user_stopped) {
                    state = next;
                    inner.set_test(generation, |status| status.state = next);
                }
            }
            Err(mpsc::error::TryRecvError::Empty) => std::thread::sleep(device_test::POLL),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                state = CallDeviceTestState::Failed;
            }
        }
    }
    link.finish(device_test::FINISH_TIMEOUT);
    if camera {
        let mut video = plain(&inner.video);
        if video.as_ref().is_some_and(|shown| shown.is_same(&store)) {
            *video = None;
        }
        drop(video);
        store.close();
    }
    inner.own_level(0);
    inner.set_test(generation, |status| {
        status.state = state;
        status.stream = 0;
    });
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
    // The meters show nothing once the call is over.
    if let Some(listener) = plain(&inner.sound_listener).clone() {
        listener(SoundUpdate::Own(0));
        listener(SoundUpdate::Participants(Vec::new()));
    }
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
    ///
    /// A stop that waits in the screen control and is older than this
    /// command goes first. A command older than a stop already read is
    /// stale and does nothing. A start whose stop count is behind the
    /// accepted count is followed by a stop: it opens no picker.
    fn screen_request(
        &mut self,
        inner: &Inner,
        on: bool,
        stops: u64,
        request: u64,
    ) -> Result<(), EndReason> {
        self.take_waiting_stop(inner, request)?;
        if request <= self.screen_seen_request {
            return Ok(());
        }
        self.screen_seen_request = request;
        let superseded = on && stops != plain(&self.screen_control).stops;
        if !superseded {
            self.screen_command(inner, on, stops)?;
        }
        self.publish(inner);
        Ok(())
    }

    /// Runs the stop that waits in the screen control, if it is older than
    /// command `before`.
    fn take_waiting_stop(&mut self, inner: &Inner, before: u64) -> Result<(), EndReason> {
        let waiting = {
            let mut control = plain(&self.screen_control);
            match control.stop_waiting {
                Some((_, request)) if request < before => control.stop_waiting.take(),
                _ => None,
            }
        };
        if let Some((stops, request)) = waiting {
            self.screen_seen_request = request;
            self.screen_command(inner, false, stops)?;
            self.publish(inner);
        }
        Ok(())
    }

    /// Starts or stops the screen share as the user asked (ADR-0053). A start
    /// asks the platform's source off the call loop: the user answers in a
    /// window the core or the desktop draws. A stop cancels a pick in flight,
    /// closes the portal session and stops the worker's capture.
    fn screen_command(&mut self, inner: &Inner, on: bool, stops: u64) -> Result<(), EndReason> {
        self.screen_wanted = on;
        self.screen_last_on = on;
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
        // On Windows and macOS this queues the worker's stop, unless the
        // handle's close already did. The helper stops with the share.
        self.close_screen_session();
        self.screen_helper = None;
        if self.screen_sent {
            self.screen_sent = false;
            if self.screen_stop_needed() {
                self.send_screen_stop()?;
            }
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
        // The number the share will have: only a finished pick starts a
        // share, and picks run one at a time, so `screen_picked` takes the
        // same number (and refuses a helper that has another one).
        let share = self.screen_share.wrapping_add(1);
        self.screen_pick = Some(ScreenPick {
            task: tokio::task::spawn_blocking(move || {
                let grant = source.choose(&flag)?;
                // Windows: the helper starts here, after the user's Yes.
                source.capture(grant, share, &flag)
            }),
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
        // A stop still waiting for room goes first, so no start overtakes
        // it: the share fails. The grant drops here, which closes its
        // session.
        if self.screen_stop_pending.is_some() {
            drop(accepted);
            self.screen_failed(inner);
            return Ok(());
        }
        // Every command of the start has its slot before any goes out
        // (Windows: `ScreenFrames` and `SetScreen`). A full command queue is
        // a worker that has not read its commands yet, not one that
        // stopped: the share alone fails, the call runs on, and no pipe goes
        // into the worker.
        let commands = self.link.command_sender();
        let mut slots = match commands.try_reserve_many(SCREEN_START_COMMANDS) {
            Ok(slots) => slots,
            Err(mpsc::error::TrySendError::Full(())) => {
                drop(accepted);
                self.screen_failed(inner);
                return Ok(());
            }
            Err(mpsc::error::TrySendError::Closed(())) => return Err(EndReason::WorkerStopped),
        };
        let ScreenGrant {
            target,
            remote,
            session,
            closed,
            helper,
        } = grant;
        let share = self.screen_share.wrapping_add(1);
        // A helper that serves another share than this one would write
        // frames that the worker refuses: it is stopped (dropped) and the
        // share fails.
        if helper
            .as_ref()
            .is_some_and(|helper| helper.share() != share)
        {
            drop(accepted);
            self.screen_failed(inner);
            return Ok(());
        }
        let Some(start) = slots.next() else {
            drop(accepted);
            self.screen_failed(inner);
            return Ok(());
        };
        // The remote goes first and the command says so by its target: the
        // worker takes the oldest screen remote with it (Windows: the frame
        // pipe goes into the worker process and `ScreenFrames`, in its own
        // slot, names it).
        if !self.send_screen_remote(remote, &target, share, &mut slots) {
            drop(accepted);
            self.screen_failed(inner);
            return Ok(());
        }
        accepted.session = session;
        self.screen_closed = closed;
        let helper_stop = helper.as_ref().and_then(HelperShare::stopper);
        self.screen_helper = helper;
        self.screen_sent = true;
        self.screen_share = share;
        start.send(ToWorker::SetScreen {
            target: Some(target),
            share,
        });
        // A share with no portal session (Windows, macOS) gets a
        // `WorkerStop` as its session, after its start is queued and under
        // the same lock: a stop that the handle accepts then queues the
        // worker's stop behind the start.
        if accepted.session.is_none() {
            let settled = Arc::new(AtomicBool::new(false));
            accepted.session = Some(Box::new(WorkerStop {
                commands: self.link.command_sender(),
                share: self.screen_share,
                settled: settled.clone(),
                helper: helper_stop,
            }));
            self.screen_stop_settled = Some(settled);
        }
        drop(accepted);
        Ok(())
    }

    /// Whether the running share's stop still has to go to the worker: not
    /// when its [`WorkerStop`] already queued it. Forgets the share's flag.
    fn screen_stop_needed(&mut self) -> bool {
        !self
            .screen_stop_settled
            .take()
            .is_some_and(|settled| settled.load(Ordering::SeqCst))
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
        _share: u32,
        _slots: &mut mpsc::PermitIterator<'_, ToWorker>,
    ) -> bool {
        use persephone_call_protocol::ScreenTarget;
        use std::os::fd::AsFd;
        match (target, remote) {
            (ScreenTarget::Portal { .. }, Some(remote)) => {
                self.link.send_screen_handle(remote.as_fd()).is_ok()
            }
            (ScreenTarget::Portal { .. }, None) => false,
            (ScreenTarget::Monitor { .. } | ScreenTarget::Display { .. }, _) => true,
        }
    }

    /// Windows (ADR-0067): a monitor's frame pipe goes into the worker
    /// process, and `ScreenFrames` names it for share `share` in one of
    /// `slots`; the caller sends the share's `SetScreen` next, in a slot it
    /// took before. The core's own copy of the pipe closes when `remote`
    /// drops. A monitor without a pipe goes on: the worker has none for it
    /// and answers `Unavailable`. A target of another OS is refused.
    #[cfg(windows)]
    fn send_screen_remote(
        &self,
        remote: Option<super::screen::Remote>,
        target: &persephone_call_protocol::ScreenTarget,
        share: u32,
        slots: &mut mpsc::PermitIterator<'_, ToWorker>,
    ) -> bool {
        use persephone_call_protocol::ScreenTarget;
        match (target, remote) {
            (ScreenTarget::Monitor { .. }, Some(frames)) => slots
                .next()
                .is_some_and(|slot| self.link.send_screen_frames(&frames, share, slot).is_ok()),
            (ScreenTarget::Monitor { .. }, None) => true,
            (ScreenTarget::Portal { .. } | ScreenTarget::Display { .. }, _) => false,
        }
    }

    /// On macOS there is no remote: a target that needs one is refused.
    #[cfg(not(any(target_os = "linux", windows)))]
    fn send_screen_remote(
        &self,
        _remote: Option<super::screen::Remote>,
        target: &persephone_call_protocol::ScreenTarget,
        _share: u32,
        _slots: &mut mpsc::PermitIterator<'_, ToWorker>,
    ) -> bool {
        matches!(
            target,
            persephone_call_protocol::ScreenTarget::Monitor { .. }
                | persephone_call_protocol::ScreenTarget::Display { .. }
        )
    }

    /// The desktop closed the portal session (the user stopped the share
    /// from the desktop). The worker's capture ends with it: say so.
    fn screen_ended_by_desktop(&mut self, inner: &Inner) -> Result<(), EndReason> {
        self.close_screen_session();
        self.screen_helper = None;
        self.screen_wanted = false;
        if self.screen_sent {
            self.screen_sent = false;
            if self.screen_stop_needed() {
                self.send_screen_stop()?;
            }
        }
        if self.view.set_screen(CallScreen::Off) == Applied::Changed {
            self.publish(inner);
        }
        Ok(())
    }

    /// The capture helper exited by itself (ADR-0067): a monitor that went
    /// or changed is `NotFound`, a refused or failed capture is
    /// `Unavailable`, and an end that nobody asked for is `Off`. The
    /// worker's capture is stopped and the share is over; the call goes on.
    /// The worker's own end item says the same, so whichever comes first
    /// shows, and the later state of the same share is ignored.
    fn screen_ended_by_helper(
        &mut self,
        inner: &Inner,
        state: ScreenState,
    ) -> Result<(), EndReason> {
        self.close_screen_session();
        self.screen_wanted = false;
        self.screen_closed = None;
        // The helper is gone; its share state stands for the whole share.
        self.screen_helper = None;
        if self.screen_sent {
            self.screen_sent = false;
            if self.screen_stop_needed() {
                self.send_screen_stop()?;
            }
        }
        let shown = match state {
            ScreenState::NotFound => CallScreen::NotFound,
            ScreenState::Refused => CallScreen::Refused,
            ScreenState::Unavailable => CallScreen::Unavailable,
            ScreenState::Off | ScreenState::Starting | ScreenState::On => CallScreen::Off,
        };
        if self.view.set_screen(shown) == Applied::Changed {
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
            return !self.screen_last_on
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
            // The worker ended the share (its end item says why): the
            // helper has nothing left to serve.
            self.screen_helper = None;
            // The worker ended the share: its `WorkerStop` sends nothing.
            if let Some(settled) = self.screen_stop_settled.take() {
                settled.store(true, Ordering::SeqCst);
            }
            self.close_screen_session();
        }
        true
    }

    /// Closes the share's portal session, if open, under the screen control
    /// lock (`close_session`).
    fn close_screen_session(&self) {
        close_session(&mut plain(&self.screen_control));
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

    /// Called when the alone deadline fires. A join may wait unread then:
    /// `select!` picks among ready arms at random, and `members_changed` can
    /// hold the loop past the deadline. So the worker's waiting messages are
    /// applied first, at most one queue's worth, and the call ends only if
    /// the deadline still holds after them.
    fn alone_still_due(&mut self, inner: &Inner) -> Result<bool, EndReason> {
        for _ in 0..EVENT_QUEUE {
            match self.link.events.try_recv() {
                Ok(event) => self.worker_event(inner, &event)?,
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Err(EndReason::WorkerStopped);
                }
            }
        }
        Ok(self.alone.is_due(time::Instant::now()))
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
        self.alone.observe(event, time::Instant::now());
        if let FromWorker::Diagnostic {
            level,
            target,
            message,
        } = event
        {
            worker_record(*level, target, message);
            return Ok(());
        }
        // The level meter follows the worker's number at once; it is not
        // call state (ADR-0055). The reader already bounded it.
        if let FromWorker::SoundLevel { level } = event {
            if let Some(listener) = plain(&inner.sound_listener).clone() {
                listener(SoundUpdate::Own(*level));
            }
            return Ok(());
        }
        if let FromWorker::ParticipantLevel {
            connection,
            identity,
            level,
        } = event
        {
            self.participant_level(inner, *connection, identity, *level);
            return Ok(());
        }
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

    /// Keeps one member's level and gives the meters every level when it
    /// changed. Only a current member counts; silence removes it.
    fn participant_level(&mut self, inner: &Inner, connection: u8, identity: &str, level: u8) {
        let Some(key) = self.view.level_member(connection, identity) else {
            return;
        };
        let changed = if level == 0 {
            self.levels.remove(&key).is_some()
        } else if self.levels.len() < MAX_CALL_MEMBERS || self.levels.contains_key(&key) {
            let sample = LevelSample {
                level,
                connection,
                identity: identity.to_owned(),
            };
            self.levels.insert(key, sample).map(|old| old.level) != Some(level)
        } else {
            false
        };
        if changed {
            self.send_levels(inner);
        }
    }

    /// Forgets each level whose source no longer stands for its device: the
    /// member left, or moved to an SFU that the sample's connection does not
    /// speak for. It gives the meters the rest. Such a source's silence is
    /// not taken (`level_member`), so without this its last level would stay.
    fn prune_levels(&mut self, inner: &Inner) {
        let before = self.levels.len();
        let view = &self.view;
        self.levels.retain(|key, sample| {
            view.level_member(sample.connection, &sample.identity)
                .as_ref()
                == Some(key)
        });
        if self.levels.len() != before {
            self.send_levels(inner);
        }
    }

    /// Gives the meters the levels of the members now, at most
    /// `CALL_PARTICIPANTS_MAX`, so the list is never refused as too long.
    fn send_levels(&self, inner: &Inner) {
        if let Some(listener) = plain(&inner.sound_listener).clone() {
            listener(SoundUpdate::Participants(
                self.levels
                    .iter()
                    .take(CALL_PARTICIPANTS_MAX)
                    .map(|((user_id, device_id), sample)| CallParticipantLevel {
                        user_id: user_id.clone(),
                        device_id: device_id.clone(),
                        level: sample.level,
                    })
                    .collect(),
            ));
        }
    }

    /// Looks for members on other SFUs and acts on what the user has and has
    /// not been asked (ADR-0054). The call goes on while a question is open.
    fn plan_others(&mut self, inner: &Inner) -> Result<(), EndReason> {
        self.view
            .other
            .track(&self.view.members, &self.view.own_sfu);
        let plan = self.view.other.plan();
        self.act_on_plan(inner, plan);
        Ok(())
    }

    fn act_on_plan(&mut self, inner: &Inner, plan: Plan) {
        for index in plan.connect {
            self.request_grant(index);
        }
        if let Some(host) = plan.ask {
            self.ask_consent(inner, host);
        }
    }

    /// Asks in the core's own window, on a blocking thread, whether to
    /// connect to the SFUs on `host`. The window names the host, says the
    /// server will see the IP address and the Matrix ID, and, with a profile
    /// proxy, that this connection does not use it.
    fn ask_consent(&mut self, inner: &Inner, host: String) {
        let (answer_tx, answer) = oneshot::channel();
        let asker = inner.host.clone();
        let name = host.clone();
        let proxy = self.proxy;
        tokio::task::spawn_blocking(move || {
            let _ = answer_tx.send(asker.confirm_other_sfu(&name, proxy));
        });
        self.consent = Some(ConsentTask { host, answer });
    }

    fn consent_answered(
        &mut self,
        inner: &Inner,
        host: &str,
        connect: bool,
    ) -> Result<(), EndReason> {
        let plan = self.view.other.answer(host, connect);
        self.act_on_plan(inner, plan);
        self.publish(inner);
        Ok(())
    }

    /// Requests the OpenID token for connection `index` and its SFU grant, on
    /// a task of the call. The token goes to that SFU's service only. The
    /// profile's HTTP client follows no redirect.
    fn request_grant(&mut self, index: u8) {
        let Some(service) = self.view.other.connecting_url(index).cloned() else {
            return;
        };
        let access = self.access.clone();
        let room_id = self.view.room_id.clone();
        let device_id = self.own_device.clone();
        let sender = self.grants_tx.clone();
        self.tasks.spawn(async move {
            let work = access.run(|client| async move {
                let user_id = client.user_id()?.to_owned();
                let openid = client
                    .send(request_openid_token::v3::Request::new(user_id))
                    .await
                    .ok()?;
                let body = sfu::request_body(&room_id, &device_id, &OpenIdToken::from(openid));
                sfu::get_grant(client.http_client(), &service, &body)
                    .await
                    .ok()
            });
            let grant = time::timeout(SEND_TIMEOUT * 2, work)
                .await
                .ok()
                .and_then(Result::ok)
                .flatten();
            let _ = sender.send((index, grant)).await;
        });
    }

    /// A token request ended. A grant goes to the worker as `Subscribe`, its
    /// token moved and not copied; a failure leaves those members "not
    /// reachable". Neither ends the call.
    fn grant_received(
        &mut self,
        inner: &Inner,
        index: u8,
        grant: Option<SfuGrant>,
    ) -> Result<(), EndReason> {
        let wanted = self.view.other.connecting_url(index).is_some();
        match grant {
            // The answer's host follows the same rules as the service host.
            Some(grant) if wanted && is_other_host(&grant.url) => {
                let SfuGrant { url, jwt } = grant;
                self.send_worker(ToWorker::Subscribe {
                    index,
                    url: url.to_string(),
                    jwt,
                })?;
            }
            _ => {
                if self.view.other.token_failed(index) {
                    self.publish(inner);
                }
            }
        }
        Ok(())
    }

    fn send_worker(&self, message: ToWorker) -> Result<(), EndReason> {
        self.link
            .send(message)
            .map_err(|_| EndReason::WorkerStopped)
    }

    /// Notes keys of `device` (a backend identity and a key index each) that
    /// must still go to the worker, oldest first. A key already waiting is
    /// not added twice, and past [`MAX_UNSENT_PER_MEMBER`] the oldest goes.
    fn mark_unsent(&mut self, device: String, keys: impl IntoIterator<Item = (String, u8)>) {
        let waiting = self.unsent.entry(device).or_default();
        for key in keys {
            if !waiting.contains(&key) {
                waiting.push_back(key);
            }
        }
        while waiting.len() > MAX_UNSENT_PER_MEMBER {
            waiting.pop_front();
        }
        self.unsent_retry
            .get_or_insert_with(|| Instant::now() + UNSENT_RETRY);
    }

    /// Gives the worker the keys in `unsent`, oldest first, while its queue
    /// has room. Each goes with the bytes stored for its index now, and only
    /// while its device is a member that still has that backend identity;
    /// otherwise it is dropped. A key that goes is not sent again, so a
    /// retry with little room goes on where the last one stopped. A device
    /// is shown as keyed once a key of it was queued and none of its keys
    /// waits, so each of its backend identities has the key: `true` when
    /// that changed what the page shows. Only a closed queue is an error.
    fn send_unsent(&mut self) -> Result<bool, EndReason> {
        self.unsent_retry = None;
        let mut keyed = false;
        let mut pending = std::mem::take(&mut self.unsent).into_iter();
        while let Some((device, mut waiting)) = pending.next() {
            let Some(backends) = self
                .view
                .key_member(&device)
                .map(|m| m.backend_identities.clone())
            else {
                continue;
            };
            let mut sent = false;
            while let Some((identity, index)) = waiting.front() {
                // Only the key that goes is copied out of the store.
                let stored = if backends.contains(identity) {
                    self.remote.get(&device, *index)
                } else {
                    None
                };
                let Some(bytes) = stored else {
                    waiting.pop_front();
                    continue;
                };
                let key = ToWorker::SetKey {
                    identity: identity.clone(),
                    key: FrameKeyMsg {
                        index: *index,
                        bytes: Zeroizing::new(bytes.to_vec()),
                    },
                };
                if !self.queue_worker(key)? {
                    // Devices that left drop out here too, so the map holds
                    // only current members.
                    self.unsent.insert(device, waiting);
                    let view = &self.view;
                    self.unsent
                        .extend(pending.filter(|(device, _)| view.key_member(device).is_some()));
                    self.unsent_retry = Some(Instant::now() + UNSENT_RETRY);
                    return Ok(keyed);
                }
                waiting.pop_front();
                sent = true;
            }
            if sent {
                keyed |= self.view.keyed.insert(device);
            }
        }
        Ok(keyed)
    }

    /// Switches the worker to the own key in `pending`, which is due. When
    /// the queue is full, the switch waits for the next retry instead of
    /// ending the call: the members already have the key, and the old one
    /// stays in use until then.
    fn apply_own_key(&self, pending: &mut Option<(Instant, FrameKeyMsg)>) -> Result<(), EndReason> {
        if let Some((_, key)) = pending.take()
            && !self.queue_worker(ToWorker::SetOwnKey { key: key.clone() })?
        {
            *pending = Some((Instant::now() + UNSENT_RETRY, key));
        }
        Ok(())
    }

    /// Gives the worker the diagnostic log's switch in `pending`. When the
    /// queue is full, the switch waits for the next retry instead of ending
    /// the call; a newer value replaces a waiting one, so only the latest
    /// goes. The worker filters its records by its last value until then,
    /// and the core drops records from a worker while its log is off.
    fn apply_diagnostics(&self, pending: &mut Option<(Instant, bool)>) -> Result<(), EndReason> {
        if let Some((_, on)) = pending.take()
            && !self.queue_worker(ToWorker::SetDiagnostics { on })?
        {
            *pending = Some((Instant::now() + UNSENT_RETRY, on));
        }
        Ok(())
    }

    /// Sends the pending device choice (ADR-0059). A full queue keeps it for
    /// a retry, so a busy worker does not end the call; a newer choice
    /// replaces it.
    fn apply_devices(
        &self,
        pending: &mut Option<(Instant, DeviceChoice)>,
    ) -> Result<(), EndReason> {
        if let Some((_, choice)) = pending.take() {
            let message = ToWorker::SetDevices {
                recording: choice.recording.clone(),
                playout: choice.playout.clone(),
                camera: choice.camera.clone(),
            };
            if !self.queue_worker(message)? {
                *pending = Some((Instant::now() + UNSENT_RETRY, choice));
            }
        }
        Ok(())
    }

    /// Queues `message` when the worker's queue has room: `false` when it is
    /// full. Only a closed queue is an error.
    fn queue_worker(&self, message: ToWorker) -> Result<bool, EndReason> {
        self.link
            .try_queue(message)
            .map_err(|_| EndReason::WorkerStopped)
    }

    fn send_all(&self, messages: Vec<ToWorker>) -> Result<(), EndReason> {
        messages
            .into_iter()
            .try_for_each(|message| self.send_worker(message))
    }

    /// The user moved the volume: the worker applies it to every remote
    /// audio track, and the page sees it at once (the worker does not
    /// answer a volume change).
    fn volume_changed(&mut self, inner: &Inner, percent: u8) -> Result<(), EndReason> {
        let percent = percent.min(MAX_VOLUME_PERCENT);
        self.send_worker(ToWorker::SetVolume { percent })?;
        if self.view.set_volume(percent) == Applied::Changed {
            self.publish(inner);
        }
        Ok(())
    }

    /// The user changed the microphone level or gain: the worker applies it
    /// to the audio processing at once and does not answer. The values were
    /// checked when they were set; the check here keeps a bad one from ever
    /// reaching the wire.
    fn microphone_changed(&mut self, settings: MicSettings) -> Result<(), EndReason> {
        if !valid_mic_gain(settings.gain) {
            return Ok(());
        }
        self.send_worker(ToWorker::SetMicrophone {
            auto_level: settings.auto_level,
            mic_gain: settings.gain,
        })
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
        self.plan_others(inner)?;
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
        // A volume set between the Join and now is applied at once; when
        // there was none, the worker gets the Join's volume again.
        let mut volume = self.volume.subscribe();
        volume.mark_changed();
        // The same for the microphone settings (ADR-0062), but with none set
        // since the join started, the Join's settings stand.
        let mut microphone = self.microphone.subscribe();
        microphone.mark_changed();
        // A device choice made since the join started; the join used the
        // saved one. A choice made before this point is sent at once.
        let mut devices = self.devices.subscribe();
        devices.mark_changed();
        // The diagnostic log's switch, sent at once and on each change.
        let mut diagnostics = crate::diagnostics::global().watch();
        diagnostics.mark_changed();
        let mut pending_diagnostics: Option<(Instant, bool)> = None;
        // The newest device choice while the worker's queue is full.
        let mut pending_devices: Option<(Instant, DeviceChoice)> = None;
        self.start_keys(inner).await?;
        // A video join on the portal path asks for the camera now (the join
        // sent `camera_on: false`).
        if self.camera_wanted.load(Ordering::SeqCst) && uses_portal(&self.link) {
            self.start_portal();
        }
        let wake = plain(&self.screen_control).wake.clone();
        loop {
            let due = pending_own.as_ref().map(|(at, _)| *at);
            let retry = self.unsent_retry;
            let diagnostics_due = pending_diagnostics.map(|(at, _)| at);
            let devices_due = pending_devices.as_ref().map(|(at, _)| *at);
            tokio::select! {
                _ = &mut *stop => return Ok(()),
                () = wake.notified() => self.take_waiting_stop(inner, u64::MAX)?,
                command = commands.recv() => match command {
                    Some(Command::Mute(muted)) => {
                        let messages = self.audio.set_muted(muted);
                        self.send_all(messages)?;
                    }
                    Some(Command::Deafen(deafened)) => {
                        let messages = self.audio.set_deafened(deafened);
                        self.send_all(messages)?;
                    }
                    Some(Command::Camera { on, request }) => {
                        self.camera_command(inner, on, request)?;
                    }
                    Some(Command::Screen { on, stops, request }) => {
                        self.screen_request(inner, on, stops, request)?;
                    }
                    None => return Ok(()),
                },
                Ok(()) = volume.changed() => {
                    let percent = *volume.borrow_and_update();
                    self.volume_changed(inner, percent)?;
                }
                Ok(()) = microphone.changed() => {
                    let settings = *microphone.borrow_and_update();
                    if let Some(settings) = settings {
                        self.microphone_changed(settings)?;
                    }
                }
                Ok(()) = devices.changed() => {
                    let choice = devices.borrow_and_update().clone();
                    if let Some(choice) = choice {
                        pending_devices = Some((Instant::now(), choice));
                        self.apply_devices(&mut pending_devices)?;
                    }
                }
                () = async {
                    match devices_due {
                        Some(at) => time::sleep_until(at.into()).await,
                        None => std::future::pending().await,
                    }
                } => {
                    // `select!` picks a ready arm at random: a newer choice
                    // that waits unread replaces the kept one first, so the
                    // worker never switches to an older choice.
                    take_newer_choice(&mut devices, &mut pending_devices);
                    self.apply_devices(&mut pending_devices)?;
                }
                Ok(()) = diagnostics.changed() => {
                    let on = *diagnostics.borrow_and_update();
                    pending_diagnostics = Some((Instant::now(), on));
                    self.apply_diagnostics(&mut pending_diagnostics)?;
                }
                () = async {
                    match diagnostics_due {
                        Some(at) => time::sleep_until(at.into()).await,
                        None => std::future::pending().await,
                    }
                } => self.apply_diagnostics(&mut pending_diagnostics)?,
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
                state = screen_helper_exited(&mut self.screen_helper) => {
                    self.screen_ended_by_helper(inner, state)?;
                }
                event = self.link.events.recv() => {
                    let Some(event) = event else {
                        return Err(EndReason::WorkerStopped);
                    };
                    self.worker_event(inner, &event)?;
                }
                (host, connect) = consent_done(&mut self.consent) => {
                    self.consent_answered(inner, &host, connect)?;
                }
                Some((index, grant)) = self.grants.recv() => {
                    self.grant_received(inner, index, grant)?;
                }
                Some(()) = self.changes.recv() => {
                    self.members_changed(inner, &mut parked, &mut pending_own, &mut seen_self)
                        .await?;
                }
                Some(candidate) = self.incoming.recv() => {
                    self.key_received(inner, candidate, &mut parked)?;
                }
                Some(reason) = self.failed.recv() => return Err(reason),
                () = self.alone.due() => {
                    if self.alone_still_due(inner)? {
                        return Err(EndReason::Alone);
                    }
                }
                () = async {
                    match due {
                        Some(at) => time::sleep_until(at.into()).await,
                        None => std::future::pending().await,
                    }
                } => self.apply_own_key(&mut pending_own)?,
                () = async {
                    match retry {
                        Some(at) => time::sleep_until(at.into()).await,
                        None => std::future::pending().await,
                    }
                } => {
                    if self.send_unsent()? {
                        self.publish(inner);
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
        let has_self = all.iter().any(|m| is_own(m, &self.own_identity));
        if has_self {
            *seen_self = true;
        } else if *seen_self {
            return Err(EndReason::MembershipLost);
        }
        let current = others(all, &self.own_identity);
        let known: BTreeSet<MemberKey> = self.view.members.iter().map(member_key).collect();
        let diff = diff_members(&known, &current);
        // The worker knows members by their backend identities, which can
        // change with a membership (ADR-0056).
        let backends = backend_changes(&self.view.members, &current);
        self.view.set_members(current);
        self.prune_levels(inner);
        // The video filter follows the members at once, so a later error
        // cannot leave a departed member's picture allowed. A video stream
        // may also have started before its member was known.
        self.sync_video();
        if !diff.left.is_empty() {
            let view = &self.view;
            self.remote.retain_members(|id| view.is_member(id));
        }
        // These go only when the queue has room: a full queue must not end
        // the call, and only a closed one does. The worker ignores
        // `RemoveParticipant`, so one that does not fit is not sent again.
        for identity in backends.removed {
            // The tile keeps its track: the worker still receives it, and
            // the member filter hides the tile and drops its picture
            // until the membership returns. `ParticipantLeft` and
            // `VideoStopped` end the assignment.
            if !self.queue_worker(ToWorker::RemoveParticipant { identity })? {
                break;
            }
        }
        // A member that now has another backend identity keeps its keys
        // under the new one too.
        for (device, added) in backends.added {
            let keys = replay_keys(&self.remote.indices_of(&device), &added);
            self.mark_unsent(device, keys);
        }
        self.send_unsent()?;
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
        // A member change may bring a new SFU (ADR-0054).
        self.plan_others(inner)?;
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
                // The worker decrypts by LiveKit identity, so the key goes
                // under each backend identity of the member (ADR-0056).
                // `accept` took the key only for a current member's device,
                // by the same match as `key_member`, so the member is found.
                // Should that ever fail, or should the member have no backend
                // identity (two devices with one membership ID), the key
                // reaches no worker participant and is not shown as keyed.
                let Some(backends) = self
                    .view
                    .key_member(&identity)
                    .map(|m| m.backend_identities.clone())
                    .filter(|backends| !backends.is_empty())
                else {
                    return Ok(());
                };
                // The key goes as a waiting key does, so one rule says when
                // the member is shown as keyed: once each of its backend
                // identities has it. What does not fit goes at the retry.
                drop(bytes);
                self.mark_unsent(
                    identity,
                    backends.into_iter().map(|backend| (backend, index)),
                );
                self.send_unsent()?;
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

    /// Ends the call: the keeper, the worker, then the screen pick and the
    /// capture helpers together with (unless the session ended) the
    /// membership and the handlers in one operation. After the end of the
    /// session the worker is killed at once, and no request is sent.
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
            screen_helper,
            ..
        } = self;
        // A portal request still waiting for the user is cancelled first, so
        // its dialog does not stay open while the call is torn down. The
        // screen pick and the share's portal session end with the call.
        drop(portal);
        drop(screen_closed);
        close_session(&mut plain(&screen_control));
        // No more restarts, so the leave cannot be pushed back.
        tasks.abort_all();
        // The worker ends first: it holds the microphone, the camera and the
        // room, and a helper's bounded stop (seconds, for a helper that
        // ignores its control pipe) must not keep them after the user left.
        // Its end also closes its copy of the frame pipe.
        if session_ended {
            drop(link);
        } else {
            finish_worker(link).await;
        }
        // The screen pick and the capture helpers end before the call is
        // gone: no helper outlives the call or the session. The membership
        // ends meanwhile, so a slow helper does not hold it either.
        if session_ended {
            end_screen_helpers(screen_pick, screen_helper).await;
            return;
        }
        let room_id = view.room_id;
        let membership = access.run(|client| async move {
            end_membership(&client, &room_id, &delayed, &state_key).await;
            for handle in handles {
                client.remove_event_handler(handle);
            }
        });
        let ((), _) = tokio::join!(end_screen_helpers(screen_pick, screen_helper), membership);
    }
}

/// The keys to give under a member's new backend identities `added`: every
/// stored key index, at most [`crate::calls::keys::MAX_KEYS_PER_MEMBER`],
/// under each identity, in the order of `indices` (oldest first). Key indices wrap at 256
/// and a late key may arrive after a newer one, so neither the index nor the
/// arrival order says which key the member uses; all of them go. They wait
/// for room in the worker queue, so they cannot end the call.
fn replay_keys(indices: &[u8], added: &[String]) -> Vec<(String, u8)> {
    indices
        .iter()
        .flat_map(|index| added.iter().map(|identity| (identity.clone(), *index)))
        .collect()
}

/// What the worker must hear when the members change (ADR-0056).
#[derive(Debug, Default, PartialEq, Eq)]
struct BackendChanges {
    /// Backend identities that no current member has any more.
    removed: Vec<String>,
    /// For each device (`{user}:{device}`, as keys name it) that stayed,
    /// the backend identities it has now and did not have before.
    added: Vec<(String, Vec<String>)>,
}

/// The backend identity changes from `old` to `new`, matched by device.
fn backend_changes(old: &[ActiveMember], new: &[ActiveMember]) -> BackendChanges {
    let mut changes = BackendChanges::default();
    // An identity is removed only when no current member has it: it can
    // move from a device that left to another device of the same user.
    let current: BTreeSet<&String> = new.iter().flat_map(|m| &m.backend_identities).collect();
    for before in old {
        let device = participant_identity(&before.user_id, &before.device_id);
        let after = new
            .iter()
            .find(|m| m.user_id == before.user_id && m.device_id == before.device_id);
        let now: &[String] = after.map_or(&[], |m| &m.backend_identities);
        changes.removed.extend(
            before
                .backend_identities
                .iter()
                .filter(|id| !now.contains(id) && !current.contains(id))
                .cloned(),
        );
        let added: Vec<String> = now
            .iter()
            .filter(|id| !before.backend_identities.contains(id))
            .cloned()
            .collect();
        if !added.is_empty() {
            changes.added.push((device, added));
        }
    }
    changes
}

/// Whether `member` is this device: `own_identity` is `{user}:{device}`.
/// The device decides, not the membership ID, which another device may also
/// name (ADR-0056).
fn is_own(member: &ActiveMember, own_identity: &str) -> bool {
    participant_identity(&member.user_id, &member.device_id) == own_identity
}

/// The other devices of `members`.
fn others(members: Vec<ActiveMember>, own_identity: &str) -> Vec<ActiveMember> {
    members
        .into_iter()
        .filter(|m| !is_own(m, own_identity))
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

    /// GIVEN the homeserver refuses the call membership WHEN the refusal is
    /// mapped THEN only 403 with `M_FORBIDDEN` is `NotPermitted`: a 403
    /// with another code, other client errors and a server error stay
    /// `Membership`.
    #[tokio::test]
    async fn given_a_refused_membership_then_only_m_forbidden_is_not_permitted() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        for (status, errcode, expected) in [
            (403, "M_FORBIDDEN", CallError::NotPermitted),
            (403, "M_UNKNOWN", CallError::Membership),
            (400, "M_BAD_JSON", CallError::Membership),
            (429, "M_LIMIT_EXCEEDED", CallError::Membership),
            (500, "M_UNKNOWN", CallError::Membership),
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
                membership_error(&matrix_sdk::Error::from(error)),
                expected,
                "{status} {errcode}"
            );
        }
    }

    /// A joined, encrypted room `!r:synthetic.test` on `server`, in which
    /// `alice` has level `alice_level` and the creator 100. `state_default`
    /// and `call_level` (when given) are the room's levels for state events
    /// and for the call membership.
    async fn encrypted_room(
        server: &wiremock::MockServer,
        alice_level: i64,
        call_level: Option<i64>,
    ) -> matrix_sdk::Client {
        use matrix_sdk::config::SyncSettings;
        use wiremock::{
            Mock, ResponseTemplate,
            matchers::{method, path},
        };
        let client = crate::calls::readiness::tests::signed_in(server).await;
        let event = |kind: &str, key: &str, content: serde_json::Value| {
            serde_json::json!({
                "type": kind,
                "state_key": key,
                "sender": "@creator:synthetic.test",
                "event_id": format!("${}", kind.replace('.', "_")),
                "origin_server_ts": 1,
                "content": content,
            })
        };
        let events = match call_level {
            Some(level) => serde_json::json!({ EVENT_TYPE: level }),
            None => serde_json::json!({}),
        };
        let body = serde_json::json!({
            "next_batch": "s1",
            "rooms": { "join": { "!r:synthetic.test": {
                "state": { "events": [
                    event("m.room.create", "", serde_json::json!({
                        "creator": "@creator:synthetic.test", "room_version": "10"
                    })),
                    event("m.room.member", "@alice:synthetic.test",
                        serde_json::json!({ "membership": "join" })),
                    event("m.room.encryption", "",
                        serde_json::json!({ "algorithm": "m.megolm.v1.aes-sha2" })),
                    event("m.room.power_levels", "", serde_json::json!({
                        "users": {
                            "@creator:synthetic.test": 100,
                            "@alice:synthetic.test": alice_level,
                        },
                        "users_default": 0,
                        "state_default": 50,
                        "events": events,
                    })),
                ] },
                "timeline": { "events": [] },
            } } }
        });
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
        client.sync_once(SyncSettings::default()).await.unwrap();
        client
    }

    /// The requests the server saw after the first `seen`, as method and path.
    async fn requests_after(server: &wiremock::MockServer, seen: usize) -> Vec<String> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .skip(seen)
            .map(|request| format!("{} {}", request.method, request.url.path()))
            .collect()
    }

    /// GIVEN an encrypted room whose levels do not let this member send the
    /// call membership WHEN the member joins THEN the join ends with
    /// `NotPermitted` before any request, delayed event or worker launch,
    /// and the controller is free again.
    #[tokio::test]
    async fn given_no_permission_then_the_join_is_refused_before_any_action() {
        let server = wiremock::MockServer::start().await;
        let client = encrypted_room(&server, 0, None).await;
        let seen = server.received_requests().await.unwrap().len();
        let host = FakeHost::new(true, None);
        let calls = controller(host.clone());
        let consent = calls.confirm_direct(false).unwrap();
        let scope = JoinScope {
            generation: 0,
            runtime: Handle::current(),
            devices: Default::default(),
            video: false,
        };
        let result = calls
            .join(scope, &client, room_id!("!r:synthetic.test"), consent)
            .await;
        assert_eq!(result.err(), Some(CallError::NotPermitted));
        assert_eq!(host.starts.load(Ordering::SeqCst), 0);
        assert_eq!(requests_after(&server, seen).await, Vec::<String>::new());
        assert!(matches!(calls.state(), CallState::Idle));
        assert!(calls.claim(room_id!("!r:synthetic.test")).is_ok());
    }

    /// GIVEN the same room WHEN a moderator's level, or an event level of 0
    /// for the call membership, allows it THEN the join passes the
    /// permission check and goes on to the call server (here none: the
    /// transports request is the first request, and it fails).
    #[tokio::test]
    async fn given_permission_then_the_join_goes_past_the_check() {
        for (alice, call_level) in [(50, None), (0, Some(0))] {
            let server = wiremock::MockServer::start().await;
            let client = encrypted_room(&server, alice, call_level).await;
            let seen = server.received_requests().await.unwrap().len();
            let calls = controller(FakeHost::new(true, None));
            let consent = calls.confirm_direct(false).unwrap();
            let scope = JoinScope {
                generation: 0,
                runtime: Handle::current(),
                devices: Default::default(),
                video: false,
            };
            let result = calls
                .join(scope, &client, room_id!("!r:synthetic.test"), consent)
                .await;
            assert_eq!(
                result.err(),
                Some(CallError::NoSfu),
                "{alice} {call_level:?}"
            );
            assert!(!requests_after(&server, seen).await.is_empty());
        }
    }

    fn member(user: &str, device: &str, sfu: &str) -> ActiveMember {
        let user_id = OwnedUserId::try_from(user).unwrap();
        let device_id = OwnedDeviceId::from(device);
        let service_url = Url::parse(sfu).unwrap();
        let identity = participant_identity(&user_id, &device_id);
        ActiveMember {
            backend_identities: vec![identity.clone()],
            identity,
            user_id,
            device_id,
            transports: vec![service_url.clone()],
            service_url,
            joined_ts: 1,
            expires_ts: u64::MAX,
        }
    }

    /// GIVEN this device with a custom membership ID and another device that
    /// names this device's identity as its membership ID WHEN the other
    /// members are taken THEN this device goes by its device and the other
    /// one stays (ADR-0056).
    #[test]
    fn given_shared_identity_strings_then_only_this_device_is_left_out() {
        let mut own = member("@a:example.org", "A", "https://sfu.example.org");
        own.identity = "custom".to_owned();
        let mut copy = member("@b:example.org", "B", "https://sfu.example.org");
        copy.identity = "@a:example.org:A".to_owned();
        let rest = others(vec![own.clone(), copy.clone()], "@a:example.org:A");
        assert_eq!(rest, [copy]);
        assert!(is_own(&own, "@a:example.org:A"));
    }

    /// The membership of a device whose membership ID is `{user}:{device}`.
    fn key(id: &str) -> MemberKey {
        (id.to_owned(), id.to_owned())
    }

    fn known(ids: &[&str]) -> BTreeSet<MemberKey> {
        ids.iter().map(|id| key(id)).collect()
    }

    /// GIVEN known members and a new list WHEN diffed THEN joined and left
    /// are named by membership.
    #[test]
    fn given_a_changed_member_list_then_the_diff_names_joined_and_left() {
        let current = [
            member("@a:example.org", "A", "https://sfu.example.org"),
            member("@c:example.org", "C", "https://sfu.example.org"),
        ];
        let diff = diff_members(&known(&["@a:example.org:A", "@b:example.org:B"]), &current);
        assert_eq!(diff.joined, [key("@c:example.org:C")]);
        assert_eq!(diff.left, [key("@b:example.org:B")]);
        assert_eq!(
            diff_members(&known(&["@a:example.org:A"]), &current[..1]),
            MemberDiff::default()
        );
    }

    /// GIVEN two devices of one user with one membership ID WHEN the second
    /// joins THEN it is a join and only it gets the young key; WHEN the first
    /// leaves THEN it is a leave, so the key rotates; WHEN a device names a
    /// new membership ID THEN it is a leave and a join (ADR-0056).
    #[test]
    fn given_a_shared_membership_id_then_each_device_joins_and_leaves() {
        let shared = |device: &str| {
            let mut m = member("@b:example.org", device, "https://sfu.example.org");
            m.identity = "shared".to_owned();
            m
        };
        let b1 = ("@b:example.org:B1".to_owned(), "shared".to_owned());
        let b2 = ("@b:example.org:B2".to_owned(), "shared".to_owned());
        let both = [shared("B1"), shared("B2")];

        let diff = diff_members(&BTreeSet::from([b1.clone()]), &both);
        assert_eq!(
            diff,
            MemberDiff {
                joined: vec![b2.clone()],
                left: vec![]
            }
        );
        let now = Instant::now();
        let mut own = OwnKeys::new(now).unwrap();
        let outgoing = key_action(&diff, &mut own, now).unwrap().unwrap();
        assert_eq!(outgoing.recipients, Recipients::NewMembersOnly);
        assert_eq!(
            recipients(&outgoing, &both, &diff.joined),
            [(owned_user_id!("@b:example.org"), owned_device_id!("B2"))]
        );

        let diff = diff_members(&BTreeSet::from([b1.clone(), b2.clone()]), &both[1..]);
        assert_eq!(
            diff,
            MemberDiff {
                joined: vec![],
                left: vec![b1]
            }
        );

        let mut renamed = shared("B2");
        renamed.identity = "new".to_owned();
        let diff = diff_members(&BTreeSet::from([b2.clone()]), &[renamed]);
        assert_eq!(
            diff.joined,
            [("@b:example.org:B2".to_owned(), "new".to_owned())]
        );
        assert_eq!(diff.left, [b2]);
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
            joined: vec![key("@b:example.org:B")],
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
            joined: vec![key("@b:example.org:B")],
            left: vec![key("@x:example.org:X")],
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
            joined: vec![key("@b:example.org:B")],
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

    /// GIVEN each device change answer WHEN it is applied THEN an undone
    /// switch is `kept` (the call has its devices), apart from a `failed`
    /// change that may have stopped the sound; a change that worked clears it.
    #[test]
    fn given_device_change_answers_then_an_undone_switch_is_kept() {
        let mut view = view();
        for (code, change) in [
            (FailureCode::DeviceKept, CallDeviceChange::Kept),
            (FailureCode::AudioDevice, CallDeviceChange::Failed),
            (FailureCode::DeviceNotFound, CallDeviceChange::NotFound),
            (FailureCode::Unsupported, CallDeviceChange::Unsupported),
        ] {
            let answer = FromWorker::DevicesSet {
                failure: Some(code),
            };
            assert_eq!(view.apply(&answer), Applied::Changed);
            assert_eq!(view.device_change, Some(change));
        }
        view.apply(&FromWorker::DevicesSet { failure: None });
        assert_eq!(view.device_change, None);
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
            connection: 0,
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

    /// GIVEN stored keys whose indices wrapped, and a late older key WHEN
    /// they are given under new backend identities THEN every key goes under
    /// each identity, in stored order, so the key in use is never left out.
    #[test]
    fn given_stored_keys_then_all_are_replayed_under_each_new_identity() {
        let added = ["h".to_owned(), "d".to_owned()];
        assert!(replay_keys(&[], &added).is_empty());
        // 255 then 0 is a wrap; 254 arrived late.
        assert_eq!(
            replay_keys(&[255, 0, 254], &added),
            [255, 255, 0, 0, 254, 254]
                .iter()
                .zip(added.iter().cycle())
                .map(|(index, id)| (id.clone(), *index))
                .collect::<Vec<_>>()
        );
    }

    /// GIVEN members that leave, stay, or change their backend identities
    /// WHEN the change is read THEN each identity no member has is removed,
    /// and each staying device names only its new identities (ADR-0056).
    #[test]
    fn given_member_changes_then_backend_changes_follow_the_devices() {
        let with = |user: &str, device: &str, ids: &[&str]| {
            let mut m = member(user, device, "https://sfu.example.org");
            m.backend_identities = ids.iter().map(|id| (*id).to_owned()).collect();
            m
        };
        let old = [
            with("@a:example.org", "A", &["@a:example.org:A"]),
            with("@b:example.org", "B", &["hash-b", "@b:example.org:B"]),
            with("@c:example.org", "C", &["@c:example.org:C"]),
        ];
        let mut a_changed = with("@a:example.org", "A", &["hash-a"]);
        // A new membership ID is a leave and a join, but the device stays.
        a_changed.identity = "new-id".to_owned();
        let new = [
            a_changed,
            with("@b:example.org", "B", &["hash-b", "@b:example.org:B"]),
            with("@d:example.org", "D", &["@d:example.org:D"]),
        ];
        assert_eq!(
            backend_changes(&old, &new),
            BackendChanges {
                removed: vec!["@a:example.org:A".to_owned(), "@c:example.org:C".to_owned()],
                added: vec![("@a:example.org:A".to_owned(), vec!["hash-a".to_owned()])],
            }
        );
        assert_eq!(backend_changes(&new, &new), BackendChanges::default());

        // An identity that moves to another device that a current member has
        // is not removed.
        let moved_from = [with("@e:example.org", "E1", &["hash-e"])];
        let moved_to = [with("@e:example.org", "E2", &["hash-e"])];
        assert_eq!(
            backend_changes(&moved_from, &moved_to),
            BackendChanges::default()
        );
    }

    /// GIVEN a member whose membership ID is not `{user}:{device}` WHEN a key
    /// names its device THEN the key finds the member, and a key naming the
    /// membership ID does not (ADR-0056).
    #[test]
    fn given_a_custom_membership_id_then_keys_find_the_member_by_device() {
        let mut view = view();
        let mut a = member("@a:example.org", "A", "https://sfu.example.org");
        a.identity = "custom-id".to_owned();
        view.set_members(vec![a]);
        assert!(view.is_member("@a:example.org:A"));
        assert!(!view.is_member("custom-id"));
        view.keyed.insert("@a:example.org:A".to_owned());
        view.apply(&FromWorker::Connected);
        let CallState::Active { participants, .. } = view.state() else {
            panic!("expected an active call");
        };
        assert!(participants[0].keyed);
    }

    /// GIVEN a member on an SFU whose homeserver issues tokens, known to
    /// LiveKit by its hashed identity WHEN the worker reports that identity
    /// or the plain membership ID THEN only the hashed identity stands for
    /// the member (ADR-0056).
    #[test]
    fn given_a_hashed_backend_identity_then_only_it_stands_for_the_member() {
        let sfu = "wss://sfu.example.org:7443";
        let mut view = View::new(
            room_id!("!r:example.org").to_owned(),
            Url::parse(sfu).unwrap(),
            false,
        );
        let mut a = member("@a:example.org", "A", sfu);
        let hashed = hashed_identity(&a.user_id, &a.identity);
        a.backend_identities = vec![hashed.clone()];
        view.set_members(vec![a]);
        view.apply(&FromWorker::Connected);
        let speak = |id: &str| FromWorker::Speaking {
            connection: 0,
            identity: id.to_owned(),
            speaking: true,
        };
        assert_eq!(view.apply(&speak("@a:example.org:A")), Applied::Same);
        assert_eq!(view.apply(&speak(&hashed)), Applied::Changed);
        let CallState::Active { participants, .. } = view.state() else {
            panic!("expected an active call");
        };
        assert!(participants[0].speaking);
        assert!(participants[0].reachable);
        assert_eq!(
            view.apply(&FromWorker::VideoStarted {
                tile: 1,
                connection: 0,
                identity: hashed,
            }),
            Applied::Changed
        );
        assert_eq!(view.video_tiles().len(), 1);
        view.set_members(Vec::new());
        assert!(view.speaking.is_empty());
        assert!(view.video_tiles().is_empty());
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
        /// The answer to the question about another SFU.
        other_answer: AtomicBool,
        /// Each question about another SFU: the host and the proxy flag.
        asked: Mutex<Vec<(String, bool)>>,
        /// The sound server names of the last start.
        audio: Mutex<Option<WorkerAudio>>,
    }

    impl FakeHost {
        fn new(confirm: bool, start_error: Option<StartError>) -> Arc<Self> {
            Arc::new(Self {
                confirm,
                start_error,
                confirms: AtomicUsize::new(0),
                starts: AtomicUsize::new(0),
                other_answer: AtomicBool::new(false),
                asked: Mutex::new(Vec::new()),
                audio: Mutex::new(None),
            })
        }

        fn answering_other(answer: bool) -> Arc<Self> {
            let host = Self::new(true, None);
            host.other_answer.store(answer, Ordering::SeqCst);
            host
        }

        fn asked(&self) -> Vec<(String, bool)> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl CallHost for FakeHost {
        fn start(&self, audio: &WorkerAudio) -> Result<CallWorkerLink, StartError> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            *self.audio.lock().unwrap() = Some(audio.clone());
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

        fn confirm_other_sfu(&self, host: &str, proxy_configured: bool) -> bool {
            self.asked
                .lock()
                .unwrap()
                .push((host.to_owned(), proxy_configured));
            self.other_answer.load(Ordering::SeqCst)
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
            start_worker(&host, WorkerAudio::default()).await,
            Err(CallError::Worker(StartError::Containment(4)))
        ));
        let host: Arc<dyn CallHost> = FakeHost::new(true, None);
        assert!(start_worker(&host, WorkerAudio::default()).await.is_ok());
    }

    /// GIVEN a saved microphone and speaker by sound server name WHEN the
    /// worker starts THEN the host gets both names for its launch.
    #[tokio::test]
    async fn given_sound_server_choices_then_the_launch_gets_their_names() {
        let fake = FakeHost::new(true, None);
        let host: Arc<dyn CallHost> = fake.clone();
        let audio = WorkerAudio::for_choice(Some("pulse:vmic"), Some("pulse:vsink"));
        assert!(start_worker(&host, audio.clone()).await.is_ok());
        assert_eq!(*fake.audio.lock().unwrap(), Some(audio));
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
        assert_eq!(controller.set_deafened(true), Err(CallError::NotInCall));
        assert_eq!(controller.set_volume(50), Err(CallError::NotInCall));
    }

    fn mic(muted: bool) -> ToWorker {
        ToWorker::SetMuted { muted }
    }

    fn ears(deafened: bool) -> ToWorker {
        ToWorker::SetDeafened { deafened }
    }

    /// GIVEN an unmuted call WHEN it is deafened and undeafened THEN the
    /// microphone mutes before remote audio stops, and remote audio comes
    /// back before the microphone opens again.
    #[test]
    fn given_an_unmuted_call_when_deafened_and_undeafened_then_the_mic_follows() {
        let mut audio = AudioWants::default();
        assert_eq!(audio.set_deafened(true), vec![mic(true), ears(true)]);
        assert!(audio.microphone_muted());
        assert_eq!(audio.set_deafened(false), vec![ears(false), mic(false)]);
        assert!(!audio.microphone_muted());
    }

    /// GIVEN a muted call WHEN it is deafened and undeafened THEN the
    /// microphone stays muted: undeafening restores the user's own mute.
    #[test]
    fn given_a_muted_call_when_undeafened_then_the_mic_stays_muted() {
        let mut audio = AudioWants::default();
        assert_eq!(audio.set_muted(true), vec![mic(true)]);
        assert_eq!(audio.set_deafened(true), vec![mic(true), ears(true)]);
        assert_eq!(audio.set_deafened(false), vec![ears(false), mic(true)]);
    }

    /// GIVEN a deafened call WHEN the user unmutes THEN remote audio comes
    /// back first and then the microphone opens; WHEN the user mutes while
    /// deafened THEN it stays deafened and an undeafen keeps the mute.
    #[test]
    fn given_a_deafened_call_when_unmuted_then_it_undeafens() {
        let mut audio = AudioWants::default();
        audio.set_deafened(true);
        assert_eq!(audio.set_muted(false), vec![ears(false), mic(false)]);
        assert_eq!(audio, AudioWants::default());

        audio.set_deafened(true);
        assert_eq!(audio.set_muted(true), vec![mic(true)]);
        assert!(audio.deafened);
        assert_eq!(audio.set_deafened(false), vec![ears(false), mic(true)]);
    }

    /// GIVEN a view WHEN the worker reports deafened, or the volume moves
    /// THEN the page state shows it, and a repeat or out-of-range value
    /// changes nothing more.
    #[test]
    fn given_deafen_and_volume_changes_then_the_view_shows_them() {
        let mut view = view();
        view.connected = true;
        assert_eq!(
            view.apply(&FromWorker::Deafened { deafened: true }),
            Applied::Changed
        );
        assert_eq!(
            view.apply(&FromWorker::Deafened { deafened: true }),
            Applied::Same
        );
        assert_eq!(view.set_volume(40), Applied::Changed);
        assert_eq!(view.set_volume(40), Applied::Same);
        assert_eq!(view.set_volume(250), Applied::Changed);
        let CallState::Active {
            deafened, volume, ..
        } = view.state()
        else {
            panic!("active");
        };
        assert!(deafened);
        assert_eq!(volume, MAX_VOLUME_PERCENT);
        let saved = View::new(
            room_id!("!r:example.org").to_owned(),
            Url::parse("https://sfu.example.org").unwrap(),
            false,
        )
        .with_volume(25);
        assert_eq!(saved.volume, 25);
    }

    /// GIVEN a running call task WHEN the volume changes THEN the worker gets
    /// it and the page state shows it at once; the same volume again is sent
    /// but publishes no change.
    #[test]
    fn given_a_running_call_when_the_volume_changes_then_the_worker_gets_it() {
        let fixture = test_call(
            no_admission(),
            Arc::new(Flags::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let inner = controller(FakeHost::new(true, None)).inner.clone();
        let (mut call, mut to_worker) = (fixture.call, fixture.to_worker);
        call.view.connected = true;
        call.volume_changed(&inner, 30).unwrap();
        assert_eq!(
            to_worker.try_recv().unwrap(),
            ToWorker::SetVolume { percent: 30 }
        );
        let CallState::Active { volume, .. } = inner.state.borrow().clone() else {
            panic!("active");
        };
        assert_eq!(volume, 30);
        call.volume_changed(&inner, 30).unwrap();
        assert_eq!(
            to_worker.try_recv().unwrap(),
            ToWorker::SetVolume { percent: 30 }
        );
        call.volume_changed(&inner, 200).unwrap();
        assert_eq!(
            to_worker.try_recv().unwrap(),
            ToWorker::SetVolume {
                percent: MAX_VOLUME_PERCENT
            }
        );
    }

    /// GIVEN a running call WHEN deafen and volume are asked for THEN the
    /// deafen is queued for the task, and the volume is kept as the newest
    /// value even while no task watches it.
    #[test]
    fn given_a_running_call_when_deafen_and_volume_are_set_then_the_task_gets_them() {
        let calls = controller(FakeHost::new(true, None));
        let (_store, mut commands, _killed) = running_call(&calls, active_state());
        assert_eq!(calls.set_deafened(true), Ok(()));
        assert_eq!(commands.try_recv().unwrap(), Command::Deafen(true));
        assert_eq!(calls.set_volume(10), Ok(()));
        assert_eq!(calls.set_volume(70), Ok(()));
        let Slot::Running(run) = &*calls.inner.slot() else {
            panic!("running");
        };
        let watched = run.volume.subscribe();
        assert_eq!(*watched.borrow(), 70, "only the newest value is kept");
    }

    /// GIVEN a running call task WHEN the microphone level and gain change
    /// THEN the worker gets them as one message, and a gain that is off the
    /// range or the step never reaches the wire.
    #[test]
    fn given_a_running_call_when_the_microphone_changes_then_the_worker_gets_it() {
        let fixture = test_call(
            no_admission(),
            Arc::new(Flags::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let (mut call, mut to_worker) = (fixture.call, fixture.to_worker);
        let settings = |auto_level, gain| MicSettings { auto_level, gain };
        call.microphone_changed(settings(false, 250)).unwrap();
        assert_eq!(
            to_worker.try_recv().unwrap(),
            ToWorker::SetMicrophone {
                auto_level: false,
                mic_gain: 250
            }
        );
        call.microphone_changed(settings(true, 0)).unwrap();
        assert_eq!(
            to_worker.try_recv().unwrap(),
            ToWorker::SetMicrophone {
                auto_level: true,
                mic_gain: 0
            }
        );
        for gain in [1, 99, 401, 405, u16::MAX] {
            call.microphone_changed(settings(true, gain)).unwrap();
            assert!(to_worker.try_recv().is_err(), "{gain} was sent");
        }
    }

    /// GIVEN no call and a running call WHEN the microphone settings are set
    /// THEN no call is `NotInCall`, a gain off the range or the step is
    /// `InvalidValue` and changes nothing, and a running call keeps only the
    /// newest valid value.
    #[test]
    fn given_the_microphone_settings_when_set_then_only_valid_ones_are_kept() {
        let calls = controller(FakeHost::new(true, None));
        assert_eq!(calls.set_microphone(true, 100), Err(CallError::NotInCall));
        assert_eq!(
            calls.set_microphone(true, 405),
            Err(CallError::InvalidValue)
        );
        let (_store, _commands, _killed) = running_call(&calls, active_state());
        assert_eq!(calls.set_microphone(false, 150), Ok(()));
        assert_eq!(calls.set_microphone(true, 250), Ok(()));
        for gain in [1, 401, 405, u16::MAX] {
            assert_eq!(
                calls.set_microphone(false, gain),
                Err(CallError::InvalidValue),
                "{gain}"
            );
        }
        let Slot::Running(run) = &*calls.inner.slot() else {
            panic!("running");
        };
        let watched = run.microphone.subscribe();
        assert_eq!(
            *watched.borrow(),
            Some(MicSettings {
                auto_level: true,
                gain: 250
            }),
            "only the newest valid value is kept"
        );
    }

    /// GIVEN a call that is still joining WHEN the microphone settings are
    /// set THEN they wait on the join's channel and a call task that
    /// subscribes later still gets the newest valid value; a refused gain
    /// changes nothing.
    #[test]
    fn given_a_joining_call_when_the_microphone_changes_then_the_task_gets_it() {
        let calls = controller(FakeHost::new(true, None));
        let guard = calls.claim(room_id!("!r:example.org")).unwrap();
        assert_eq!(
            *guard.microphone.borrow(),
            None,
            "the join sends the saved one"
        );
        assert_eq!(calls.set_microphone(false, 150), Ok(()));
        assert_eq!(
            calls.set_microphone(true, 405),
            Err(CallError::InvalidValue)
        );
        // The call task subscribes after the worker connects, as `run` does.
        let mut task = guard.microphone.subscribe();
        task.mark_changed();
        assert!(task.has_changed().unwrap(), "an earlier value is sent");
        assert_eq!(
            *task.borrow_and_update(),
            Some(MicSettings {
                auto_level: false,
                gain: 150
            })
        );
    }

    /// GIVEN a call that is still joining WHEN the microphone and speaker
    /// change THEN the choice waits on the join's channel, and a task that
    /// subscribes later still sees it; WHEN no call runs THEN it is
    /// `NotInCall` and applies to the next call.
    #[test]
    fn given_a_joining_call_when_devices_change_then_the_task_gets_the_choice() {
        let calls = controller(FakeHost::new(true, None));
        assert_eq!(
            calls.set_devices(Some("mic-1"), None, None),
            Err(CallError::NotInCall)
        );
        let guard = calls.claim(room_id!("!r:example.org")).unwrap();
        assert_eq!(
            calls.set_devices(Some("mic-1"), Some("spk-1"), Some("cam-1")),
            Ok(())
        );
        // The call task subscribes after the worker connects, as `run` does.
        let mut task = guard.devices.subscribe();
        task.mark_changed();
        assert!(task.has_changed().unwrap(), "an earlier choice is sent");
        assert_eq!(
            *task.borrow_and_update(),
            Some(DeviceChoice {
                recording: Some("mic-1".to_owned()),
                playout: Some("spk-1".to_owned()),
                camera: Some("cam-1".to_owned()),
            })
        );
    }

    /// GIVEN a running call WHEN the same microphone and speaker are chosen
    /// again THEN nothing is sent, so saving only the camera keeps the sound;
    /// WHEN the last change failed THEN the same choice is sent again, so a
    /// device plugged back in reaches the worker.
    #[test]
    fn given_a_failed_device_change_then_the_same_choice_is_sent_again() {
        let calls = controller(FakeHost::new(true, None));
        let (_store, _commands, _killed) = running_call(&calls, active_state());
        let mut task = match &*calls.inner.slot() {
            Slot::Running(run) => run.devices.subscribe(),
            _ => unreachable!("the call runs"),
        };
        let choose = || calls.set_devices(Some("mic-1"), Some("spk-1"), None);
        assert_eq!(choose(), Ok(()));
        assert!(task.has_changed().unwrap());
        task.borrow_and_update();
        assert_eq!(choose(), Ok(()));
        assert!(!task.has_changed().unwrap(), "the same choice is not sent");

        let mut failed = active_state();
        if let CallState::Active { device_change, .. } = &mut failed {
            *device_change = Some(CallDeviceChange::NotFound);
        }
        calls.inner.publish(failed);
        assert_eq!(choose(), Ok(()));
        assert!(
            task.has_changed().unwrap(),
            "after a failure it is sent again"
        );
    }

    /// GIVEN a call that runs WHEN a device test is asked for THEN the reply
    /// is `in_call`, and the test's status stays as it was, so the refusal
    /// does not stay after the call ends.
    #[test]
    fn given_a_running_call_when_a_device_test_is_asked_then_the_refusal_is_not_kept() {
        let calls = controller(FakeHost::new(true, None));
        let (_store, _commands, _killed) = running_call(&calls, active_state());
        let reply = calls.start_device_test(CallDeviceTestKind::Microphone, None);
        assert_eq!(reply.state, CallDeviceTestState::InCall);
        assert_eq!(reply.test, Some(CallDeviceTestKind::Microphone));
        let kept = calls.device_test_status();
        assert_eq!(kept.state, CallDeviceTestState::Idle);
        assert_eq!(kept.test, None);
    }

    /// GIVEN a device test thread that still waits for the test before it
    /// WHEN more tests are asked for THEN no thread starts: the job of the
    /// waiting thread becomes the newest test, so at most two test threads
    /// exist, and the stop of the older job is set.
    #[test]
    fn given_a_waiting_test_thread_when_tests_are_asked_then_no_thread_starts() {
        let calls = controller(FakeHost::new(true, None));
        let first_stop = Arc::new(AtomicBool::new(false));
        {
            let mut slot = plain(&calls.inner.device_test);
            slot.generation = 1;
            slot.stop = first_stop.clone();
            slot.job = Some(DeviceTestJob {
                generation: 1,
                test: CallDeviceTestKind::Microphone,
                device: None,
                stop: first_stop.clone(),
            });
        }
        for _ in 0..3 {
            let reply =
                calls.start_device_test(CallDeviceTestKind::Speaker, Some("spk".to_owned()));
            assert_eq!(reply.state, CallDeviceTestState::Starting);
        }
        assert!(first_stop.load(Ordering::SeqCst), "the older job stops");
        let slot = plain(&calls.inner.device_test);
        assert!(slot.thread.is_none(), "no thread started");
        let job = slot.job.as_ref().expect("the newest job waits");
        assert_eq!(job.generation, 4);
        assert_eq!(job.generation, slot.generation);
        assert_eq!(slot.status.id, slot.generation, "the status names its test");
        assert_eq!(job.test, CallDeviceTestKind::Speaker);
        assert_eq!(job.device.as_deref(), Some("spk"));
    }

    /// A host whose worker answers with `events`, in order, and keeps what
    /// it is sent.
    struct ScriptHost {
        events: Mutex<Vec<FromWorker>>,
        sent: Mutex<Option<mpsc::Receiver<ToWorker>>>,
        /// The worker's side, kept open so a test can answer later.
        answers: Mutex<Option<mpsc::Sender<FromWorker>>>,
        /// Set during the start, as a stop that comes while it waits.
        stop_on_start: Mutex<Option<Arc<AtomicBool>>>,
    }

    impl ScriptHost {
        fn new(events: Vec<FromWorker>) -> Arc<Self> {
            Arc::new(Self {
                events: Mutex::new(events),
                sent: Mutex::new(None),
                answers: Mutex::new(None),
                stop_on_start: Mutex::new(None),
            })
        }
    }

    impl CallHost for ScriptHost {
        fn start(&self, _: &WorkerAudio) -> Result<CallWorkerLink, StartError> {
            let (commands, received) = mpsc::channel(8);
            let (answers, events) = mpsc::channel(8);
            for event in self.events.lock().unwrap().drain(..) {
                answers.try_send(event).unwrap();
            }
            *self.sent.lock().unwrap() = Some(received);
            *self.answers.lock().unwrap() = Some(answers);
            if let Some(stop) = self.stop_on_start.lock().unwrap().as_ref() {
                stop.store(true, Ordering::SeqCst);
            }
            Ok(CallWorkerLink::from_parts(
                commands,
                events,
                Box::new(FakeProcess(Arc::new(AtomicBool::new(false)))),
            ))
        }

        fn confirm_direct_connection(&self) -> bool {
            true
        }
    }

    /// GIVEN a microphone test WHEN its worker starts THEN it gets the
    /// diagnostic log's switch before `TestDevice` (ADR-0058); WHEN the
    /// worker sends a diagnostic record during the test THEN the record
    /// goes to the log and the test still ends as done.
    #[test]
    fn given_a_device_test_then_its_worker_gets_the_diagnostic_switch_first() {
        let record = FromWorker::Diagnostic {
            level: 2,
            target: "call_worker".into(),
            message: "no device".into(),
        };
        let host = ScriptHost::new(vec![FromWorker::Connected, record, FromWorker::Left]);
        let calls = controller(host.clone());
        plain(&calls.inner.device_test).generation = 1;
        let mic = Some("mic".to_owned());
        run_device_test(
            &calls.inner,
            1,
            CallDeviceTestKind::Microphone,
            mic,
            &AtomicBool::new(false),
            device_test::TEST_LIMIT,
        );
        assert_eq!(calls.device_test_status().state, CallDeviceTestState::Done);
        let mut sent = host
            .sent
            .lock()
            .unwrap()
            .take()
            .expect("the worker started");
        // Other tests turn the global log on and off, so only the order is
        // checked here.
        assert!(matches!(
            sent.try_recv(),
            Ok(ToWorker::SetDiagnostics { .. })
        ));
        assert!(matches!(
            sent.try_recv(),
            Ok(ToWorker::TestDevice { device: Some(id), .. }) if id == "mic"
        ));
    }

    /// GIVEN a test that never ran (a camera open that hangs) WHEN the
    /// core's limit ends it THEN it is failed, so the page says why; WHEN
    /// the user stops it THEN it is idle.
    #[test]
    fn given_a_test_ended_by_the_limit_then_it_failed_and_a_user_stop_is_idle() {
        // The limit passes at once: the worker answers `Leave` with `Left`.
        let host = ScriptHost::new(vec![FromWorker::Left]);
        let calls = controller(host.clone());
        plain(&calls.inner.device_test).generation = 1;
        let stop = AtomicBool::new(false);
        run_device_test(
            &calls.inner,
            1,
            CallDeviceTestKind::Camera,
            None,
            &stop,
            Duration::ZERO,
        );
        assert_eq!(
            calls.device_test_status().state,
            CallDeviceTestState::Failed
        );

        // The user stops it after the start; the worker answers the core's
        // `Leave` with `Left`.
        let host = ScriptHost::new(Vec::new());
        let calls = controller(host.clone());
        plain(&calls.inner.device_test).generation = 1;
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut seen_leave = false;
                while !seen_leave {
                    if let Some(sent) = host.sent.lock().unwrap().as_mut() {
                        stop.store(true, Ordering::SeqCst);
                        while let Ok(message) = sent.try_recv() {
                            seen_leave |= matches!(message, ToWorker::Leave);
                        }
                    }
                    std::thread::yield_now();
                }
                let answers = host.answers.lock().unwrap().clone().unwrap();
                answers.try_send(FromWorker::Left).unwrap();
            });
            let limit = device_test::TEST_LIMIT;
            run_device_test(
                &calls.inner,
                1,
                CallDeviceTestKind::Camera,
                None,
                &stop,
                limit,
            );
        });
        assert_eq!(calls.device_test_status().state, CallDeviceTestState::Idle);
    }

    /// GIVEN a stop recorded while the test worker starts WHEN the worker is
    /// ready THEN no `TestDevice` is sent, so no device opens, and the test
    /// ends as idle.
    #[test]
    fn given_a_stop_during_the_worker_start_then_no_device_opens() {
        let host = ScriptHost::new(Vec::new());
        let stop = Arc::new(AtomicBool::new(false));
        *host.stop_on_start.lock().unwrap() = Some(stop.clone());
        let calls = controller(host.clone());
        plain(&calls.inner.device_test).generation = 1;
        let limit = device_test::TEST_LIMIT;
        run_device_test(
            &calls.inner,
            1,
            CallDeviceTestKind::Microphone,
            None,
            &stop,
            limit,
        );
        assert_eq!(calls.device_test_status().state, CallDeviceTestState::Idle);
        let mut sent = host
            .sent
            .lock()
            .unwrap()
            .take()
            .expect("the worker started");
        while let Ok(message) = sent.try_recv() {
            assert!(
                !matches!(message, ToWorker::TestDevice { .. }),
                "no device test is sent"
            );
        }
    }

    /// GIVEN test 2 is the newest WHEN a stop names test 1 THEN test 2 runs
    /// on; WHEN a stop names test 2, or no test, THEN it stops.
    #[test]
    fn given_a_stop_for_an_older_test_then_the_newest_runs_on() {
        let calls = controller(FakeHost::new(true, None));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let mut slot = plain(&calls.inner.device_test);
            slot.generation = 2;
            slot.stop = stop.clone();
        }
        calls.stop_device_test_if(Some(1));
        assert!(
            !stop.load(Ordering::SeqCst),
            "an older test's stop is ignored"
        );
        calls.stop_device_test_if(Some(2));
        assert!(stop.load(Ordering::SeqCst));
        stop.store(false, Ordering::SeqCst);
        calls.stop_device_test();
        assert!(stop.load(Ordering::SeqCst), "no ID stops any test");
    }

    /// GIVEN a test thread that still runs WHEN the next test thread cannot
    /// start THEN the running thread is kept as `previous` and the test is
    /// `failed`; WHEN a later thread starts THEN it takes that thread and
    /// waits for it, so no test thread is left unwaited.
    #[test]
    fn given_a_failed_test_thread_start_then_the_running_thread_is_still_waited_for() {
        let calls = controller(FakeHost::new(true, None));
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let running = std::thread::spawn(move || {
            let _ = wait.recv();
        });
        let mut slot = plain(&calls.inner.device_test);
        slot.thread = Some(running);
        slot.status.state = CallDeviceTestState::Starting;
        start_test_thread(&mut slot, &calls.inner, |_| {
            Err(std::io::Error::other("no thread"))
        });
        assert!(slot.thread.is_none());
        assert_eq!(slot.previous.len(), 1, "the running thread is kept");
        assert_eq!(slot.status.state, CallDeviceTestState::Failed);

        slot.status.state = CallDeviceTestState::Starting;
        start_test_thread(&mut slot, &calls.inner, spawn_device_test);
        let next = slot.thread.take().expect("the next thread starts");
        drop(slot);
        release.send(()).unwrap();
        next.join().unwrap();
        assert!(
            plain(&calls.inner.device_test).previous.is_empty(),
            "the next thread took the running one and waited for it"
        );
    }

    /// GIVEN a kept thread A and a newest thread B that both still run
    /// WHEN another test thread starts THEN neither handle is dropped: the
    /// new thread waits for both, so neither is left unwaited, and an ended
    /// kept thread is pruned.
    #[test]
    fn given_two_running_test_threads_when_another_starts_then_it_waits_for_both() {
        let calls = controller(FakeHost::new(true, None));
        let (done_tx, done) = std::sync::mpsc::channel::<&str>();
        let mut releases = Vec::new();
        let mut running = |name: &'static str| {
            let (release, wait) = std::sync::mpsc::channel::<()>();
            releases.push(release);
            let done_tx = done_tx.clone();
            std::thread::spawn(move || {
                let _ = wait.recv();
                // Late, so a thread that did not join it ends first.
                std::thread::sleep(Duration::from_millis(50));
                let _ = done_tx.send(name);
            })
        };
        let ended = std::thread::spawn(|| {});
        while !ended.is_finished() {
            std::thread::yield_now();
        }
        let a = running("a");
        let b = running("b");
        let mut slot = plain(&calls.inner.device_test);
        slot.previous = vec![ended, a];
        slot.thread = Some(b);
        start_test_thread(&mut slot, &calls.inner, spawn_device_test);
        assert_eq!(
            slot.previous.len(),
            2,
            "A and B are kept; the ended one goes"
        );
        let next = slot.thread.take().expect("the next thread starts");
        drop(slot);
        for release in releases {
            release.send(()).unwrap();
        }
        next.join().unwrap();
        let mut finished: Vec<_> = done.try_iter().collect();
        finished.sort_unstable();
        assert_eq!(finished, ["a", "b"], "the next thread ended after both");
    }

    /// GIVEN a running call whose command queue is full
    /// WHEN mute or deafen is asked for
    /// THEN each is refused as `Busy`, not taken as if there were no call;
    /// once the task reads a command there is room again, and a call task
    /// that has ended is `NotInCall`.
    #[test]
    fn given_a_full_command_queue_when_mute_or_deafen_is_set_then_it_is_busy() {
        let calls = controller(FakeHost::new(true, None));
        let (_store, mut commands, _killed) = running_call(&calls, active_state());
        while calls.set_deafened(true).is_ok() {}
        assert_eq!(calls.set_deafened(false), Err(CallError::Busy));
        assert_eq!(calls.set_muted(true), Err(CallError::Busy));
        assert_eq!(commands.try_recv().unwrap(), Command::Deafen(true));
        assert_eq!(calls.set_muted(true), Ok(()));
        commands.close();
        while commands.try_recv().is_ok() {}
        drop(commands);
        assert_eq!(calls.set_deafened(true), Err(CallError::NotInCall));
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
            volume: watch::channel(MAX_VOLUME_PERCENT).0,
            microphone: watch::channel(None).0,
            devices: watch::channel(None).0,
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
        let (grants_tx, grants) = mpsc::channel(MAX_OTHER_SFUS);
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
            unsent: BTreeMap::new(),
            unsent_retry: None,
            view: view().with_video_streams(video_streams),
            alone: Alone::default(),
            changes,
            incoming,
            failed,
            handles: Vec::new(),
            tasks: JoinSet::new(),
            camera_wanted: Arc::new(AtomicBool::new(false)),
            audio: AudioWants::default(),
            volume: watch::channel(MAX_VOLUME_PERCENT).0,
            microphone: watch::channel(None).0,
            devices: watch::channel(None).0,
            levels: BTreeMap::new(),
            portal: None,
            portal_capture: false,
            portal_sent: false,
            ring: None,
            screens: crate::calls::screen::unavailable(),
            screen_pick: None,
            screen_wanted: false,
            screen_last_on: false,
            screen_closed: None,
            screen_helper: None,
            screen_sent: false,
            proxy: false,
            consent: None,
            grants,
            grants_tx,
            screen_control: Arc::default(),
            screen_pick_stops: 0,
            screen_seen_stops: 0,
            screen_seen_request: 0,
            screen_share: 0,
            screen_stop_pending: None,
            screen_stop_settled: None,
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

    /// Fills the fake worker queue, then frees `room` places in it.
    fn fill_queue(call: &Call, to_worker: &mut mpsc::Receiver<ToWorker>, room: usize) {
        while call
            .queue_worker(ToWorker::SetMuted { muted: true })
            .unwrap()
        {}
        for _ in 0..room {
            to_worker.try_recv().unwrap();
        }
    }

    /// The `SetKey` commands the fake worker received, as identity and index.
    fn set_keys(to_worker: &mut mpsc::Receiver<ToWorker>) -> Vec<(String, u8)> {
        let mut sent = Vec::new();
        while let Ok(message) = to_worker.try_recv() {
            if let ToWorker::SetKey { identity, key } = message {
                sent.push((identity, key.index));
            }
        }
        sent
    }

    /// GIVEN a member with two backend identities and a queue with room for
    /// one key WHEN its key is sent THEN it is not shown as keyed until the
    /// key went under the second identity too.
    #[tokio::test]
    async fn given_a_key_under_one_of_two_identities_then_the_member_is_not_keyed_yet() {
        let fixture = test_call(
            no_admission(),
            Arc::new(Flags::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let (mut call, mut to_worker) = (fixture.call, fixture.to_worker);
        let device = "@b:example.org:B".to_owned();
        let mut b = member("@b:example.org", "B", "https://sfu.example.org");
        b.backend_identities = vec!["hash-b".to_owned(), device.clone()];
        call.view.set_members(vec![b]);
        call.remote.insert(&device, 1, &[1; 16], &|_| true).unwrap();

        fill_queue(&call, &mut to_worker, 1);
        call.mark_unsent(
            device.clone(),
            [("hash-b".to_owned(), 1), (device.clone(), 1)],
        );
        assert!(!call.send_unsent().unwrap());
        assert_eq!(set_keys(&mut to_worker), [("hash-b".to_owned(), 1)]);
        assert!(!call.view.keyed.contains(&device));

        fill_queue(&call, &mut to_worker, 1);
        assert!(call.send_unsent().unwrap());
        assert_eq!(set_keys(&mut to_worker), [(device.clone(), 1)]);
        assert!(call.view.keyed.contains(&device));
    }

    /// GIVEN keys waiting for the worker and a queue with room for one WHEN
    /// they are retried THEN each retry goes on with the next key, so every
    /// key arrives once; a key whose member left or whose backend identity
    /// went is dropped; and a closed queue ends the call as a stopped worker.
    #[tokio::test]
    async fn given_little_queue_room_then_unsent_keys_go_on_where_they_stopped() {
        let fixture = test_call(
            no_admission(),
            Arc::new(Flags::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let (mut call, mut to_worker) = (fixture.call, fixture.to_worker);
        let device = "@b:example.org:B".to_owned();
        let b = || member("@b:example.org", "B", "https://sfu.example.org");
        call.view.set_members(vec![b()]);
        for index in 1..=2 {
            call.remote
                .insert(&device, index, &[index; 16], &|_| true)
                .unwrap();
        }
        let waiting = [(device.clone(), 1), (device.clone(), 2)];

        fill_queue(&call, &mut to_worker, 0);
        call.mark_unsent(device.clone(), waiting.clone());
        assert!(!call.send_unsent().unwrap());
        assert!(call.unsent_retry.is_some(), "a full queue keeps the keys");
        assert!(
            !call.view.keyed.contains(&device),
            "no key reached the worker"
        );

        // Each pass has room for one key: the first, then the second. The
        // device is shown as keyed once none of its keys waits.
        for (pass, expected) in waiting.iter().enumerate() {
            fill_queue(&call, &mut to_worker, 1);
            assert_eq!(call.send_unsent().unwrap(), pass == 1);
            assert_eq!(set_keys(&mut to_worker), std::slice::from_ref(expected));
            assert_eq!(call.view.keyed.contains(&device), pass == 1);
        }
        assert!(call.unsent.is_empty());
        assert_eq!(call.unsent_retry, None);

        // A full queue keeps the keys of current members only.
        fill_queue(&call, &mut to_worker, 0);
        call.mark_unsent(device.clone(), waiting.clone());
        call.mark_unsent(
            "@z:example.org:Z".to_owned(),
            [("@z:example.org:Z".to_owned(), 1)],
        );
        call.send_unsent().unwrap();
        assert_eq!(call.unsent.keys().collect::<Vec<_>>(), [&device]);
        while to_worker.try_recv().is_ok() {}
        call.send_unsent().unwrap();
        assert!(call.unsent.is_empty());
        set_keys(&mut to_worker);

        call.mark_unsent(
            device.clone(),
            [("gone-id".to_owned(), 1), (device.clone(), 9)],
        );
        call.send_unsent().unwrap();
        assert!(
            set_keys(&mut to_worker).is_empty(),
            "no such identity or key"
        );
        call.mark_unsent(device.clone(), waiting.clone());
        call.view.set_members(Vec::new());
        call.send_unsent().unwrap();
        assert!(
            set_keys(&mut to_worker).is_empty(),
            "a member that left gets no key"
        );
        assert!(call.unsent.is_empty());

        call.view.set_members(vec![b()]);
        call.mark_unsent(device, waiting);
        drop(to_worker);
        assert!(matches!(call.send_unsent(), Err(EndReason::WorkerStopped)));
    }

    /// GIVEN more keys waiting for one member than are kept WHEN more come
    /// THEN the oldest go and a repeated key is kept once.
    #[test]
    fn given_many_unsent_keys_then_only_the_newest_are_kept() {
        let fixture = test_call(
            no_admission(),
            Arc::new(Flags::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let mut call = fixture.call;
        let device = "@b:example.org:B".to_owned();
        let count = MAX_UNSENT_PER_MEMBER + 3;
        let keys: Vec<(String, u8)> = (0..count)
            .map(|i| (device.clone(), u8::try_from(i).unwrap()))
            .collect();
        call.mark_unsent(device.clone(), keys.clone());
        call.mark_unsent(device.clone(), [keys[count - 1].clone()]);
        let kept: Vec<_> = call.unsent[&device].iter().cloned().collect();
        assert_eq!(kept, keys[3..]);
    }

    /// GIVEN a due own key and a full worker queue WHEN it is applied THEN
    /// the call goes on and the key waits; WHEN the queue has room THEN the
    /// worker switches to it; WHEN the queue is closed THEN the call ends.
    #[tokio::test]
    async fn given_a_full_queue_then_a_due_own_key_waits() {
        let fixture = test_call(
            no_admission(),
            Arc::new(Flags::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let (call, mut to_worker) = (fixture.call, fixture.to_worker);
        let key = FrameKeyMsg {
            index: 3,
            bytes: Zeroizing::new(vec![3; 16]),
        };
        let due = Instant::now();
        let mut pending = Some((due, key.clone()));

        fill_queue(&call, &mut to_worker, 0);
        call.apply_own_key(&mut pending).unwrap();
        let (retry, waiting) = pending.clone().expect("the key waits");
        assert!(retry > due);
        assert_eq!(waiting, key);

        while to_worker.try_recv().is_ok() {}
        call.apply_own_key(&mut pending).unwrap();
        assert_eq!(pending, None);
        assert!(matches!(
            to_worker.try_recv(),
            Ok(ToWorker::SetOwnKey { key: sent }) if sent == key
        ));

        let mut pending = Some((due, key));
        drop(to_worker);
        assert!(matches!(
            call.apply_own_key(&mut pending),
            Err(EndReason::WorkerStopped)
        ));
    }

    /// GIVEN a diagnostics switch and a full worker queue WHEN it is applied
    /// THEN the call goes on and the switch waits; WHEN the queue has room
    /// THEN the worker gets it; WHEN the queue is closed THEN the call ends.
    #[tokio::test]
    async fn given_a_full_queue_then_the_diagnostics_switch_waits() {
        let fixture = test_call(
            no_admission(),
            Arc::new(Flags::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let (call, mut to_worker) = (fixture.call, fixture.to_worker);
        let now = Instant::now();
        let mut pending = Some((now, true));

        fill_queue(&call, &mut to_worker, 0);
        call.apply_diagnostics(&mut pending).unwrap();
        let (retry, on) = pending.expect("the switch waits");
        assert!(retry > now && on);

        while to_worker.try_recv().is_ok() {}
        call.apply_diagnostics(&mut pending).unwrap();
        assert_eq!(pending, None);
        assert!(matches!(
            to_worker.try_recv(),
            Ok(ToWorker::SetDiagnostics { on: true })
        ));

        let mut pending = Some((now, false));
        drop(to_worker);
        assert!(matches!(
            call.apply_diagnostics(&mut pending),
            Err(EndReason::WorkerStopped)
        ));
    }

    /// GIVEN a device choice and a full worker queue WHEN it is applied
    /// THEN the call goes on and the choice waits; WHEN the queue has room
    /// THEN the worker gets it; WHEN the queue is closed THEN the call ends.
    #[tokio::test]
    async fn given_a_full_queue_then_a_device_choice_waits() {
        let fixture = test_call(
            no_admission(),
            Arc::new(Flags::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let (call, mut to_worker) = (fixture.call, fixture.to_worker);
        let now = Instant::now();
        let choice = DeviceChoice {
            recording: Some("mic-2".to_owned()),
            playout: None,
            camera: Some("cam-2".to_owned()),
        };
        let mut pending = Some((now, choice.clone()));

        fill_queue(&call, &mut to_worker, 0);
        call.apply_devices(&mut pending).unwrap();
        let (retry, waiting) = pending.clone().expect("the choice waits");
        assert!(retry > now);
        assert_eq!(waiting, choice);

        while to_worker.try_recv().is_ok() {}
        call.apply_devices(&mut pending).unwrap();
        assert_eq!(pending, None);
        assert!(matches!(
            to_worker.try_recv(),
            Ok(ToWorker::SetDevices { recording: Some(id), playout: None, camera: Some(cam) })
                if id == "mic-2" && cam == "cam-2"
        ));

        let mut pending = Some((now, choice));
        drop(to_worker);
        assert!(matches!(
            call.apply_devices(&mut pending),
            Err(EndReason::WorkerStopped)
        ));
    }

    /// GIVEN a device choice kept for a retry and a newer choice not read
    /// yet WHEN the retry is due THEN the newer choice replaces the kept one
    /// before anything is sent; with no newer choice the kept one stays.
    #[test]
    fn given_a_newer_choice_when_a_retry_is_due_then_only_the_newer_is_sent() {
        let sender = watch::channel(None::<DeviceChoice>).0;
        let mut devices = sender.subscribe();
        let choice = |id: &str| DeviceChoice {
            recording: Some(id.to_owned()),
            playout: None,
            camera: None,
        };
        let mut pending = Some((Instant::now(), choice("mic-1")));
        take_newer_choice(&mut devices, &mut pending);
        assert_eq!(
            pending.as_ref().map(|(_, c)| c.clone()),
            Some(choice("mic-1"))
        );
        sender.send_replace(Some(choice("mic-2")));
        take_newer_choice(&mut devices, &mut pending);
        assert_eq!(
            pending.as_ref().map(|(_, c)| c.clone()),
            Some(choice("mic-2"))
        );
        assert!(!devices.has_changed().unwrap(), "the newer choice is read");
    }

    /// GIVEN two members with a sound level WHEN one leaves the call THEN
    /// its level goes from the meters at once, and the member that stays
    /// keeps its level; a prune with no change sends nothing.
    #[test]
    fn given_a_member_with_a_level_when_it_leaves_then_its_level_goes() {
        let mut fixture = test_call(
            no_admission(),
            Arc::new(Flags::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let calls = controller(FakeHost::new(true, None));
        let updates = Arc::new(Mutex::new(Vec::new()));
        let heard = updates.clone();
        calls.set_sound_listener(Arc::new(move |update| plain(&heard).push(update)));
        let inner = calls.inner.clone();
        fixture.call.view = view();
        fixture.call.view.set_members(vec![
            member("@a:example.org", "A", "https://sfu.example.org"),
            member("@c:example.org", "C", "https://sfu.example.org"),
        ]);
        let call = &mut fixture.call;
        let members = call.view.members.clone();
        let (a, b) = (members[0].clone(), members[1].clone());
        call.participant_level(&inner, 0, &a.backend_identities[0], 40);
        call.participant_level(&inner, 0, &b.backend_identities[0], 20);
        assert_eq!(call.levels.len(), 2);
        call.view.set_members(vec![b.clone()]);
        call.prune_levels(&inner);
        let last = plain(&updates).last().cloned();
        assert_eq!(
            last,
            Some(SoundUpdate::Participants(vec![CallParticipantLevel {
                user_id: b.user_id.to_string(),
                device_id: b.device_id.to_string(),
                level: 20,
            }]))
        );
        let sent = plain(&updates).len();
        call.prune_levels(&inner);
        assert_eq!(plain(&updates).len(), sent, "no change sends nothing");
        // The member that stays moves to another SFU: connection 0 no longer
        // speaks for it, so its level goes too.
        call.view.set_members(vec![member(
            b.user_id.as_str(),
            b.device_id.as_str(),
            "https://other.example.org",
        )]);
        call.prune_levels(&inner);
        assert_eq!(
            plain(&updates).last().cloned(),
            Some(SoundUpdate::Participants(Vec::new()))
        );
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

    /// GIVEN the log on before the join WHEN the worker sends records before
    /// it reports `Connected` THEN the call goes on: the records end no call
    /// and change nothing, and `Connected` still applies (ADR-0058).
    #[test]
    fn given_records_before_connected_then_the_call_goes_on() {
        let fixture = test_call(
            no_admission(),
            Arc::new(Flags::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let inner = controller(FakeHost::new(true, None)).inner.clone();
        let mut call = fixture.call;
        for message in ["connecting", "ice candidate"] {
            let record = FromWorker::Diagnostic {
                level: 3,
                target: "livekit::room".into(),
                message: message.into(),
            };
            assert_eq!(call.worker_event(&inner, &record), Ok(()));
            assert_eq!(call.view.apply(&record), Applied::Same);
        }
        assert_eq!(call.view.apply(&FromWorker::Connected), Applied::Changed);
    }

    /// GIVEN a sound level listener WHEN the worker reports levels THEN each
    /// reaches the listener in order, and none changes the call state or
    /// tells the state listener (ADR-0055).
    #[test]
    fn given_sound_levels_then_they_reach_the_meter_and_not_the_state() {
        let fixture = test_call(
            no_admission(),
            Arc::new(Flags::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let calls = controller(FakeHost::new(true, None));
        let changes = Arc::new(AtomicUsize::new(0));
        let seen = changes.clone();
        calls.set_listener(Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        let levels = Arc::new(Mutex::new(Vec::new()));
        let heard = levels.clone();
        calls.set_sound_listener(Arc::new(move |update| plain(&heard).push(update)));
        let inner = calls.inner.clone();
        let mut call = fixture.call;
        call.worker_event(&inner, &FromWorker::Connected).unwrap();
        let before = changes.load(Ordering::SeqCst);
        let state = call.view.state();
        for level in [0, 55, 100] {
            let message = FromWorker::SoundLevel { level };
            assert_eq!(call.worker_event(&inner, &message), Ok(()));
            assert_eq!(call.view.apply(&message), Applied::Same);
        }
        assert_eq!(*plain(&levels), [0, 55, 100].map(SoundUpdate::Own));
        assert_eq!(call.view.state(), state);
        assert_eq!(changes.load(Ordering::SeqCst), before);
    }

    /// GIVEN a call alone for 30 seconds with a join that waits unread
    /// WHEN the alone deadline fires THEN the join is applied first and the
    /// call stays; with nothing waiting after another 30 seconds alone it
    /// ends; and a closed worker channel is the worker's stop.
    #[tokio::test(start_paused = true)]
    async fn given_a_join_waiting_at_the_alone_deadline_then_the_call_stays() {
        let mut fixture = test_call(
            no_admission(),
            Arc::new(Flags::default()),
            Arc::new(AtomicBool::new(false)),
        );
        let inner = controller(FakeHost::new(true, None)).inner.clone();
        let bob = |joined: bool| {
            let identity = "@b:example.org:B".to_owned();
            if joined {
                FromWorker::ParticipantJoined {
                    connection: 0,
                    identity,
                }
            } else {
                FromWorker::ParticipantLeft {
                    connection: 0,
                    identity,
                }
            }
        };
        fixture
            .call
            .worker_event(&inner, &FromWorker::Connected)
            .unwrap();
        time::advance(super::super::alone::ALONE_LIMIT).await;
        fixture._events.try_send(bob(true)).unwrap();
        assert_eq!(fixture.call.alone_still_due(&inner), Ok(false));
        assert_eq!(fixture.call.alone.deadline(), None);

        fixture.call.worker_event(&inner, &bob(false)).unwrap();
        time::advance(super::super::alone::ALONE_LIMIT).await;
        assert_eq!(fixture.call.alone_still_due(&inner), Ok(true));

        drop(fixture._events);
        assert_eq!(
            fixture.call.alone_still_due(&inner),
            Err(EndReason::WorkerStopped)
        );
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
        call.view.set_members(vec![
            member("@a:example.org", "A", "https://sfu.example.org"),
            member("@b:example.org", "B", "https://sfu.example.org"),
        ]);
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
            volume: call.volume.clone(),
            microphone: call.microphone.clone(),
            devices: call.devices.clone(),
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
            connection: 0,
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
        view.set_members(vec![
            member("@a:example.org", "A", "https://sfu.example.org"),
            member("@b:example.org", "B", "https://sfu.example.org"),
        ]);
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
                connection: 0,
                identity: "@b:example.org:B".to_owned()
            }),
            Applied::Changed
        );
        assert_eq!(view.video_allowed().1, [false; 5]);
        view.apply(&started(2, "@a:example.org:A"));
        assert!(view.forget_video(0, "@a:example.org:A"));
        assert!(!view.forget_video(0, "@a:example.org:A"));
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
            volume: watch::channel(MAX_VOLUME_PERCENT).0,
            microphone: watch::channel(None).0,
            devices: watch::channel(None).0,
            task: None,
        });
        calls.inner.publish(state);
        (store, command_rx, killed)
    }

    fn active_state() -> CallState {
        CallState::Active {
            room_id: room_id!("!r:example.org").to_owned(),
            muted: false,
            deafened: false,
            volume: MAX_VOLUME_PERCENT,
            participants: Vec::new(),
            camera: CallCamera::Off,
            screen: CallScreen::Off,
            video: Vec::new(),
            ring_declined: false,
            device_change: None,
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
            deafened: false,
            volume: MAX_VOLUME_PERCENT,
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
            device_change: None,
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
            name: persephone_call_protocol::MonitorName::new(br"\\.\DISPLAY2").expect("a name"),
            width: 1920,
            height: 1080,
            rect: persephone_call_protocol::DesktopRect::new(1920, 0, 3840, 1080)
                .expect("a rectangle"),
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
            helper: None,
        }
    }

    fn is_monitor_start(message: Option<ToWorker>) -> bool {
        matches!(
            message,
            Some(ToWorker::SetScreen {
                target: Some(target),
                ..
            }) if target == monitor()
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

    /// A Windows or macOS grant of one monitor: it has no portal session.
    fn bare_grant() -> ScreenGrant {
        ScreenGrant {
            target: monitor(),
            remote: None,
            session: None,
            closed: None,
            helper: None,
        }
    }

    fn is_stop_of(message: Result<ToWorker, mpsc::error::TryRecvError>, share: u32) -> bool {
        matches!(
            message,
            Ok(ToWorker::SetScreen { target: None, share: stopped }) if stopped == share
        )
    }

    /// A Windows or macOS share that runs, its start taken off the queue.
    fn running_bare_share() -> (TestCall, Arc<Inner>, u32) {
        let (mut fixture, inner, _) = call_with_source(true);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        fixture
            .call
            .screen_picked(&inner, Ok(bare_grant()))
            .unwrap();
        assert!(is_monitor_start(fixture.to_worker.try_recv().ok()));
        let share = fixture.call.screen_share;
        (fixture, inner, share)
    }

    /// What `set_screen(false)` does under the control lock before the call
    /// task reads the stop.
    fn handle_stop(call: &Call) {
        let mut control = plain(&call.screen_control);
        control.stops += 1;
        close_session(&mut control);
    }

    /// GIVEN a Windows or macOS share (no portal session) and a call task
    /// busy with other work WHEN the handle accepts a stop THEN the worker's
    /// stop is queued at once, behind the start; WHEN the task reads the
    /// stop later THEN no second stop goes, and the share is off.
    #[tokio::test]
    async fn given_a_share_without_a_session_when_the_handle_stops_it_then_the_worker_stops_at_once()
     {
        let (mut fixture, inner, share) = running_bare_share();
        handle_stop(&fixture.call);
        assert!(
            is_stop_of(fixture.to_worker.try_recv(), share),
            "the worker stops before the task reads the command"
        );
        fixture.call.screen_command(&inner, false, 1).unwrap();
        assert!(fixture.to_worker.try_recv().is_err(), "no second stop");
        assert!(!fixture.call.screen_sent);
        assert_eq!(fixture.call.view.screen, CallScreen::Off);
    }

    /// GIVEN a Windows or macOS share WHEN the task runs a stop that the
    /// handle did not close (or the worker ends the share itself) THEN
    /// exactly one stop goes (none after the worker's own end).
    #[tokio::test]
    async fn given_a_share_without_a_session_when_it_ends_then_one_stop_at_most_goes() {
        let (mut fixture, inner, share) = running_bare_share();
        fixture.call.screen_command(&inner, false, 0).unwrap();
        assert!(is_stop_of(fixture.to_worker.try_recv(), share));
        assert!(fixture.to_worker.try_recv().is_err(), "one stop only");

        let (mut fixture, inner, share) = running_bare_share();
        fixture
            .call
            .worker_event(
                &inner,
                &FromWorker::Screen {
                    state: ScreenState::NotFound,
                    share,
                },
            )
            .unwrap();
        assert_eq!(fixture.call.view.screen, CallScreen::NotFound);
        assert!(plain(&fixture.call.screen_control).session.is_none());
        assert!(
            fixture.to_worker.try_recv().is_err(),
            "an ended share gets no stop"
        );
        handle_stop(&fixture.call);
        fixture.call.screen_command(&inner, false, 1).unwrap();
        assert!(fixture.to_worker.try_recv().is_err());
    }

    /// GIVEN a Windows or macOS share and a full worker queue WHEN the
    /// handle accepts a stop THEN nothing is lost: the task's stop waits as
    /// the pending stop and goes when the queue has room.
    #[tokio::test]
    async fn given_a_full_queue_when_the_handle_stops_a_share_without_a_session_then_the_task_sends_it()
     {
        let (mut fixture, inner, share) = running_bare_share();
        fill_queue(&fixture.call, &mut fixture.to_worker, 0);
        handle_stop(&fixture.call);
        fixture.call.screen_command(&inner, false, 1).unwrap();
        assert_eq!(fixture.call.screen_stop_pending, Some(share));
        while fixture.to_worker.try_recv().is_ok() {}
        let permit = screen_stop_room(Some(fixture.call.link.command_sender())).await;
        fixture.call.screen_stop_room_ready(permit).unwrap();
        assert!(is_stop_of(fixture.to_worker.try_recv(), share));
        assert!(fixture.to_worker.try_recv().is_err(), "one stop only");
    }

    /// GIVEN a portal share (it has a session) WHEN it starts THEN its
    /// session is the portal's, not a worker stop: the Linux path is as
    /// before.
    #[tokio::test]
    async fn given_a_share_with_a_session_then_no_worker_stop_is_added() {
        let (mut fixture, inner, _) = call_with_source(true);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        fixture
            .call
            .screen_picked(&inner, Ok(grant(&dropped)))
            .unwrap();
        assert!(fixture.call.screen_stop_settled.is_none());
        let _ = fixture.to_worker.try_recv();
        handle_stop(&fixture.call);
        assert!(dropped.load(Ordering::SeqCst));
        assert!(
            fixture.to_worker.try_recv().is_err(),
            "the portal close revokes; the task sends the stop"
        );
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

    /// GIVEN a running call whose command queue is full WHEN the page stops
    /// the share THEN the stop is accepted: the portal session closes, the
    /// page shows `Off`, and the stop waits in the screen control with a
    /// wake for the task; a start is still refused on a full queue.
    #[tokio::test]
    async fn given_a_full_command_queue_when_the_page_stops_then_capture_is_revoked() {
        let calls = controller(FakeHost::new(true, None));
        let (_store, _commands, _) = running_call(&calls, active_state());
        let (control, sender) = {
            let Slot::Running(run) = &*calls.inner.slot() else {
                panic!("running");
            };
            (run.screen_control.clone(), run.commands.clone())
        };
        let dropped = Arc::new(AtomicBool::new(false));
        plain(&control).session = Some(Box::new(SessionGuard(dropped.clone())));
        assert_eq!(calls.set_screen(true), Ok(()));
        while sender.try_send(Command::Mute(true)).is_ok() {}

        assert_eq!(calls.set_screen(true), Err(CallError::NotInCall));
        assert_eq!(calls.set_screen(false), Ok(()));
        assert!(dropped.load(Ordering::SeqCst), "capture is revoked");
        assert!(matches!(
            calls.state(),
            CallState::Active {
                screen: CallScreen::Off,
                ..
            }
        ));
        let wake = {
            let control = plain(&control);
            assert_eq!(control.stop_waiting, Some((1, 2)));
            control.wake.clone()
        };
        tokio::time::timeout(Duration::from_secs(1), wake.notified())
            .await
            .expect("the task is woken");
    }

    /// GIVEN a share whose portal session is open WHEN the page stops it
    /// THEN the session closes before the page shows `Off`.
    #[tokio::test]
    async fn given_a_share_when_the_page_stops_it_then_the_session_closes_before_off_shows() {
        struct Watching(watch::Receiver<CallState>, Arc<Mutex<Option<CallScreen>>>);
        impl Drop for Watching {
            fn drop(&mut self) {
                if let CallState::Active { screen, .. } = &*self.0.borrow() {
                    *plain(&self.1) = Some(*screen);
                }
            }
        }
        let calls = controller(FakeHost::new(true, None));
        let (_store, _commands, _) = running_call(&calls, active_state());
        let control = {
            let Slot::Running(run) = &*calls.inner.slot() else {
                panic!("running");
            };
            run.screen_control.clone()
        };
        assert_eq!(calls.set_screen(true), Ok(()));
        let seen = Arc::new(Mutex::new(None));
        plain(&control).session = Some(Box::new(Watching(
            calls.inner.state.subscribe(),
            seen.clone(),
        )));
        assert_eq!(calls.set_screen(false), Ok(()));
        assert_eq!(
            *plain(&seen),
            Some(CallScreen::Starting),
            "the page did not show Off yet"
        );
        assert!(matches!(
            calls.state(),
            CallState::Active {
                screen: CallScreen::Off,
                ..
            }
        ));
    }

    /// GIVEN the call task closing the portal session (a worker end state),
    /// paused inside the close WHEN the page stops the share meanwhile THEN
    /// the stop waits for the close: the page shows `Off` only after it.
    #[test]
    fn given_a_close_under_way_when_the_page_stops_then_off_waits_for_it() {
        struct SlowClose {
            started: std::sync::mpsc::Sender<()>,
            page: watch::Receiver<CallState>,
            seen: Arc<Mutex<Option<CallScreen>>>,
        }
        impl Drop for SlowClose {
            fn drop(&mut self) {
                let _ = self.started.send(());
                std::thread::sleep(Duration::from_millis(50));
                if let CallState::Active { screen, .. } = &*self.page.borrow() {
                    *plain(&self.seen) = Some(*screen);
                }
            }
        }
        let calls = controller(FakeHost::new(true, None));
        let (_store, _commands, _) = running_call(&calls, active_state());
        let control = {
            let Slot::Running(run) = &*calls.inner.slot() else {
                panic!("running");
            };
            run.screen_control.clone()
        };
        assert_eq!(calls.set_screen(true), Ok(()));
        let (started, closing) = std::sync::mpsc::channel();
        let seen = Arc::new(Mutex::new(None));
        plain(&control).session = Some(Box::new(SlowClose {
            started,
            page: calls.inner.state.subscribe(),
            seen: seen.clone(),
        }));
        // What the call task's `close_screen_session` does.
        let task = {
            let control = control.clone();
            std::thread::spawn(move || close_session(&mut plain(&control)))
        };
        closing.recv().unwrap();
        assert_eq!(calls.set_screen(false), Ok(()));
        task.join().unwrap();
        assert_eq!(
            *plain(&seen),
            Some(CallScreen::Starting),
            "Off did not show during the close"
        );
        assert!(matches!(
            calls.state(),
            CallState::Active {
                screen: CallScreen::Off,
                ..
            }
        ));
    }

    /// GIVEN a start queued for the task and a later stop that waits in the
    /// screen control (the queue was full) WHEN the task reads the start
    /// THEN it opens no picker; WHEN it is woken THEN the stop runs. GIVEN a
    /// running share and a waiting stop WHEN a later start is read first
    /// THEN the stop goes to the worker before the start runs.
    #[tokio::test]
    async fn given_a_waiting_stop_then_an_earlier_start_opens_no_picker_and_the_stop_runs() {
        let (mut fixture, inner, _) = call_with_source(true);
        fixture.call.view.apply(&FromWorker::Connected);
        let shown = |inner: &Inner| match &*inner.state.borrow() {
            CallState::Active { screen, .. } => *screen,
            other => panic!("{other:?}"),
        };
        let wait = |control: &Mutex<ScreenControl>, inner: &Inner| {
            let (stops, request) = accept_command(control, inner, false);
            plain(control).stop_waiting = Some((stops, request));
        };
        let (stops, start) = accept_command(&fixture.call.screen_control, &inner, true);
        wait(&fixture.call.screen_control, &inner);
        fixture
            .call
            .screen_request(&inner, true, stops, start)
            .unwrap();
        assert!(fixture.call.screen_pick.is_none(), "no picker opens");
        assert_eq!(shown(&inner), CallScreen::Off);
        fixture.call.take_waiting_stop(&inner, u64::MAX).unwrap();
        assert!(plain(&fixture.call.screen_control).stop_waiting.is_none());
        assert_eq!(fixture.call.view.screen, CallScreen::Off);

        // A running share, then a stop that waits and a later start.
        let (stops, start) = accept_command(&fixture.call.screen_control, &inner, true);
        fixture
            .call
            .screen_request(&inner, true, stops, start)
            .unwrap();
        let first = Arc::new(AtomicBool::new(false));
        fixture
            .call
            .screen_picked(&inner, Ok(grant(&first)))
            .unwrap();
        let share = fixture.call.screen_share;
        while fixture.to_worker.try_recv().is_ok() {}
        wait(&fixture.call.screen_control, &inner);
        let (stops, later) = accept_command(&fixture.call.screen_control, &inner, true);
        fixture
            .call
            .screen_request(&inner, true, stops, later)
            .unwrap();
        assert!(matches!(
            fixture.to_worker.try_recv(),
            Ok(ToWorker::SetScreen { target: None, share: stopped }) if stopped == share
        ));
        assert!(fixture.call.screen_wanted, "the later start runs");
        assert_eq!(shown(&inner), CallScreen::Starting);
    }

    /// GIVEN a share's stop that waits for worker queue room and a new start
    /// that then fails (`Unavailable`) WHEN the stop goes and the worker
    /// reports the stopped share `Off` THEN the failure stays shown.
    #[tokio::test]
    async fn given_a_failed_restart_when_the_old_share_reports_off_then_the_failure_stays() {
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
        fixture.call.screen_command(&inner, false, 1).unwrap();
        fixture.call.screen_command(&inner, true, 1).unwrap();
        let second = Arc::new(AtomicBool::new(false));
        fixture
            .call
            .screen_picked(&inner, Ok(grant(&second)))
            .unwrap();
        assert_eq!(fixture.call.view.screen, CallScreen::Unavailable);

        while fixture.to_worker.try_recv().is_ok() {}
        let permit = screen_stop_room(Some(fixture.call.link.command_sender())).await;
        fixture.call.screen_stop_room_ready(permit).unwrap();
        fixture
            .call
            .worker_event(
                &inner,
                &FromWorker::Screen {
                    state: ScreenState::Off,
                    share,
                },
            )
            .unwrap();
        assert_eq!(fixture.call.view.screen, CallScreen::Unavailable);
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
    #[test]
    fn given_a_task_publication_when_the_page_stops_then_off_waits_for_the_lock() {
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
            connection: 0,
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
                connection: 0,
                identity: "@a:example.org:A".to_owned()
            }),
            Applied::Changed
        );
        assert!(!view.members.iter().any(|m| view.is_sharing(m)));
        assert!(!view.sharing.contains(&(0, "@a:example.org:A".to_owned())));
        assert!(view.video_tiles().is_empty());
    }

    /// GIVEN share hints for identities whose membership has not arrived
    /// WHEN the membership arrives THEN a member shows as sharing when the
    /// hint came from the connection that speaks for it, and not when it came
    /// from another connection; a stranger is never shown, and hints beyond
    /// the bound are not kept.
    #[test]
    fn given_a_share_hint_before_the_membership_then_it_shows_once_the_member_arrives() {
        let mut view = view();
        view.apply(&FromWorker::Connected);
        let early = FromWorker::ScreenSharer {
            connection: 0,
            identity: "@c:example.org:C".to_owned(),
            sharing: true,
        };
        assert_eq!(view.apply(&early), Applied::Same, "nothing shown yet");
        // Connection 2 does not speak for a member on the own SFU.
        let elsewhere = FromWorker::ScreenSharer {
            connection: 2,
            identity: "@d:example.org:D".to_owned(),
            sharing: true,
        };
        assert_eq!(view.apply(&elsewhere), Applied::Same);
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
            member("@d:example.org", "D", "https://sfu.example.org"),
        ]);
        assert_eq!(sharing(&view), ["@c:example.org:C"]);

        for index in 0..persephone_call_protocol::MAX_PARTICIPANTS + 4 {
            view.apply(&FromWorker::ScreenSharer {
                connection: 0,
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

    /// GIVEN a muted microphone reported before the membership of its
    /// member arrives WHEN the membership arrives THEN the member shows the
    /// mute mark; a hint from a connection that does not speak for the
    /// member does not show; unmuting and leaving remove the mark; hints
    /// beyond the bound are not kept.
    #[test]
    fn given_a_mute_hint_before_the_membership_then_it_shows_once_the_member_arrives() {
        let mut view = view();
        view.apply(&FromWorker::Connected);
        let muted = |connection, identity: &str, muted| FromWorker::ParticipantMuted {
            connection,
            identity: identity.to_owned(),
            muted,
        };
        assert_eq!(
            view.apply(&muted(0, "@c:example.org:C", true)),
            Applied::Same
        );
        assert_eq!(
            view.apply(&muted(2, "@d:example.org:D", true)),
            Applied::Same
        );
        let marked = |view: &View| {
            let CallState::Active { participants, .. } = view.state() else {
                panic!("active");
            };
            participants
                .iter()
                .filter(|p| p.muted)
                .map(|p| format!("{}:{}", p.user_id, p.device_id))
                .collect::<Vec<_>>()
        };
        assert!(marked(&view).is_empty());
        view.set_members(vec![
            member("@a:example.org", "A", "https://sfu.example.org"),
            member("@c:example.org", "C", "https://sfu.example.org"),
            member("@d:example.org", "D", "https://sfu.example.org"),
        ]);
        assert_eq!(marked(&view), ["@c:example.org:C"]);
        assert_eq!(
            view.apply(&muted(0, "@c:example.org:C", false)),
            Applied::Changed
        );
        assert!(marked(&view).is_empty());
        view.apply(&muted(0, "@a:example.org:A", true));
        assert_eq!(marked(&view), ["@a:example.org:A"]);
        assert_eq!(
            view.apply(&FromWorker::ParticipantLeft {
                connection: 0,
                identity: "@a:example.org:A".to_owned(),
            }),
            Applied::Changed
        );
        assert!(marked(&view).is_empty());

        for index in 0..persephone_call_protocol::MAX_PARTICIPANTS + 4 {
            view.apply(&muted(0, &format!("@s{index}:example.org:S"), true));
        }
        assert!(view.mic_muted.len() <= persephone_call_protocol::MAX_PARTICIPANTS);
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
            helper: None,
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
            helper: None,
        };
        fixture.call.screen_picked(&inner, Ok(no_remote)).unwrap();
        assert_eq!(fixture.call.view.screen, CallScreen::Unavailable);
    }

    // ---- The Windows capture helper (ADR-0067) ----

    use crate::calls::screen_helper::HelperShare;
    use crate::calls::screen_helper::fake::FakeHelper;

    /// A bare grant of one monitor with a fake helper for share `share`.
    fn helper_grant(helper: &Arc<FakeHelper>, share: u32) -> ScreenGrant {
        ScreenGrant {
            helper: Some(
                HelperShare::start(share, Arc::new(helper.clone())).expect("a helper share"),
            ),
            ..bare_grant()
        }
    }

    fn closed(helper: &Arc<FakeHelper>) -> usize {
        helper.controls_closed.load(Ordering::SeqCst)
    }

    /// GIVEN a grant with the helper of the next share WHEN the share starts
    /// THEN the call keeps the helper and the start goes to the worker; WHEN
    /// the share is stopped THEN the helper's control pipe closes and the
    /// worker's stop is queued, once.
    #[tokio::test]
    async fn given_a_helper_when_the_share_starts_and_stops_then_the_helper_stops_with_it() {
        let (mut fixture, inner, _) = call_with_source(true);
        let helper = FakeHelper::new(true, 0);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        fixture
            .call
            .screen_picked(&inner, Ok(helper_grant(&helper, 1)))
            .unwrap();
        assert!(is_monitor_start(fixture.to_worker.try_recv().ok()));
        assert!(fixture.call.screen_helper.is_some());
        assert_eq!(closed(&helper), 0, "the helper runs");
        fixture.call.screen_command(&inner, false, 0).unwrap();
        assert!(fixture.call.screen_helper.is_none());
        assert_eq!(closed(&helper), 1, "its control pipe closed");
        assert!(is_stop_of(fixture.to_worker.try_recv(), 1));
        assert!(fixture.to_worker.try_recv().is_err(), "one stop only");
        assert_eq!(fixture.call.view.screen, CallScreen::Off);
    }

    /// GIVEN a share with a helper and a full worker queue WHEN the handle
    /// accepts a stop THEN the helper's control pipe closes at once, before
    /// the call task reads the stop, so the screen is no longer copied while
    /// the worker's stop waits; WHEN the queue has room THEN that stop still
    /// goes, once, and the share is off.
    #[tokio::test]
    async fn given_a_full_queue_when_the_handle_stops_a_helper_share_then_the_helper_stops_at_once()
    {
        let (mut fixture, inner, _) = call_with_source(true);
        let helper = FakeHelper::new(true, 0);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        fixture
            .call
            .screen_picked(&inner, Ok(helper_grant(&helper, 1)))
            .unwrap();
        assert!(is_monitor_start(fixture.to_worker.try_recv().ok()));
        let share = fixture.call.screen_share;
        fill_queue(&fixture.call, &mut fixture.to_worker, 0);
        handle_stop(&fixture.call);
        assert_eq!(
            closed(&helper),
            1,
            "the capture ends before the task reads the stop"
        );
        assert!(
            fixture.call.screen_helper.is_some(),
            "the task still owns it"
        );
        fixture.call.screen_command(&inner, false, 1).unwrap();
        assert!(fixture.call.screen_helper.is_none());
        assert_eq!(fixture.call.screen_stop_pending, Some(share));
        while fixture.to_worker.try_recv().is_ok() {}
        let permit = screen_stop_room(Some(fixture.call.link.command_sender())).await;
        fixture.call.screen_stop_room_ready(permit).unwrap();
        assert!(is_stop_of(fixture.to_worker.try_recv(), share));
        assert!(fixture.to_worker.try_recv().is_err(), "one stop only");
        assert_eq!(fixture.call.view.screen, CallScreen::Off);
    }

    /// GIVEN a helper that serves another share than the next one WHEN the
    /// grant arrives THEN the share is unavailable, the helper is stopped and
    /// nothing goes to the worker.
    #[tokio::test]
    async fn given_a_helper_of_another_share_when_the_grant_arrives_then_it_is_stopped() {
        let (mut fixture, inner, _) = call_with_source(true);
        let helper = FakeHelper::new(true, 0);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        fixture
            .call
            .screen_picked(&inner, Ok(helper_grant(&helper, 7)))
            .unwrap();
        assert_eq!(fixture.call.view.screen, CallScreen::Unavailable);
        assert!(fixture.call.screen_helper.is_none());
        assert_eq!(closed(&helper), 1);
        assert!(fixture.to_worker.try_recv().is_err());
        assert!(!fixture.call.screen_sent);
    }

    /// GIVEN a stop that the handle accepted after the pick began WHEN the
    /// pick's grant with a helper arrives THEN the helper is stopped at once
    /// and no start goes to the worker; and GIVEN a stop that cancelled the
    /// pick (a stop during the helper's start) WHEN the cancelled pick ends
    /// THEN its helper is stopped too.
    #[tokio::test]
    async fn given_a_stop_during_the_helper_start_when_the_pick_ends_then_the_helper_is_stopped() {
        let (mut fixture, inner, _) = call_with_source(true);
        let helper = FakeHelper::new(true, 0);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        handle_stop(&fixture.call);
        fixture
            .call
            .screen_picked(&inner, Ok(helper_grant(&helper, 1)))
            .unwrap();
        assert_eq!(closed(&helper), 1, "a stale grant's helper stops");
        assert!(fixture.to_worker.try_recv().is_err(), "no start went");
        assert!(fixture.call.screen_helper.is_none());

        let (mut fixture, inner, _) = call_with_source(true);
        let helper = FakeHelper::new(true, 0);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        fixture.call.screen_command(&inner, false, 1).unwrap();
        fixture
            .call
            .stopped_pick_ended(Ok(helper_grant(&helper, 1)));
        assert_eq!(closed(&helper), 1, "a cancelled pick's helper stops");
        assert!(fixture.to_worker.try_recv().is_err());
    }

    /// GIVEN a running share WHEN its helper exits by itself with each state
    /// THEN the page shows that state, the worker's capture is stopped once,
    /// the helper is released and the call goes on; and a later worker state
    /// of the same share changes nothing.
    #[tokio::test]
    async fn given_a_helper_that_exits_when_the_share_runs_then_the_state_and_a_stop_follow() {
        for (state, shown) in [
            (ScreenState::NotFound, CallScreen::NotFound),
            (ScreenState::Unavailable, CallScreen::Unavailable),
            (ScreenState::Off, CallScreen::Off),
        ] {
            let (mut fixture, inner, _) = call_with_source(true);
            let helper = FakeHelper::new(true, 0);
            fixture.call.screen_command(&inner, true, 0).unwrap();
            fixture
                .call
                .screen_picked(&inner, Ok(helper_grant(&helper, 1)))
                .unwrap();
            assert!(is_monitor_start(fixture.to_worker.try_recv().ok()));
            fixture.call.screen_ended_by_helper(&inner, state).unwrap();
            assert_eq!(fixture.call.view.screen, shown, "{state:?}");
            assert!(fixture.call.screen_helper.is_none());
            assert!(!fixture.call.screen_sent);
            assert!(is_stop_of(fixture.to_worker.try_recv(), 1));
            assert!(fixture.to_worker.try_recv().is_err(), "one stop only");
            assert!(!fixture.call.screen_state_from_worker(ScreenState::On, 1));
        }
    }

    /// GIVEN a running share WHEN the worker reports that it ended (its end
    /// item named the reason) THEN the helper is released with the share and
    /// the worker's state shows; a state of an older share does not end it.
    #[tokio::test]
    async fn given_the_worker_ends_the_share_then_the_helper_is_released() {
        let (mut fixture, inner, _) = call_with_source(true);
        let helper = FakeHelper::new(true, 0);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        fixture
            .call
            .screen_picked(&inner, Ok(helper_grant(&helper, 1)))
            .unwrap();
        let _ = fixture.to_worker.try_recv();
        assert!(!fixture.call.screen_state_from_worker(ScreenState::Off, 9));
        assert!(
            fixture.call.screen_helper.is_some(),
            "another share's state"
        );
        assert!(
            fixture
                .call
                .screen_state_from_worker(ScreenState::NotFound, 1)
        );
        assert!(fixture.call.screen_helper.is_none());
        assert_eq!(closed(&helper), 1);
    }

    /// GIVEN a running share WHEN the call ends, or the core's session ends
    /// THEN the helper's control pipe closes and the helper is joined before
    /// the call is gone, with no stop of the share needed.
    #[tokio::test]
    async fn given_a_running_share_when_the_call_or_the_session_ends_then_the_helper_is_joined() {
        for session_ended in [false, true] {
            let (mut fixture, inner, _) = call_with_source(true);
            let helper = FakeHelper::new(true, 0);
            fixture.call.screen_command(&inner, true, 0).unwrap();
            fixture
                .call
                .screen_picked(&inner, Ok(helper_grant(&helper, 1)))
                .unwrap();
            assert_eq!(closed(&helper), 0);
            fixture.call.finish(session_ended).await;
            assert_eq!(closed(&helper), 1, "session_ended {session_ended}");
            assert!(!helper.terminated.load(Ordering::SeqCst));
            assert!(
                helper.wait_exit_for_test(Duration::from_secs(1)),
                "the helper ended"
            );
        }
    }

    /// GIVEN a helper that ignores its control pipe WHEN the call ends THEN
    /// the end waits the bounded time and terminates the Job: no helper is
    /// left running.
    #[tokio::test]
    async fn given_a_helper_that_hangs_when_the_call_ends_then_it_is_terminated() {
        let (mut fixture, inner, _) = call_with_source(true);
        let helper = FakeHelper::new(false, 0);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        fixture
            .call
            .screen_picked(&inner, Ok(helper_grant(&helper, 1)))
            .unwrap();
        fixture.call.finish(true).await;
        assert!(helper.terminated.load(Ordering::SeqCst));
    }

    /// GIVEN a share whose helper ignores its control pipe WHEN the call
    /// ends THEN the worker's `Leave` goes, and WHEN the core's session ends
    /// THEN the worker is killed, while the helper's stop still waits: the
    /// microphone, the camera and the room do not wait for the helper. The
    /// helper is still terminated before the end returns.
    #[tokio::test]
    async fn given_a_hanging_helper_when_the_call_ends_then_the_worker_stops_first() {
        use crate::calls::screen_helper::STOP_WAIT;
        for session_ended in [false, true] {
            let killed = Arc::new(AtomicBool::new(false));
            let mut fixture = test_call(no_admission(), Arc::new(Flags::default()), killed.clone());
            fixture.call.screens = Arc::new(Scripted {
                available: true,
                picks: AtomicUsize::new(0),
            });
            let inner = controller(FakeHost::new(true, None)).inner.clone();
            let helper = FakeHelper::new(false, 0);
            fixture.call.screen_command(&inner, true, 0).unwrap();
            fixture
                .call
                .screen_picked(&inner, Ok(helper_grant(&helper, 1)))
                .unwrap();
            assert!(is_monitor_start(fixture.to_worker.try_recv().ok()));
            let TestCall {
                call,
                mut to_worker,
                ..
            } = fixture;
            let end = tokio::spawn(call.finish(session_ended));
            let stopped = time::timeout(STOP_WAIT / 2, async {
                if session_ended {
                    while !killed.load(Ordering::SeqCst) {
                        time::sleep(Duration::from_millis(10)).await;
                    }
                } else {
                    while !matches!(to_worker.recv().await, Some(ToWorker::Leave) | None) {}
                }
            })
            .await;
            assert!(
                stopped.is_ok(),
                "session_ended {session_ended}: the worker stops at once"
            );
            assert!(
                !helper.terminated.load(Ordering::SeqCst),
                "the helper's stop still waits"
            );
            end.await.expect("the end of the call");
            assert!(helper.terminated.load(Ordering::SeqCst));
        }
    }

    /// A worker process that takes handles, as the Windows one does.
    #[cfg(windows)]
    struct GivingProcess(Arc<AtomicBool>);

    #[cfg(windows)]
    impl WorkerProcess for GivingProcess {
        fn kill(&self) {
            self.0.store(true, Ordering::SeqCst);
        }
        fn wait_exit(&self, _: Duration) -> bool {
            true
        }
        fn give_handle(&self, _handle: &std::os::windows::io::OwnedHandle) -> Option<u32> {
            Some(77)
        }
    }

    /// GIVEN a grant with a frame pipe and a helper WHEN the share starts
    /// THEN the pipe goes into the worker (`ScreenFrames` with the handle
    /// value and the share number) and the start follows it, in that order;
    /// a worker that cannot take the handle fails the share and stops the
    /// helper.
    #[cfg(windows)]
    #[tokio::test]
    async fn given_a_frame_pipe_when_the_share_starts_then_screen_frames_goes_before_the_start() {
        use std::os::windows::io::OwnedHandle;
        let pipe = || {
            let (read, write) = std::io::pipe().expect("a pipe");
            (OwnedHandle::from(read), OwnedHandle::from(write))
        };
        let (mut fixture, inner, _) = call_with_source(true);
        let (command_tx, mut commands) = mpsc::channel(4);
        fixture.call.link = CallWorkerLink::from_parts(
            command_tx,
            mpsc::channel(1).1,
            Box::new(GivingProcess(Arc::new(AtomicBool::new(false)))),
        );
        fixture.call.screen_command(&inner, true, 0).unwrap();
        let helper = FakeHelper::new(true, 0);
        let (read, _write) = pipe();
        let grant = ScreenGrant {
            remote: Some(read),
            ..helper_grant(&helper, 1)
        };
        fixture.call.screen_picked(&inner, Ok(grant)).unwrap();
        assert!(matches!(
            commands.try_recv(),
            Ok(ToWorker::ScreenFrames {
                share: 1,
                handle: 77
            })
        ));
        assert!(is_monitor_start(commands.try_recv().ok()));
        assert!(fixture.call.screen_helper.is_some());

        // A worker that cannot take the handle: the share fails.
        let (mut fixture, inner, _) = call_with_source(true);
        fixture.call.screen_command(&inner, true, 0).unwrap();
        let helper = FakeHelper::new(true, 0);
        let (read, _write) = pipe();
        let grant = ScreenGrant {
            remote: Some(read),
            ..helper_grant(&helper, 1)
        };
        fixture.call.screen_picked(&inner, Ok(grant)).unwrap();
        assert_eq!(fixture.call.view.screen, CallScreen::Unavailable);
        assert_eq!(closed(&helper), 1, "the helper stops");
        assert!(fixture.to_worker.try_recv().is_err());
    }

    /// A worker process that counts the handles it is given.
    #[cfg(windows)]
    struct CountingProcess(Arc<AtomicUsize>);

    #[cfg(windows)]
    impl WorkerProcess for CountingProcess {
        fn kill(&self) {}
        fn wait_exit(&self, _: Duration) -> bool {
            true
        }
        fn give_handle(&self, _handle: &std::os::windows::io::OwnedHandle) -> Option<u32> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Some(78)
        }
    }

    /// GIVEN a worker queue with one free slot WHEN a share with a frame
    /// pipe starts THEN the share alone fails: no pipe goes into the worker,
    /// nothing more is queued, the helper stops and the call runs on.
    #[cfg(windows)]
    #[tokio::test]
    async fn given_one_free_slot_when_a_share_with_a_pipe_starts_then_only_the_share_fails() {
        use std::os::windows::io::OwnedHandle;
        let (mut fixture, inner, _) = call_with_source(true);
        let (command_tx, mut commands) = mpsc::channel(2);
        let given = Arc::new(AtomicUsize::new(0));
        fixture.call.link = CallWorkerLink::from_parts(
            command_tx,
            mpsc::channel(1).1,
            Box::new(CountingProcess(given.clone())),
        );
        fixture.call.screen_command(&inner, true, 0).unwrap();
        fixture
            .call
            .link
            .send(ToWorker::SetMuted { muted: true })
            .expect("one of two slots");
        let helper = FakeHelper::new(true, 0);
        let (read, _write) = std::io::pipe().expect("a pipe");
        let grant = ScreenGrant {
            remote: Some(OwnedHandle::from(read)),
            ..helper_grant(&helper, 1)
        };
        assert_eq!(fixture.call.screen_picked(&inner, Ok(grant)), Ok(()));
        assert_eq!(fixture.call.view.screen, CallScreen::Unavailable);
        assert_eq!(given.load(Ordering::SeqCst), 0, "no pipe went in");
        assert!(matches!(
            commands.try_recv(),
            Ok(ToWorker::SetMuted { muted: true })
        ));
        assert!(commands.try_recv().is_err(), "nothing more is queued");
        assert_eq!(closed(&helper), 1, "the helper stops");
        assert!(fixture.call.screen_helper.is_none());
    }

    /// GIVEN a pick that starts its helper just as the call ends WHEN the
    /// call finishes THEN the finish waits for the pick, and that helper is
    /// stopped and has ended before the finish returns.
    #[cfg(windows)]
    #[tokio::test]
    async fn given_a_pick_that_starts_a_helper_when_the_call_ends_then_the_helper_is_stopped() {
        let (mut fixture, _inner, _) = call_with_source(true);
        let helper = FakeHelper::new(true, 0);
        let cancel = Arc::new(AtomicBool::new(false));
        let seen = cancel.clone();
        let started = helper.clone();
        fixture.call.screen_pick = Some(ScreenPick {
            task: tokio::task::spawn_blocking(move || {
                // A launch that the cancel came too late to stop.
                while !seen.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(helper_grant(&started, 1))
            }),
            cancel,
        });
        fixture.call.finish(true).await;
        assert_eq!(closed(&helper), 1, "the helper was stopped");
        assert!(helper.wait_exit_for_test(Duration::ZERO), "and it ended");
    }

    // ---- Other SFUs (ADR-0054) ----

    fn on_own(user: &str, device: &str) -> ActiveMember {
        member(user, device, "https://sfu.example.org")
    }

    fn on(user: &str, device: &str, sfu: &str) -> ActiveMember {
        member(user, device, sfu)
    }

    fn speaking(connection: u8, id: &str, on: bool) -> FromWorker {
        FromWorker::Speaking {
            connection,
            identity: id.to_owned(),
            speaking: on,
        }
    }

    /// A view with a member on the own SFU and one on `https://other.example.org`
    /// whose connection (1) is up.
    fn view_with_other_up() -> View {
        let mut view = view();
        view.apply(&FromWorker::Connected);
        view.other.track(&view.members, &view.own_sfu);
        view.other.plan();
        view.other.answer("other.example.org", true);
        assert_eq!(
            view.apply(&FromWorker::Connection {
                connection: 1,
                up: true
            }),
            Applied::Changed
        );
        view
    }

    /// GIVEN a member on another SFU WHEN its connection is not up, up, then
    /// down THEN it is not reachable, reachable, then not reachable, and the
    /// own-SFU member stays reachable throughout.
    #[test]
    fn given_another_sfu_connection_then_reachable_follows_the_worker() {
        let mut view = view();
        view.apply(&FromWorker::Connected);
        let reachable = |view: &View| match view.state() {
            CallState::Active { participants, .. } => {
                participants.iter().map(|p| p.reachable).collect::<Vec<_>>()
            }
            _ => panic!("active"),
        };
        assert_eq!(reachable(&view), [true, false]);
        view.other.track(&view.members, &view.own_sfu);
        view.other.plan();
        view.other.answer("other.example.org", true);
        assert_eq!(
            reachable(&view),
            [true, false],
            "connecting is not reachable"
        );
        view.apply(&FromWorker::Connection {
            connection: 1,
            up: true,
        });
        assert_eq!(reachable(&view), [true, true]);
        assert_eq!(
            view.apply(&FromWorker::Connection {
                connection: 1,
                up: false
            }),
            Applied::Changed
        );
        assert_eq!(reachable(&view), [true, false]);
        // The call goes on: the message is not an end, and a repeat changes nothing.
        assert_eq!(
            view.apply(&FromWorker::Connection {
                connection: 1,
                up: false
            }),
            Applied::Same
        );
    }

    /// GIVEN a member on another SFU WHEN a participant of that name comes
    /// from the wrong connection (0, 2, 3) THEN speaking, sharing and video are
    /// ignored; WHEN it comes from its own connection (1) THEN they show.
    #[test]
    fn given_a_participant_from_the_wrong_connection_then_it_is_ignored() {
        let mut view = view_with_other_up();
        let b = "@b:example.org:B";
        for wrong in [0, 2, 3] {
            assert_eq!(view.apply(&speaking(wrong, b, true)), Applied::Same);
            assert_eq!(
                view.apply(&FromWorker::ScreenSharer {
                    connection: wrong,
                    identity: b.to_owned(),
                    sharing: true
                }),
                Applied::Same
            );
            view.apply(&started_on(wrong, 1, b));
            assert_eq!(view.video_allowed().1, [false; 5], "connection {wrong}");
        }
        assert!(view.speaking.is_empty() && view.sharing.is_empty());
        assert_eq!(view.apply(&speaking(1, b, true)), Applied::Changed);
        assert_eq!(
            view.apply(&FromWorker::ScreenSharer {
                connection: 1,
                identity: b.to_owned(),
                sharing: true
            }),
            Applied::Changed
        );
        view.apply(&started_on(1, 2, b));
        assert_eq!(view.video_allowed().1, [false, true, false, false, false]);
        // A member on the own SFU is heard from connection 0 only.
        let a = "@a:example.org:A";
        assert_eq!(view.apply(&speaking(1, a, true)), Applied::Same);
        assert_eq!(view.apply(&speaking(0, a, true)), Applied::Changed);
    }

    /// GIVEN a member on another SFU that also shows as a silent participant
    /// on the own SFU WHEN that presence leaves the own SFU THEN its speaking
    /// and video from its own SFU stay.
    #[test]
    fn given_a_presence_on_the_own_sfu_leaves_then_the_other_sfu_state_stays() {
        let mut view = view_with_other_up();
        let b = "@b:example.org:B";
        view.apply(&speaking(1, b, true));
        view.apply(&started_on(1, 1, b));
        assert_eq!(
            view.apply(&FromWorker::ParticipantLeft {
                connection: 0,
                identity: b.to_owned()
            }),
            Applied::Same
        );
        assert!(view.speaking.contains(b));
        assert!(view.video_allowed().1[0]);
        assert_eq!(
            view.apply(&FromWorker::ParticipantLeft {
                connection: 1,
                identity: b.to_owned()
            }),
            Applied::Changed
        );
        assert!(view.speaking.is_empty());
        assert_eq!(view.video_allowed().1, [false; 5]);
    }

    /// GIVEN video of a member on another SFU WHEN its connection ends THEN
    /// the tile is hidden and its pictures are no longer allowed, and the call
    /// state stays active.
    #[test]
    fn given_another_sfu_ends_then_its_video_is_hidden_and_the_call_stays() {
        let mut view = view_with_other_up();
        view.apply(&started_on(1, 3, "@b:example.org:B"));
        assert_eq!(view.video_allowed().1, [false, false, true, false, false]);
        view.apply(&FromWorker::Connection {
            connection: 1,
            up: false,
        });
        // The worker stops the tile with VideoStopped; until then the member
        // is not reachable, but the page already sees the call as active.
        assert!(matches!(view.state(), CallState::Active { .. }));
    }

    fn started_on(connection: u8, tile: u8, identity: &str) -> FromWorker {
        FromWorker::VideoStarted {
            tile,
            connection,
            identity: identity.to_owned(),
        }
    }

    fn other_call(host: &Arc<FakeHost>, killed: Arc<AtomicBool>) -> (TestCall, Arc<Inner>) {
        let inner = controller(host.clone()).inner.clone();
        let mut fixture = test_call(no_admission(), Arc::new(Flags::default()), killed);
        fixture.call.view.set_members(vec![
            on_own("@a:example.org", "A"),
            on("@b:example.org", "B", "https://other.example.org/one"),
            on("@c:example.org", "C", "https://other.example.org/two"),
        ]);
        (fixture, inner)
    }

    /// GIVEN members on two SFUs of one other host WHEN the call plans THEN the
    /// core asks once, with the host and the proxy flag, contacts nobody
    /// before the answer, and the call goes on while the window is open.
    #[tokio::test]
    async fn given_members_on_another_host_then_the_user_is_asked_once_and_nothing_is_contacted_first()
     {
        let host = FakeHost::answering_other(true);
        let (mut fixture, inner) = other_call(&host, Arc::new(AtomicBool::new(false)));
        fixture.call.proxy = true;
        let call = &mut fixture.call;
        call.worker_event(&inner, &FromWorker::Connected).unwrap();
        call.plan_others(&inner).unwrap();
        assert!(call.consent.is_some());
        assert!(call.tasks.is_empty(), "no token request before the answer");
        // The call goes on while the window is open.
        call.worker_event(&inner, &FromWorker::Muted { muted: true })
            .unwrap();
        assert!(matches!(
            inner.state.borrow().clone(),
            CallState::Active { muted: true, .. }
        ));
        let (asked, connect) = consent_done(&mut call.consent).await;
        assert_eq!((asked.as_str(), connect), ("other.example.org", true));
        call.consent_answered(&inner, &asked, connect).unwrap();
        assert_eq!(call.tasks.len(), 2, "one token request for each SFU");
        // Planning again, as a member change does, asks nothing more.
        call.plan_others(&inner).unwrap();
        assert!(call.consent.is_none());
        assert_eq!(host.asked(), [("other.example.org".to_owned(), true)]);
    }

    /// GIVEN the user answers "Not now" WHEN the call plans again, also after a
    /// new member on that host THEN nobody is asked again, no token is
    /// requested, nothing goes to the worker, and the members are not
    /// reachable.
    #[tokio::test]
    async fn given_not_now_then_that_host_is_not_asked_or_contacted_again() {
        let host = FakeHost::answering_other(false);
        let (mut fixture, inner) = other_call(&host, Arc::new(AtomicBool::new(false)));
        let call = &mut fixture.call;
        call.worker_event(&inner, &FromWorker::Connected).unwrap();
        call.plan_others(&inner).unwrap();
        let (asked, connect) = consent_done(&mut call.consent).await;
        assert!(!connect);
        call.consent_answered(&inner, &asked, connect).unwrap();
        let mut members = call.view.members.clone();
        members.push(on("@d:example.org", "D", "https://other.example.org/three"));
        call.view.set_members(members);
        call.plan_others(&inner).unwrap();
        assert!(call.consent.is_none() && call.tasks.is_empty());
        assert!(fixture.to_worker.try_recv().is_err());
        assert_eq!(host.asked().len(), 1);
        let CallState::Active { participants, .. } = call.view.state() else {
            panic!("active");
        };
        assert_eq!(
            participants.iter().map(|p| p.reachable).collect::<Vec<_>>(),
            [true, false, false, false]
        );
    }

    /// GIVEN a token request that failed WHEN its result arrives THEN the
    /// members are not reachable, no Subscribe goes out and the call is not
    /// ended.
    #[tokio::test]
    async fn given_a_failed_token_request_then_the_call_goes_on_without_those_members() {
        let host = FakeHost::answering_other(true);
        let (mut fixture, inner) = other_call(&host, Arc::new(AtomicBool::new(false)));
        let call = &mut fixture.call;
        call.worker_event(&inner, &FromWorker::Connected).unwrap();
        call.plan_others(&inner).unwrap();
        let (asked, connect) = consent_done(&mut call.consent).await;
        call.consent_answered(&inner, &asked, connect).unwrap();
        // No session admits work in this fixture, so each request ends at once
        // without a grant.
        for _ in 0..2 {
            let (index, grant) = call.grants.recv().await.expect("a result");
            assert!(grant.is_none());
            call.grant_received(&inner, index, grant).unwrap();
        }
        assert!(fixture.to_worker.try_recv().is_err());
        assert!(matches!(
            inner.state.borrow().clone(),
            CallState::Active { .. }
        ));
    }

    /// GIVEN a grant for a connecting SFU WHEN it arrives THEN the worker gets
    /// `Subscribe` with that index, URL and token; a grant for a connection
    /// nobody waits for sends nothing.
    #[tokio::test]
    async fn given_a_grant_then_the_worker_gets_subscribe_for_that_connection_only() {
        use zeroize::Zeroizing;
        let host = FakeHost::answering_other(true);
        let (mut fixture, inner) = other_call(&host, Arc::new(AtomicBool::new(false)));
        let call = &mut fixture.call;
        call.plan_others(&inner).unwrap();
        let (asked, connect) = consent_done(&mut call.consent).await;
        call.consent_answered(&inner, &asked, connect).unwrap();
        let grant = |url: &str| SfuGrant {
            url: Url::parse(url).unwrap(),
            jwt: Zeroizing::new("a.b.c".to_owned()),
        };
        call.grant_received(&inner, 1, Some(grant("wss://sfu.other.example")))
            .unwrap();
        let sent = fixture.to_worker.try_recv().expect("Subscribe");
        let ToWorker::Subscribe { index, url, jwt } = sent else {
            panic!("Subscribe, not {sent:?}");
        };
        assert_eq!(
            (index, url.as_str(), jwt.as_str()),
            (1, "wss://sfu.other.example/", "a.b.c")
        );
        // Connection 3 was never planned.
        call.grant_received(&inner, 3, Some(grant("wss://sfu.other.example")))
            .unwrap();
        assert!(fixture.to_worker.try_recv().is_err());
    }

    /// GIVEN a grant whose `wss://` host is an IP literal, `localhost`, a
    /// single-label, `.local`, `.home.arpa` or `.internal` name
    /// WHEN it arrives THEN nothing goes to the worker, the members stay not
    /// reachable and the call goes on.
    #[tokio::test]
    async fn given_a_grant_with_a_refused_host_then_no_subscribe_goes_out() {
        use zeroize::Zeroizing;
        let refused = [
            "wss://192.0.2.10",
            "wss://[2001:db8::1]",
            "wss://localhost",
            "wss://LocalHost.",
            "wss://x.localhost",
            "wss://sfu",
            "wss://x.local",
            "wss://x.home.arpa",
            "wss://x.internal.",
        ];
        for bad in refused {
            let host = FakeHost::answering_other(true);
            let (mut fixture, inner) = other_call(&host, Arc::new(AtomicBool::new(false)));
            let call = &mut fixture.call;
            call.worker_event(&inner, &FromWorker::Connected).unwrap();
            call.plan_others(&inner).unwrap();
            let (asked, connect) = consent_done(&mut call.consent).await;
            call.consent_answered(&inner, &asked, connect).unwrap();
            let grant = SfuGrant {
                url: Url::parse(bad).unwrap(),
                jwt: Zeroizing::new("a.b.c".to_owned()),
            };
            call.grant_received(&inner, 1, Some(grant)).unwrap();
            assert!(fixture.to_worker.try_recv().is_err(), "{bad}");
            assert!(call.view.other.connecting_url(1).is_none(), "{bad}");
            assert!(matches!(
                inner.state.borrow().clone(),
                CallState::Active { .. }
            ));
        }
    }

    /// GIVEN a call with other SFUs WHEN the call ends (the session ends)
    /// THEN the worker, which holds every connection, is ended at once, and a
    /// question still open is dropped with the call.
    #[tokio::test]
    async fn given_a_call_with_other_sfus_ends_then_the_worker_is_ended() {
        let host = FakeHost::answering_other(true);
        let killed = Arc::new(AtomicBool::new(false));
        let (fixture, inner) = other_call(&host, killed.clone());
        let mut call = fixture.call;
        call.plan_others(&inner).unwrap();
        assert!(call.consent.is_some());
        call.finish(true).await;
        assert!(killed.load(Ordering::SeqCst));
    }
}
