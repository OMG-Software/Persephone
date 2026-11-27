//! The pure parts of the worker: no LiveKit types, no I/O, no clock reads.

pub mod stats;

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::{Duration, Instant};

use persephone_call_protocol::{
    FailureCode, FromWorker, MAX_DEVICES, MAX_IDENTITY_BYTES, MAX_OTHER_CONNECTIONS,
    MAX_PARTICIPANTS, MAX_SOUND_LEVEL, MAX_VOLUME_PERCENT, MediaDevice, ToWorker, pulse_id,
    pulse_name, valid_mic_gain,
};

/// The most `Speaking` messages sent in any one second.
pub const MAX_SPEAKING_PER_SECOND: usize = 10;
const WINDOW: Duration = Duration::from_secs(1);

/// An identity is forwarded only when the protocol would accept it back.
pub fn valid_identity(identity: &str) -> bool {
    !identity.is_empty()
        && identity.len() <= MAX_IDENTITY_BYTES
        && !identity.chars().any(char::is_control)
}

/// The first message must be `Join`, `ListDevices` or `TestDevice`; nothing
/// else may start.
pub fn is_first_message_allowed(message: &ToWorker) -> bool {
    matches!(
        message,
        ToWorker::Join { .. } | ToWorker::ListDevices | ToWorker::TestDevice { .. }
    )
}

/// LiveKit makes `index:N` IDs when the OS gives no GUID. They change when a
/// device is plugged in or out, so they are never listed or selected.
pub fn is_listable_device_id(id: &str) -> bool {
    !id.starts_with("index:")
}

/// Picks the wanted device from the available IDs. `None` means the system
/// default (no choice). A missing or unstable ID is `DeviceNotFound`.
pub fn choose_device<'a>(
    wanted: Option<&str>,
    available: impl IntoIterator<Item = &'a str>,
) -> Result<Option<String>, FailureCode> {
    let Some(wanted) = wanted else {
        return Ok(None);
    };
    if !is_listable_device_id(wanted) {
        return Err(FailureCode::DeviceNotFound);
    }
    if available.into_iter().any(|id| id == wanted) {
        Ok(Some(wanted.to_owned()))
    } else {
        Err(FailureCode::DeviceNotFound)
    }
}

/// The device to open when the user chose none: the system's default for
/// media (`sound_defaults`), when the list has it. `None` leaves libwebrtc's
/// own choice, which on Windows is the default communications device. An
/// unstable ID is never selected.
pub fn system_default<'a>(
    system: Option<&str>,
    available: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let system = system.filter(|id| is_listable_device_id(id))?;
    available
        .into_iter()
        .any(|id| id == system)
        .then(|| system.to_owned())
}

/// Builds one wire list from (ID, name) pairs. It skips unstable IDs, IDs the
/// wire refuses and repeated IDs, and keeps at most `MAX_DEVICES`.
pub fn bounded_device_list<'a>(
    devices: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<MediaDevice> {
    let mut list: Vec<MediaDevice> = Vec::new();
    for (id, name) in devices {
        if list.len() >= MAX_DEVICES {
            break;
        }
        if !is_listable_device_id(id) || list.iter().any(|device| device.id == id) {
            continue;
        }
        if let Some(device) = MediaDevice::bounded(id, name) {
            list.push(device);
        }
    }
    list
}

/// The wire list of sound server devices from (name, description) pairs
/// (Linux, ADR-0049). Each ID is `pulse:` and the name. A name that
/// `valid_pulse_name` refuses is left out, so it can never reach the
/// worker's environment in a later call. Otherwise as
/// [`bounded_device_list`].
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn pulse_device_list<'a>(
    devices: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<MediaDevice> {
    let tagged: Vec<(String, &str)> = devices
        .into_iter()
        .filter_map(|(name, description)| pulse_id(name).map(|id| (id, description)))
        .take(2 * MAX_DEVICES)
        .collect();
    bounded_device_list(tagged.iter().map(|(id, name)| (id.as_str(), *name)))
}

/// How a microphone or speaker choice is opened.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SoundPath {
    /// Through libwebrtc's own device list (`choose_device`).
    Listed,
    /// By sound server name (Linux): libwebrtc opens its default stream, and
    /// the client library sends it to the name the launcher put in the
    /// environment. libwebrtc's device selection is left alone.
    ServerName,
}

/// Checks a microphone or speaker choice against the sound server name in
/// the worker's environment (`PULSE_SOURCE` or `PULSE_SINK`, set by the
/// launcher) and the names the sound server has now (`None`: it could not be
/// asked).
///
/// * A `pulse:` choice needs the same name in the environment, else
///   `Protocol` (the core and the launch disagree), and a device of that
///   name now, else `DeviceNotFound`. The check is needed: the sound server
///   quietly uses its default for a name it does not have, and a chosen
///   device must never fall back to another one.
/// * Any other choice, or none, needs no name in the environment, else
///   `Protocol` (the environment would move the default stream).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn sound_path(
    wanted: Option<&str>,
    environment: Option<&str>,
    present: Option<&[&str]>,
) -> Result<SoundPath, FailureCode> {
    match wanted.and_then(pulse_name) {
        Some(name) => {
            if environment != Some(name) {
                Err(FailureCode::Protocol)
            } else if present.is_some_and(|names| names.contains(&name)) {
                Ok(SoundPath::ServerName)
            } else {
                Err(FailureCode::DeviceNotFound)
            }
        }
        None if environment.is_some() => Err(FailureCode::Protocol),
        None => Ok(SoundPath::Listed),
    }
}

/// The most own sound server streams of one kind that a move touches.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const MAX_OWN_STREAMS: usize = 8;

/// The indexes of this process's own streams from a sound server listing of
/// `(index, owner process ID)` pairs (Linux, ADR-0049). A stream with no
/// owner, or with another owner, is never listed: no other program's stream
/// is ever moved. At most [`MAX_OWN_STREAMS`] are kept.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn own_streams(
    streams: impl IntoIterator<Item = (u32, Option<u32>)>,
    own_process: u32,
) -> Vec<u32> {
    streams
        .into_iter()
        .filter(|(_, owner)| *owner == Some(own_process))
        .map(|(index, _)| index)
        .take(MAX_OWN_STREAMS)
        .collect()
}

/// What the call knows about its sound server devices (Linux, ADR-0049).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default)]
pub struct SoundKnown<'a> {
    /// The sound server's default microphone and speaker names when the
    /// call started: what "System default" means during the call (ADR-0059).
    pub default_source: Option<&'a str>,
    pub default_sink: Option<&'a str>,
    /// The microphone and speaker the call's streams are on now; `None`
    /// when that is not known.
    pub source: Option<&'a str>,
    pub sink: Option<&'a str>,
}

/// What the sound server shows now (Linux, ADR-0049).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Copy, Debug)]
pub struct SoundNow<'a> {
    /// The names of the microphones and speakers (monitors left out).
    pub sources: &'a [&'a str],
    pub sinks: &'a [&'a str],
    /// How many own record and playback streams the call has.
    pub own_recording: usize,
    pub own_playout: usize,
}

/// What to do for one kind (microphone or speaker) after a choice.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum SoundTarget {
    /// The call already uses the chosen device. A device that waited for a
    /// stream is no longer wanted.
    #[default]
    Keep,
    /// Move the own streams to this name now.
    Move(String),
    /// The call has no own stream of this kind yet (nobody else is in the
    /// call, so libwebrtc opened no playback stream). Keep the name until a
    /// stream exists; the change counts as done. A newer choice replaces it.
    Wait(String),
}

/// The sound server targets for the microphone and the speaker.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SoundMove {
    pub source: SoundTarget,
    pub sink: SoundTarget,
}

/// The target for one kind.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn sound_target(name: &str, in_use: Option<&str>, own_streams: usize) -> SoundTarget {
    if in_use == Some(name) {
        SoundTarget::Keep
    } else if own_streams == 0 {
        SoundTarget::Wait(name.to_owned())
    } else {
        SoundTarget::Move(name.to_owned())
    }
}

/// The sound server name for one choice: the name of a `pulse:` ID that the
/// server has now, else `DeviceNotFound`; for no choice the default when the
/// call started (`Unsupported` if it is not known). A default is not looked
/// up: it may be a monitor, which the lists leave out.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn pulse_target<'a>(
    choice: Option<&'a str>,
    start_default: Option<&'a str>,
    present: &[&str],
) -> Result<&'a str, FailureCode> {
    match choice {
        Some(id) => pulse_name(id)
            .filter(|name| present.contains(name))
            .ok_or(FailureCode::DeviceNotFound),
        None => start_default.ok_or(FailureCode::Unsupported),
    }
}

/// Plans a microphone and speaker change during a call on Linux (ADR-0049,
/// ADR-0059). Both choices are checked before either stream moves: an ID
/// that is not a `pulse:` ID, or a name the server lacks, is
/// `DeviceNotFound`. A kind that already is on its target is left alone. A
/// kind that must move but has no own stream yet is not a failure: its
/// target waits (`SoundTarget::Wait`) until a stream exists.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn plan_sound_move(
    recording: Option<&str>,
    playout: Option<&str>,
    known: &SoundKnown<'_>,
    now: &SoundNow<'_>,
) -> Result<SoundMove, FailureCode> {
    let source = pulse_target(recording, known.default_source, now.sources)?;
    let sink = pulse_target(playout, known.default_sink, now.sinks)?;
    Ok(SoundMove {
        source: sound_target(source, known.source, now.own_recording),
        sink: sound_target(sink, known.sink, now.own_playout),
    })
}

/// What the worker saw in the room, reduced to what it reports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RoomChange {
    /// A participant joined; `true` when it has a microphone that is not
    /// muted. No microphone counts as muted (ADR-0059): later microphone
    /// track events keep the mark current.
    Joined(u8, String, bool),
    Left(u8, String),
    /// The complete set of current speakers of one connection.
    Speakers(u8, Vec<String>),
    /// The room of this connection ended without a request from the core.
    /// The own SFU (0) ends the call; another SFU (1 to 3) only ends that
    /// connection (ADR-0054).
    Disconnected(u8),
}

/// Turns Speaking changes into at most [`MAX_SPEAKING_PER_SECOND`] messages
/// per second. A change that does not fit is kept and sent later, so the
/// core's view still ends up correct.
#[derive(Debug, Default)]
pub struct SpeakingTracker {
    reported: BTreeSet<(u8, String)>,
    wanted: BTreeSet<(u8, String)>,
    sent: VecDeque<Instant>,
}

impl SpeakingTracker {
    /// Replaces the speakers of one connection; the others stay.
    pub fn set_speakers(&mut self, connection: u8, identities: impl IntoIterator<Item = String>) {
        self.wanted.retain(|(known, _)| *known != connection);
        self.wanted.extend(
            identities
                .into_iter()
                .filter(|identity| valid_identity(identity))
                .take(MAX_PARTICIPANTS)
                .map(|identity| (connection, identity)),
        );
    }

    /// A participant left: nothing more is said about them.
    pub fn forget(&mut self, connection: u8, identity: &str) {
        let key = (connection, identity.to_owned());
        self.wanted.remove(&key);
        self.reported.remove(&key);
    }

    /// A connection ended: every speaker of it stops without a message
    /// (its participants are gone for the core as well).
    pub fn forget_connection(&mut self, connection: u8) {
        self.wanted.retain(|(known, _)| *known != connection);
        self.reported.retain(|(known, _)| *known != connection);
    }

    /// Returns the messages that may be sent at `now`. Stops come first.
    pub fn poll(&mut self, now: Instant) -> Vec<FromWorker> {
        while self
            .sent
            .front()
            .is_some_and(|&at| now.saturating_duration_since(at) >= WINDOW)
        {
            self.sent.pop_front();
        }
        let budget = MAX_SPEAKING_PER_SECOND.saturating_sub(self.sent.len());
        let stops = self.reported.difference(&self.wanted).cloned();
        let starts = self.wanted.difference(&self.reported).cloned();
        let due: Vec<((u8, String), bool)> = stops
            .map(|key| (key, false))
            .chain(starts.map(|key| (key, true)))
            .take(budget)
            .collect();
        let mut messages = Vec::with_capacity(due.len());
        for (key, speaking) in due {
            if speaking {
                self.reported.insert(key.clone());
            } else {
                self.reported.remove(&key);
            }
            self.sent.push_back(now);
            let (connection, identity) = key;
            messages.push(FromWorker::Speaking {
                connection,
                identity,
                speaking,
            });
        }
        messages
    }

    /// Whether a change is still waiting for budget.
    #[cfg(test)]
    pub fn has_pending(&self) -> bool {
        self.reported != self.wanted
    }
}

/// Applies one room change. Join and leave messages go out at once; speaking
/// changes wait for [`SpeakingTracker::poll`]. Identities that fail
/// [`valid_identity`] are ignored.
pub fn apply(change: RoomChange, tracker: &mut SpeakingTracker) -> Vec<FromWorker> {
    match change {
        RoomChange::Joined(connection, identity, microphone) if valid_identity(&identity) => {
            vec![
                FromWorker::ParticipantJoined {
                    connection,
                    identity: identity.clone(),
                },
                FromWorker::ParticipantMuted {
                    connection,
                    identity,
                    muted: !microphone,
                },
            ]
        }
        RoomChange::Left(connection, identity) if valid_identity(&identity) => {
            tracker.forget(connection, &identity);
            vec![FromWorker::ParticipantLeft {
                connection,
                identity,
            }]
        }
        RoomChange::Speakers(connection, identities) => {
            tracker.set_speakers(connection, identities);
            Vec::new()
        }
        // The own SFU ending ends the call. Another SFU ending only ends its
        // connection: the core marks those members not reachable.
        RoomChange::Disconnected(0) => vec![FromWorker::Failed {
            code: FailureCode::Network,
        }],
        RoomChange::Disconnected(connection) => {
            tracker.forget_connection(connection);
            vec![FromWorker::Connection {
                connection,
                up: false,
            }]
        }
        RoomChange::Joined(..) | RoomChange::Left(..) => Vec::new(),
    }
}

/// One connection to another SFU (ADR-0054): `C` is the connect in flight,
/// `U` the open connection.
#[derive(Debug, Eq, PartialEq)]
pub enum Slot<C, U> {
    Connecting(C),
    Up(U),
    /// Failed or ended. The index stays used: it is never reopened.
    Ended,
}

/// The connections to other SFUs by index, 1 to [`MAX_OTHER_CONNECTIONS`].
#[derive(Debug)]
pub struct Slots<C, U>(HashMap<u8, Slot<C, U>>);

impl<C, U> Default for Slots<C, U> {
    fn default() -> Self {
        Self(HashMap::new())
    }
}

impl<C, U> Slots<C, U> {
    /// Starts a connection at `index`. An index outside 1 to 3, or one used
    /// before (also after a failure), gives `connecting` back.
    pub fn begin(&mut self, index: u8, connecting: C) -> Result<(), C> {
        if !(1..=MAX_OTHER_CONNECTIONS).contains(&index) || self.0.contains_key(&index) {
            return Err(connecting);
        }
        self.0.insert(index, Slot::Connecting(connecting));
        Ok(())
    }

    pub fn is_connecting(&self, index: u8) -> bool {
        matches!(self.0.get(&index), Some(Slot::Connecting(_)))
    }

    /// The connect at `index` finished with an open connection.
    pub fn up(&mut self, index: u8, open: U) {
        self.0.insert(index, Slot::Up(open));
    }

    /// The connection at `index` failed or ended. Returns what it held.
    pub fn end(&mut self, index: u8) -> Option<Slot<C, U>> {
        self.0.insert(index, Slot::Ended)
    }

    /// The open connections, in no set order.
    pub fn open(&self) -> impl Iterator<Item = &U> {
        self.0.values().filter_map(|slot| match slot {
            Slot::Up(open) => Some(open),
            Slot::Connecting(_) | Slot::Ended => None,
        })
    }

    /// The open connections with their indexes, in no set order. The
    /// statistics probe names each remote entry by its connection.
    pub fn open_indexed(&self) -> impl Iterator<Item = (u8, &U)> {
        self.0.iter().filter_map(|(index, slot)| match slot {
            Slot::Up(open) => Some((*index, open)),
            Slot::Connecting(_) | Slot::Ended => None,
        })
    }

    /// Takes every connection out, for the end of the call.
    pub fn take_all(&mut self) -> Vec<Slot<C, U>> {
        self.0.drain().map(|(_, slot)| slot).collect()
    }
}

/// Words in a connection error that mean the TLS layer refused the server.
const TLS_WORDS: [&str; 6] = [
    "certificate",
    "tls",
    "rustls",
    "unknownissuer",
    "alert",
    "peer is incompatible",
];

/// The kind of failure a connection attempt had. The error text is read here
/// and never forwarded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectFault {
    /// The server answered with an HTTP status.
    Refused,
    /// A transport error with this text.
    Transport,
    Timeout,
    Other,
}

pub fn failure_for_connect(fault: ConnectFault, text: &str) -> FailureCode {
    match fault {
        ConnectFault::Refused => FailureCode::SignallingRefused,
        ConnectFault::Timeout => FailureCode::Network,
        ConnectFault::Transport => {
            let text = text.to_ascii_lowercase();
            if TLS_WORDS.iter().any(|word| text.contains(word)) {
                FailureCode::Tls
            } else {
                FailureCode::Network
            }
        }
        ConnectFault::Other => FailureCode::Internal,
    }
}

/// How every remote audio track plays (ADR-0055): the volume and whether
/// the user deafened the call. Each remote audio track gets it when it is
/// subscribed and again on every change.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Playout {
    pub deafened: bool,
    /// 0 to [`MAX_VOLUME_PERCENT`].
    pub volume: u8,
}

impl Playout {
    /// The playout at the start of a call: not deafened, at `volume`. A
    /// volume over the limit is cut to it (the protocol refuses one anyway).
    pub fn new(volume: u8) -> Self {
        Self {
            deafened: false,
            volume: volume.min(MAX_VOLUME_PERCENT),
        }
    }

    /// The gain WebRTC applies: 0.0 (silent) to 1.0 (as received).
    pub fn gain(&self) -> f64 {
        f64::from(self.volume.min(MAX_VOLUME_PERCENT)) / f64::from(MAX_VOLUME_PERCENT)
    }

    /// Whether remote audio tracks play at all.
    pub fn enabled(&self) -> bool {
        !self.deafened
    }
}

/// The microphone settings of a call (ADR-0062): the automatic level
/// (automatic gain control) and the manual gain, a linear factor on the
/// captured sound before any processing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Microphone {
    pub auto_level: bool,
    /// 0 to 400 percent in steps of 5, checked by [`Microphone::new`].
    pub gain_percent: u16,
}

impl Microphone {
    /// The settings, or `Protocol` for a gain over 400 percent or off the
    /// 5 percent step. The protocol reader refuses such a value too; this is
    /// the second check, where the value is used.
    pub fn new(auto_level: bool, gain_percent: u16) -> Result<Self, FailureCode> {
        if valid_mic_gain(gain_percent) {
            Ok(Self {
                auto_level,
                gain_percent,
            })
        } else {
            Err(FailureCode::Protocol)
        }
    }
}

/// The quietest sound the level meter shows, in dBFS (ADR-0055). Each of
/// the [`MAX_SOUND_LEVEL`] steps above it is 0.6 dB.
const SOUND_FLOOR_DB: f64 = -60.0;

/// The meter level of a linear sound level, as WebRTC's `audioLevel` gives
/// it (0.0 is silence, 1.0 is full scale): 0 at [`SOUND_FLOOR_DB`] or
/// quieter, [`MAX_SOUND_LEVEL`] at full scale. A value that is not a number
/// is silence.
pub fn sound_level(linear: f64) -> u8 {
    if linear.is_nan() || linear <= 0.0 {
        return 0;
    }
    let decibels = 20.0 * linear.min(1.0).log10();
    let steps = (decibels - SOUND_FLOOR_DB) / -SOUND_FLOOR_DB * f64::from(MAX_SOUND_LEVEL);
    // Clamped to 0..=MAX_SOUND_LEVEL first, so the cast cannot truncate.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let level = steps.round().clamp(0.0, f64::from(MAX_SOUND_LEVEL)) as u8;
    level
}

/// How often an unchanged sound level above silence is sent again. A page
/// that attaches during a call (a reload, or a call started before the
/// page) starts at silence and gets no earlier level, so steady sound would
/// otherwise show as silence there.
pub const SOUND_LEVEL_REPEAT: Duration = Duration::from_secs(1);

/// Lets a sound level through when it differs from the last one sent, and
/// an unchanged level above silence again after [`SOUND_LEVEL_REPEAT`]. It
/// starts at silence, so a call with no sound sends nothing.
#[derive(Debug, Default)]
pub struct LevelGate {
    last: u8,
    sent: Option<Instant>,
}

impl LevelGate {
    /// The level to send for `level` at `now`, or `None` when the core
    /// already has it and it is not yet due again.
    pub fn next(&mut self, level: u8, now: Instant) -> Option<u8> {
        let due = level > 0
            && self
                .sent
                .is_none_or(|at| now.saturating_duration_since(at) >= SOUND_LEVEL_REPEAT);
        if level == self.last && !due {
            return None;
        }
        self.last = level;
        self.sent = Some(now);
        Some(level)
    }
}

/// One measurement of the call's sound, linear (0.0 to 1.0, as WebRTC's
/// `audioLevel`): the own microphone as it is sent, and each remote
/// participant as it is played (ADR-0055, ADR-0059). `None` is a level
/// that was not read in time: its meter keeps the level it has, so a slow
/// read never shows as silence.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Levels {
    pub own: Option<f64>,
    /// Connection, identity and level. One identity may come more than
    /// once (one per audio track); the loudest counts, and one unread track
    /// makes the participant's level unread.
    pub remote: Vec<(u8, String, Option<f64>)>,
}

/// The gates of the own level and of each remote participant. It holds at
/// most [`MAX_PARTICIPANTS`] remote gates; a participant that is no longer
/// measured gets silence once and its gate goes.
#[derive(Debug, Default)]
pub struct LevelMeters {
    own: LevelGate,
    remote: HashMap<(u8, String), LevelGate>,
}

impl LevelMeters {
    /// The messages for one measurement at `now`: `SoundLevel` for the own
    /// microphone, then `ParticipantLevel` for each remote participant whose
    /// level the core does not have. An identity the protocol would refuse
    /// is left out.
    pub fn next(&mut self, levels: &Levels, now: Instant) -> Vec<FromWorker> {
        let mut out = Vec::new();
        if let Some(linear) = levels.own
            && let Some(level) = self.own.next(sound_level(linear), now)
        {
            out.push(FromWorker::SoundLevel { level });
        }
        let mut loudest: Vec<((u8, &str), Option<f64>)> = Vec::new();
        for (connection, identity, linear) in &levels.remote {
            if !valid_identity(identity) || *connection > MAX_OTHER_CONNECTIONS {
                continue;
            }
            let full = loudest.len() >= MAX_PARTICIPANTS;
            match loudest
                .iter_mut()
                .find(|(key, _)| *key == (*connection, identity.as_str()))
            {
                Some((_, level)) => *level = level.zip(*linear).map(|(a, b)| a.max(b)),
                None if !full => loudest.push(((*connection, identity), *linear)),
                None => {}
            }
        }
        for ((connection, identity), linear) in &loudest {
            let key = (*connection, (*identity).to_owned());
            if !self.remote.contains_key(&key) && self.remote.len() >= MAX_PARTICIPANTS {
                continue;
            }
            let gate = self.remote.entry(key).or_default();
            // Not read in time: the core keeps the level it has.
            let Some(linear) = linear else {
                continue;
            };
            if let Some(level) = gate.next(sound_level(*linear), now) {
                out.push(FromWorker::ParticipantLevel {
                    connection: *connection,
                    identity: (*identity).to_owned(),
                    level,
                });
            }
        }
        self.remote.retain(|(connection, identity), gate| {
            if loudest
                .iter()
                .any(|(key, _)| *key == (*connection, identity.as_str()))
            {
                return true;
            }
            if let Some(level) = gate.next(0, now) {
                out.push(FromWorker::ParticipantLevel {
                    connection: *connection,
                    identity: identity.clone(),
                    level,
                });
            }
            false
        });
        out
    }

    /// The own level alone, for a measurement without remote participants
    /// (the microphone test).
    pub fn own(&mut self, linear: f64, now: Instant) -> Option<FromWorker> {
        self.own
            .next(sound_level(linear), now)
            .map(|level| FromWorker::SoundLevel { level })
    }
}

/// The most remote audio tracks measured at once: two for each participant
/// (a microphone and a screen share's sound).
pub const MAX_MEASURED_TRACKS: usize = 2 * MAX_PARTICIPANTS;

/// The most remote audio tracks that [`AudioRoster`] keeps: the measured
/// ones and as many that wait. A track that waits is measured when a
/// measured one goes.
pub const MAX_KEPT_AUDIO_TRACKS: usize = 2 * MAX_MEASURED_TRACKS;

/// One remote audio track that [`AudioRoster`] keeps.
#[derive(Debug)]
struct KeptTrack<T> {
    connection: u8,
    identity: String,
    sid: String,
    track: T,
}

/// The subscribed remote audio tracks of every connection, in the order of
/// their subscriptions, at most [`MAX_KEPT_AUDIO_TRACKS`] (ADR-0059). The
/// room events change it: a subscription adds a track, and an
/// unsubscription, a participant that leaves or a connection that goes
/// removes tracks. The level meters read it on each tick, so no tick copies
/// the SDK's participant map. `T` is the track.
#[derive(Debug)]
pub struct AudioRoster<T> {
    tracks: Vec<KeptTrack<T>>,
}

impl<T> Default for AudioRoster<T> {
    fn default() -> Self {
        Self { tracks: Vec::new() }
    }
}

impl<T> AudioRoster<T> {
    /// Keeps a subscribed track and returns whether it is kept. The same
    /// track (connection, identity and SID) again replaces the kept track
    /// in its place. An identity that the protocol refuses, a connection
    /// out of range, or a full roster keeps nothing.
    pub fn add(&mut self, connection: u8, identity: String, sid: String, track: T) -> bool {
        if !valid_identity(&identity) || connection > MAX_OTHER_CONNECTIONS {
            return false;
        }
        if let Some(kept) = self.tracks.iter_mut().find(|kept| {
            kept.connection == connection && kept.identity == identity && kept.sid == sid
        }) {
            kept.track = track;
            return true;
        }
        if self.tracks.len() >= MAX_KEPT_AUDIO_TRACKS {
            return false;
        }
        self.tracks.push(KeptTrack {
            connection,
            identity,
            sid,
            track,
        });
        true
    }

    /// An unsubscribed track goes.
    pub fn remove(&mut self, connection: u8, identity: &str, sid: &str) {
        self.tracks.retain(|kept| {
            !(kept.connection == connection && kept.identity == identity && kept.sid == sid)
        });
    }

    /// Every track of a participant that left the connection goes.
    pub fn remove_participant(&mut self, connection: u8, identity: &str) {
        self.tracks
            .retain(|kept| !(kept.connection == connection && kept.identity == identity));
    }

    /// Every track of a connection that went down goes.
    pub fn remove_connection(&mut self, connection: u8) {
        self.tracks.retain(|kept| kept.connection != connection);
    }

    /// The tracks to measure: the first [`MAX_MEASURED_TRACKS`], each with
    /// its connection and identity.
    pub fn measured(&self) -> Vec<(u8, String, T)>
    where
        T: Clone,
    {
        self.tracks
            .iter()
            .take(MAX_MEASURED_TRACKS)
            .map(|kept| (kept.connection, kept.identity.clone(), kept.track.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use persephone_call_protocol::FrameKeyMsg;
    use zeroize::Zeroizing;

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    fn names(count: usize) -> Vec<String> {
        (0..count)
            .map(|index| format!("@u{index:02}:example.org:D"))
            .collect()
    }

    fn speaking(messages: &[FromWorker]) -> Vec<(String, bool)> {
        messages
            .iter()
            .map(|message| match message {
                FromWorker::Speaking {
                    identity, speaking, ..
                } => (identity.clone(), *speaking),
                other => panic!("not a Speaking message: {other:?}"),
            })
            .collect()
    }

    fn key() -> FrameKeyMsg {
        FrameKeyMsg {
            index: 1,
            bytes: Zeroizing::new(vec![1; 16]),
        }
    }

    #[test]
    fn given_join_message_when_it_is_first_then_it_is_allowed() {
        // GIVEN a Join, WHEN it is the first message, THEN the worker accepts it.
        let join = ToWorker::Join {
            sfu_url: "wss://sfu.example.org".to_owned(),
            jwt: Zeroizing::new("jwt".to_owned()),
            turn: Vec::new(),
            relay_only: false,
            own_identity: "@a:example.org:D".to_owned(),
            own_key: key(),
            recording_device: None,
            playout_device: None,
            camera_device: None,
            camera_on: false,
            volume: 100,
            auto_level: true,
            mic_gain: 100,
            video_effect: persephone_call_protocol::VideoEffect::NONE,
            video_quality: persephone_call_protocol::VideoQuality::Auto,
        };
        assert!(is_first_message_allowed(&join));
    }

    #[test]
    fn given_list_devices_when_it_is_first_then_it_is_allowed() {
        // GIVEN ListDevices, WHEN it is the first message, THEN it is accepted.
        assert!(is_first_message_allowed(&ToWorker::ListDevices));
    }

    #[test]
    fn given_test_device_when_it_is_first_then_it_is_allowed() {
        // GIVEN TestDevice, WHEN it is the first message, THEN it is accepted.
        assert!(is_first_message_allowed(&ToWorker::TestDevice {
            test: persephone_call_protocol::DeviceTest::Speaker,
            device: None,
            video_effect: persephone_call_protocol::VideoEffect::NONE,
        }));
    }

    #[test]
    fn given_device_ids_when_checked_then_index_ids_are_not_listable() {
        // GIVEN a GUID and index IDs, WHEN checked, THEN only the GUID passes.
        assert!(is_listable_device_id("{0.0.1.00000000}.{6a2c}"));
        assert!(!is_listable_device_id("index:0"));
        assert!(!is_listable_device_id("index:"));
    }

    #[test]
    fn given_no_wanted_device_when_chosen_then_system_default_is_kept() {
        // GIVEN no wanted ID, WHEN chosen, THEN nothing is selected.
        assert_eq!(choose_device(None, ["a", "b"]), Ok(None));
        assert_eq!(choose_device(None, []), Ok(None));
    }

    #[test]
    fn given_wanted_device_when_present_then_it_is_chosen() {
        // GIVEN a wanted ID in the list, WHEN chosen, THEN it is returned.
        assert_eq!(
            choose_device(Some("b"), ["a", "b"]),
            Ok(Some("b".to_owned()))
        );
    }

    #[test]
    fn given_wanted_device_when_absent_or_inexact_then_it_is_not_found() {
        // GIVEN absent and near-miss IDs, WHEN chosen, THEN DeviceNotFound.
        assert_eq!(
            choose_device(Some("c"), ["a", "b"]),
            Err(FailureCode::DeviceNotFound)
        );
        assert_eq!(
            choose_device(Some("A"), ["a"]),
            Err(FailureCode::DeviceNotFound)
        );
        assert_eq!(
            choose_device(Some("a"), []),
            Err(FailureCode::DeviceNotFound)
        );
    }

    #[test]
    fn given_no_choice_when_the_system_default_is_listed_then_it_is_opened() {
        // GIVEN the Windows console default speaker among the listed ones
        // (the default communications device is another one), WHEN no
        // speaker is chosen, THEN the console default is the one opened.
        let console = "{0.0.0.00000000}.{00000000-0000-4000-8000-000000000001}";
        let mixer = "{0.0.0.00000000}.{00000000-0000-4000-8000-000000000002}";
        assert_eq!(
            system_default(Some(console), [mixer, console]),
            Some(console.to_owned())
        );
    }

    #[test]
    fn given_no_usable_system_default_then_libwebrtc_keeps_its_choice() {
        // GIVEN no default, a default that is not listed, or an unstable
        // index ID, WHEN no device is chosen, THEN nothing is selected.
        assert_eq!(system_default(None, ["a"]), None);
        assert_eq!(system_default(Some("b"), ["a"]), None);
        assert_eq!(system_default(Some("A"), ["a"]), None);
        assert_eq!(system_default(Some("index:0"), ["index:0"]), None);
    }

    #[test]
    fn given_index_id_when_chosen_then_it_never_matches() {
        // GIVEN an index ID that is also available, WHEN chosen, THEN refused.
        assert_eq!(
            choose_device(Some("index:1"), ["index:1"]),
            Err(FailureCode::DeviceNotFound)
        );
    }

    #[test]
    fn given_mixed_devices_when_listed_then_bad_and_repeated_ones_are_dropped() {
        // GIVEN an index ID, an empty ID, a control ID and a repeat,
        // WHEN the list is built, THEN only the first good ones stay.
        let list = bounded_device_list([
            ("index:0", "Unstable"),
            ("", "No id"),
            ("bad\u{7}id", "Control id"),
            ("a", "First\u{7}"),
            ("b", "Second"),
            ("a", "Repeat"),
        ]);
        assert_eq!(
            list,
            vec![
                MediaDevice {
                    id: "a".into(),
                    name: "First".into()
                },
                MediaDevice {
                    id: "b".into(),
                    name: "Second".into()
                },
            ]
        );
    }

    #[test]
    fn given_too_many_devices_when_listed_then_the_list_is_cut() {
        // GIVEN more devices than the limit, WHEN listed, THEN MAX_DEVICES stay.
        let ids: Vec<String> = (0..MAX_DEVICES + 5).map(|n| format!("id{n}")).collect();
        let list = bounded_device_list(ids.iter().map(|id| (id.as_str(), "n")));
        assert_eq!(list.len(), MAX_DEVICES);
        assert_eq!(list[0].id, "id0");
    }

    #[test]
    fn given_sound_server_names_when_listed_then_only_safe_names_get_pulse_ids() {
        // GIVEN good names, an unsafe name, an empty name and a repeat,
        // WHEN listed, THEN each good name is a pulse: ID once, in order.
        let list = pulse_device_list([
            ("alsa_input.usb-Mic_01-00.analog-stereo", "USB Mic"),
            ("bad name", "Space"),
            ("", "Empty"),
            ("x;rm", "Shell"),
            ("vmic", "Virtual\u{7}"),
            ("vmic", "Repeat"),
        ]);
        assert_eq!(
            list,
            vec![
                MediaDevice {
                    id: "pulse:alsa_input.usb-Mic_01-00.analog-stereo".into(),
                    name: "USB Mic".into()
                },
                MediaDevice {
                    id: "pulse:vmic".into(),
                    name: "Virtual".into()
                },
            ]
        );
        let names: Vec<String> = (0..MAX_DEVICES + 5).map(|n| format!("s{n}")).collect();
        let list = pulse_device_list(names.iter().map(|name| (name.as_str(), "n")));
        assert_eq!(list.len(), MAX_DEVICES);
    }

    #[test]
    fn given_a_pulse_choice_when_the_launch_and_server_agree_then_the_name_is_used() {
        // GIVEN a pulse: choice, the same name in the environment and on the
        // server, WHEN checked, THEN the default stream goes to that name.
        assert_eq!(
            sound_path(Some("pulse:vmic"), Some("vmic"), Some(&["other", "vmic"])),
            Ok(SoundPath::ServerName)
        );
    }

    #[test]
    fn given_a_pulse_choice_when_the_device_is_gone_then_it_is_not_found() {
        // GIVEN a pulse: choice that the server does not have, or a server
        // that could not be asked, WHEN checked, THEN DeviceNotFound (never
        // the server's quiet fallback to its default).
        assert_eq!(
            sound_path(Some("pulse:vmic"), Some("vmic"), Some(&["other"])),
            Err(FailureCode::DeviceNotFound)
        );
        assert_eq!(
            sound_path(Some("pulse:vmic"), Some("vmic"), Some(&[])),
            Err(FailureCode::DeviceNotFound)
        );
        assert_eq!(
            sound_path(Some("pulse:vmic"), Some("vmic"), None),
            Err(FailureCode::DeviceNotFound)
        );
    }

    #[test]
    fn given_a_choice_and_launch_that_disagree_then_it_is_a_protocol_failure() {
        // GIVEN a pulse: choice with no or another name in the environment,
        // or a non-pulse choice with a name there, WHEN checked, THEN
        // Protocol: the worker never opens a device nobody chose.
        let present: &[&str] = &["vmic", "other"];
        assert_eq!(
            sound_path(Some("pulse:vmic"), None, Some(present)),
            Err(FailureCode::Protocol)
        );
        assert_eq!(
            sound_path(Some("pulse:vmic"), Some("other"), Some(present)),
            Err(FailureCode::Protocol)
        );
        assert_eq!(
            sound_path(None, Some("vmic"), Some(present)),
            Err(FailureCode::Protocol)
        );
        assert_eq!(
            sound_path(Some("{guid}"), Some("vmic"), Some(present)),
            Err(FailureCode::Protocol)
        );
    }

    #[test]
    fn given_no_pulse_choice_and_no_launch_name_then_the_listed_path_is_kept() {
        // GIVEN no choice, a GUID choice or an unsafe pulse: ID, and no name
        // in the environment, WHEN checked, THEN libwebrtc's list decides
        // (an unsafe pulse: ID is then not found there).
        assert_eq!(sound_path(None, None, None), Ok(SoundPath::Listed));
        assert_eq!(
            sound_path(Some("{guid}"), None, None),
            Ok(SoundPath::Listed)
        );
        assert_eq!(
            sound_path(Some("pulse:bad name"), None, None),
            Ok(SoundPath::Listed)
        );
        assert_eq!(
            choose_device(Some("pulse:bad name"), ["{guid}"]),
            Err(FailureCode::DeviceNotFound)
        );
    }

    #[test]
    fn given_a_stream_listing_when_own_streams_are_picked_then_only_this_process_counts() {
        // GIVEN streams of this process, of another one and of none, WHEN
        // picked, THEN only the own indexes stay, in order.
        let listing = [(3, Some(100)), (4, Some(200)), (5, None), (9, Some(100))];
        assert_eq!(own_streams(listing, 100), vec![3, 9]);
        assert_eq!(own_streams(listing, 300), Vec::<u32>::new());
        assert_eq!(own_streams([(1, None)], 0), Vec::<u32>::new());
    }

    #[test]
    fn given_many_own_streams_then_at_most_the_limit_is_kept() {
        // GIVEN more own streams than the limit, WHEN picked, THEN the rest
        // are dropped.
        let listing = (0..MAX_OWN_STREAMS as u32 + 5).map(|index| (index, Some(7)));
        assert_eq!(own_streams(listing, 7).len(), MAX_OWN_STREAMS);
    }

    const NOW: SoundNow<'static> = SoundNow {
        sources: &["mic-a", "mic-b"],
        sinks: &["spk-a", "spk-b"],
        own_recording: 1,
        own_playout: 1,
    };

    fn known() -> SoundKnown<'static> {
        SoundKnown {
            default_source: Some("mic-a.monitor"),
            default_sink: Some("spk-a"),
            source: Some("mic-a"),
            sink: Some("spk-a"),
        }
    }

    #[test]
    fn given_pulse_choices_when_planned_then_each_kind_moves_to_its_name() {
        // GIVEN a new microphone and a new speaker that the server has,
        // WHEN planned, THEN both names are to be moved to.
        assert_eq!(
            plan_sound_move(Some("pulse:mic-b"), Some("pulse:spk-b"), &known(), &NOW),
            Ok(SoundMove {
                source: SoundTarget::Move("mic-b".to_owned()),
                sink: SoundTarget::Move("spk-b".to_owned()),
            })
        );
    }

    #[test]
    fn given_no_choice_when_planned_then_the_default_at_call_start_is_used() {
        // GIVEN the call on other devices than the defaults it started with
        // and no choice ("System default"), WHEN planned, THEN both go to the
        // start defaults, even a default that is a monitor the lists omit.
        let mut known = known();
        known.source = Some("mic-b");
        known.sink = Some("spk-b");
        assert_eq!(
            plan_sound_move(None, None, &known, &NOW),
            Ok(SoundMove {
                source: SoundTarget::Move("mic-a.monitor".to_owned()),
                sink: SoundTarget::Move("spk-a".to_owned()),
            })
        );
        // WHEN the start default is not known, THEN the change is refused.
        known.default_sink = None;
        assert_eq!(
            plan_sound_move(None, None, &known, &NOW),
            Err(FailureCode::Unsupported)
        );
    }

    #[test]
    fn given_the_same_devices_when_planned_then_nothing_moves_and_no_stream_is_needed() {
        // GIVEN choices equal to what the call uses and no own streams,
        // WHEN planned, THEN nothing moves and nothing fails.
        let now = SoundNow {
            own_recording: 0,
            own_playout: 0,
            ..NOW
        };
        assert_eq!(
            plan_sound_move(Some("pulse:mic-a"), None, &known(), &now),
            Ok(SoundMove::default())
        );
    }

    #[test]
    fn given_a_bad_choice_when_planned_then_nothing_is_planned_for_either_kind() {
        // GIVEN a good microphone and a speaker that is not a pulse: ID, one
        // the server lacks, or an unsafe name, WHEN planned, THEN the whole
        // change is DeviceNotFound, so the good microphone does not move.
        for bad in [
            "{guid}",
            "index:0",
            "pulse:gone",
            "pulse:bad name",
            "pulse:",
        ] {
            assert_eq!(
                plan_sound_move(Some("pulse:mic-b"), Some(bad), &known(), &NOW),
                Err(FailureCode::DeviceNotFound),
                "{bad}"
            );
        }
        assert_eq!(
            plan_sound_move(Some("{guid}"), None, &known(), &NOW),
            Err(FailureCode::DeviceNotFound)
        );
    }

    #[test]
    fn given_no_own_stream_when_a_kind_must_move_then_its_target_is_pending() {
        // GIVEN no own playback stream (nobody else is in the call) and a new
        // speaker, WHEN planned, THEN the speaker waits and is not an error,
        // and the microphone still moves now.
        let now = SoundNow {
            own_playout: 0,
            ..NOW
        };
        assert_eq!(
            plan_sound_move(Some("pulse:mic-b"), Some("pulse:spk-b"), &known(), &now),
            Ok(SoundMove {
                source: SoundTarget::Move("mic-b".to_owned()),
                sink: SoundTarget::Wait("spk-b".to_owned()),
            })
        );
    }

    #[test]
    fn given_a_pending_target_when_a_newer_choice_comes_then_it_replaces_it() {
        // GIVEN a speaker that waits, WHEN a newer choice is planned, THEN
        // the plan names only the newer one, so the stored name is replaced.
        let now = SoundNow {
            own_playout: 0,
            ..NOW
        };
        let wait = |name: &str| {
            plan_sound_move(None, Some(name), &known(), &now)
                .unwrap()
                .sink
        };
        assert_eq!(wait("pulse:spk-b"), SoundTarget::Wait("spk-b".to_owned()));
        let newer = SoundNow {
            sinks: &["spk-a", "spk-b", "spk-c"],
            ..now
        };
        assert_eq!(
            plan_sound_move(None, Some("pulse:spk-c"), &known(), &newer)
                .unwrap()
                .sink,
            SoundTarget::Wait("spk-c".to_owned())
        );
    }

    #[test]
    fn given_a_pending_target_when_the_device_in_use_is_chosen_then_it_is_cleared() {
        // GIVEN a call on spk-a with no playback stream, WHEN spk-a is chosen
        // again, THEN the plan is Keep, which clears a waiting target.
        let now = SoundNow {
            own_playout: 0,
            ..NOW
        };
        assert_eq!(
            plan_sound_move(None, Some("pulse:spk-a"), &known(), &now)
                .unwrap()
                .sink,
            SoundTarget::Keep
        );
    }

    #[test]
    fn given_a_stream_when_a_pending_kind_is_planned_again_then_it_moves() {
        // GIVEN a waiting speaker and then a playback stream, WHEN the same
        // choice is planned, THEN it moves now.
        assert_eq!(
            plan_sound_move(None, Some("pulse:spk-b"), &known(), &NOW)
                .unwrap()
                .sink,
            SoundTarget::Move("spk-b".to_owned())
        );
    }

    #[test]
    fn given_any_other_message_when_it_is_first_then_it_is_refused() {
        // GIVEN each non-Join message, WHEN it comes first, THEN it is refused.
        for message in [
            ToWorker::Leave,
            ToWorker::SetMuted { muted: true },
            ToWorker::SetDeafened { deafened: true },
            ToWorker::SetVolume { percent: 50 },
            ToWorker::SetCamera {
                on: true,
                remote: false,
            },
            ToWorker::ScreenFrames {
                share: 1,
                handle: 4,
            },
            ToWorker::SetScreen {
                target: None,
                share: 1,
            },
            ToWorker::SetScreen {
                target: Some(persephone_call_protocol::ScreenTarget::Portal { node: 7 }),
                share: 2,
            },
            ToWorker::SetOwnKey { key: key() },
            ToWorker::SetKey {
                identity: "@a:example.org:D".to_owned(),
                key: key(),
            },
            ToWorker::RemoveParticipant {
                identity: "@a:example.org:D".to_owned(),
            },
            ToWorker::Subscribe {
                index: 1,
                url: "wss://sfu.other.example".to_owned(),
                jwt: Zeroizing::new("a.b.c".to_owned()),
            },
            ToWorker::SetDevices {
                recording: None,
                playout: None,
                camera: None,
            },
        ] {
            assert!(!is_first_message_allowed(&message), "{message:?}");
        }
    }

    #[test]
    fn given_identities_when_checked_then_only_protocol_valid_ones_pass() {
        // GIVEN empty, long, control-character and normal identities,
        // WHEN checked, THEN only the normal and the longest allowed pass.
        assert!(valid_identity("@alice:example.org:DEVICE"));
        assert!(valid_identity(&"a".repeat(MAX_IDENTITY_BYTES)));
        assert!(!valid_identity(""));
        assert!(!valid_identity(&"a".repeat(MAX_IDENTITY_BYTES + 1)));
        assert!(!valid_identity("bad\nname"));
    }

    #[test]
    fn given_participant_events_when_applied_then_messages_follow_the_identity_rules() {
        // GIVEN a tracker, WHEN a valid join and leave and an invalid join arrive,
        // THEN the valid ones map to messages and the invalid one is ignored.
        let mut tracker = SpeakingTracker::default();
        assert_eq!(
            apply(
                RoomChange::Joined(0, "@a:example.org:D".into(), true),
                &mut tracker
            ),
            vec![
                FromWorker::ParticipantJoined {
                    connection: 0,
                    identity: "@a:example.org:D".into()
                },
                FromWorker::ParticipantMuted {
                    connection: 0,
                    identity: "@a:example.org:D".into(),
                    muted: false
                }
            ]
        );
        // A participant with no microphone, or a muted one, shows as muted.
        assert_eq!(
            apply(
                RoomChange::Joined(0, "@b:example.org:D".into(), false),
                &mut tracker
            )[1],
            FromWorker::ParticipantMuted {
                connection: 0,
                identity: "@b:example.org:D".into(),
                muted: true
            }
        );
        assert_eq!(
            apply(RoomChange::Left(0, "@a:example.org:D".into()), &mut tracker),
            vec![FromWorker::ParticipantLeft {
                connection: 0,
                identity: "@a:example.org:D".into()
            }]
        );
        assert!(
            apply(
                RoomChange::Joined(0, "bad\u{7}".into(), false),
                &mut tracker
            )
            .is_empty()
        );
    }

    #[test]
    fn given_participants_of_other_connections_when_applied_then_messages_carry_the_index() {
        // GIVEN a tracker, WHEN a join and a leave arrive on connection 2,
        // THEN each message names connection 2.
        let mut tracker = SpeakingTracker::default();
        assert_eq!(
            apply(
                RoomChange::Joined(2, "@c:other.example:D".into(), true),
                &mut tracker
            ),
            vec![
                FromWorker::ParticipantJoined {
                    connection: 2,
                    identity: "@c:other.example:D".into()
                },
                FromWorker::ParticipantMuted {
                    connection: 2,
                    identity: "@c:other.example:D".into(),
                    muted: false
                }
            ]
        );
        assert_eq!(
            apply(
                RoomChange::Left(2, "@c:other.example:D".into()),
                &mut tracker
            ),
            vec![FromWorker::ParticipantLeft {
                connection: 2,
                identity: "@c:other.example:D".into()
            }]
        );
    }

    #[test]
    fn given_another_sfu_disconnects_then_only_its_connection_ends() {
        // GIVEN speakers on connections 0 and 1, WHEN connection 1 disconnects,
        // THEN the message is Connection down (not a failure of the call) and
        // the speakers of connection 0 stay.
        let start = Instant::now();
        let mut tracker = SpeakingTracker::default();
        tracker.set_speakers(0, names(1));
        tracker.set_speakers(1, names(2));
        assert_eq!(tracker.poll(start).len(), 3);
        assert_eq!(
            apply(RoomChange::Disconnected(1), &mut tracker),
            vec![FromWorker::Connection {
                connection: 1,
                up: false
            }]
        );
        tracker.set_speakers(0, names(1));
        assert!(tracker.poll(at(start, 2000)).is_empty());
        assert!(!tracker.has_pending());
        // WHEN the own SFU disconnects, THEN the call fails with Network.
        assert_eq!(
            apply(RoomChange::Disconnected(0), &mut tracker),
            vec![FromWorker::Failed {
                code: FailureCode::Network
            }]
        );
    }

    #[test]
    fn given_speakers_of_two_connections_then_each_set_is_kept_apart() {
        // GIVEN speakers on connection 0, WHEN connection 2 reports its own
        // set, THEN connection 0's speakers are not replaced, and each
        // Speaking message names its connection.
        let start = Instant::now();
        let mut tracker = SpeakingTracker::default();
        tracker.set_speakers(0, vec!["@a:x:D".into()]);
        tracker.set_speakers(2, vec!["@b:y:D".into()]);
        let sent = tracker.poll(start);
        assert!(sent.contains(&FromWorker::Speaking {
            connection: 0,
            identity: "@a:x:D".into(),
            speaking: true
        }));
        assert!(sent.contains(&FromWorker::Speaking {
            connection: 2,
            identity: "@b:y:D".into(),
            speaking: true
        }));
        tracker.set_speakers(2, Vec::new());
        let sent = tracker.poll(at(start, 2000));
        assert_eq!(
            sent,
            vec![FromWorker::Speaking {
                connection: 2,
                identity: "@b:y:D".into(),
                speaking: false
            }]
        );
    }

    #[test]
    fn given_connection_slots_then_an_index_opens_once_and_only_1_to_3() {
        // GIVEN empty slots, WHEN indices 0 and 4 are begun, THEN they are
        // refused; WHEN 1 is begun twice, THEN the second is refused, also
        // after the first failed.
        let mut slots: Slots<&str, &str> = Slots::default();
        assert_eq!(slots.begin(0, "c"), Err("c"));
        assert_eq!(slots.begin(4, "c"), Err("c"));
        assert_eq!(slots.begin(1, "c"), Ok(()));
        assert!(slots.is_connecting(1));
        assert_eq!(slots.begin(1, "d"), Err("d"));
        assert_eq!(slots.end(1), Some(Slot::Connecting("c")));
        assert_eq!(slots.begin(1, "e"), Err("e"));
        assert!(!slots.is_connecting(1));
    }

    #[test]
    fn given_open_and_pending_connections_when_the_call_ends_then_all_are_taken() {
        // GIVEN one open, one pending and one failed connection, WHEN the
        // call ends, THEN every one is handed over to be closed or stopped.
        let mut slots: Slots<&str, &str> = Slots::default();
        slots.begin(1, "c1").unwrap();
        slots.up(1, "u1");
        slots.begin(2, "c2").unwrap();
        slots.begin(3, "c3").unwrap();
        slots.end(3);
        let mut all = slots.take_all();
        all.sort_by_key(|slot| format!("{slot:?}"));
        assert_eq!(
            all,
            vec![Slot::Connecting("c2"), Slot::Ended, Slot::Up("u1")]
        );
        assert!(slots.take_all().is_empty());
    }

    #[test]
    fn given_mixed_connections_then_only_open_ones_get_the_playout() {
        // GIVEN one open, one pending and one failed connection, WHEN the
        // open ones are listed for the call's playout (ADR-0055) or with
        // their indexes for the statistics (ADR-0066), THEN only the open
        // one is listed, with its index, and the slots are unchanged.
        let mut slots: Slots<&str, &str> = Slots::default();
        slots.begin(1, "c1").unwrap();
        slots.up(1, "u1");
        slots.begin(2, "c2").unwrap();
        slots.begin(3, "c3").unwrap();
        slots.end(3);
        assert_eq!(slots.open().copied().collect::<Vec<_>>(), vec!["u1"]);
        assert_eq!(
            slots
                .open_indexed()
                .map(|(index, open)| (index, *open))
                .collect::<Vec<_>>(),
            vec![(1, "u1")]
        );
        assert!(slots.is_connecting(2));
    }

    #[test]
    fn given_room_disconnect_when_applied_then_the_call_fails_with_network() {
        // GIVEN a room that ends by itself, WHEN applied, THEN Failed(Network) results.
        let mut tracker = SpeakingTracker::default();
        assert_eq!(
            apply(RoomChange::Disconnected(0), &mut tracker),
            vec![FromWorker::Failed {
                code: FailureCode::Network
            }]
        );
    }

    #[test]
    fn given_speaker_set_when_it_changes_then_only_changes_are_reported() {
        // GIVEN two speakers reported, WHEN one stops and one starts,
        // THEN exactly one stop and one start follow, stop first.
        let start = Instant::now();
        let mut tracker = SpeakingTracker::default();
        let ids = names(3);
        tracker.set_speakers(0, ids[..2].to_vec());
        assert_eq!(speaking(&tracker.poll(start)).len(), 2);
        assert!(tracker.poll(at(start, 10)).is_empty());
        tracker.set_speakers(0, vec![ids[1].clone(), ids[2].clone()]);
        assert_eq!(
            speaking(&tracker.poll(at(start, 20))),
            vec![(ids[0].clone(), false), (ids[2].clone(), true)]
        );
    }

    #[test]
    fn given_many_speakers_when_polled_then_at_most_ten_go_out_per_second() {
        // GIVEN 25 speakers at once, WHEN polled over time,
        // THEN each second carries at most 10 messages and all arrive in the end.
        let start = Instant::now();
        let mut tracker = SpeakingTracker::default();
        tracker.set_speakers(0, names(25));
        assert_eq!(tracker.poll(start).len(), 10);
        assert!(tracker.has_pending());
        assert!(tracker.poll(at(start, 500)).is_empty());
        assert_eq!(tracker.poll(at(start, 1000)).len(), 10);
        assert_eq!(tracker.poll(at(start, 2000)).len(), 5);
        assert!(!tracker.has_pending());
    }

    #[test]
    fn given_a_leaving_participant_when_forgotten_then_no_stop_is_sent() {
        // GIVEN a reported speaker, WHEN they leave, THEN no Speaking message follows.
        let start = Instant::now();
        let mut tracker = SpeakingTracker::default();
        tracker.set_speakers(0, names(1));
        tracker.poll(start);
        apply(RoomChange::Left(0, names(1).remove(0)), &mut tracker);
        tracker.set_speakers(0, Vec::new());
        assert!(tracker.poll(at(start, 2000)).is_empty());
    }

    #[test]
    fn given_invalid_or_too_many_speakers_when_set_then_they_are_dropped() {
        // GIVEN an invalid identity and more than 32 speakers,
        // WHEN set, THEN the invalid one is dropped and the set is capped.
        let start = Instant::now();
        let mut tracker = SpeakingTracker::default();
        let mut ids = names(40);
        ids.push("bad\u{1}".to_owned());
        tracker.set_speakers(0, ids);
        let mut total = 0;
        for second in 0..5 {
            let messages = tracker.poll(start + Duration::from_secs(second * 2));
            assert!(
                speaking(&messages)
                    .iter()
                    .all(|(identity, _)| valid_identity(identity))
            );
            total += messages.len();
        }
        assert_eq!(total, MAX_PARTICIPANTS);
    }

    #[test]
    fn given_connect_faults_when_mapped_then_codes_match_without_text() {
        // GIVEN each kind of fault, WHEN mapped, THEN the closed code follows.
        use ConnectFault::*;
        assert_eq!(
            failure_for_connect(Refused, ""),
            FailureCode::SignallingRefused
        );
        assert_eq!(failure_for_connect(Timeout, ""), FailureCode::Network);
        assert_eq!(failure_for_connect(Other, ""), FailureCode::Internal);
        assert_eq!(
            failure_for_connect(Transport, "connection refused"),
            FailureCode::Network
        );
        assert_eq!(
            failure_for_connect(Transport, "invalid peer certificate: UnknownIssuer"),
            FailureCode::Tls
        );
        assert_eq!(
            failure_for_connect(Transport, "received fatal alert: ProtocolVersion"),
            FailureCode::Tls
        );
    }

    /// GIVEN microphone gains inside and outside 0 to 400 percent and off the
    /// 5 percent step
    /// WHEN the settings are made
    /// THEN a valid gain is kept with the automatic level flag, and any other
    /// gain is refused as a protocol fault.
    #[test]
    fn given_microphone_gain_when_made_then_only_valid_steps_pass() {
        for gain in [0, 5, 100, 395, 400] {
            assert_eq!(
                Microphone::new(true, gain),
                Ok(Microphone {
                    auto_level: true,
                    gain_percent: gain
                })
            );
        }
        for gain in [1, 99, 401, 405, 1000, u16::MAX] {
            assert_eq!(Microphone::new(false, gain), Err(FailureCode::Protocol));
        }
    }

    /// GIVEN a call's playout at 0, 50, 100 and an over-limit volume
    /// WHEN the gain is read
    /// THEN it runs from 0.0 to 1.0, and an over-limit volume is cut to 1.0.
    #[test]
    fn given_volume_when_gain_is_read_then_it_is_between_zero_and_one() {
        assert_eq!(Playout::new(0).gain(), 0.0);
        assert_eq!(Playout::new(50).gain(), 0.5);
        assert_eq!(Playout::new(100).gain(), 1.0);
        let over = Playout::new(200);
        assert_eq!(over.volume, MAX_VOLUME_PERCENT);
        assert_eq!(over.gain(), 1.0);
    }

    /// GIVEN a new call WHEN it is deafened and undeafened THEN tracks are
    /// disabled only while deafened, and the volume is kept.
    #[test]
    fn given_deafen_when_toggled_then_tracks_follow_and_volume_stays() {
        let mut playout = Playout::new(40);
        assert!(playout.enabled());
        playout.deafened = true;
        assert!(!playout.enabled());
        assert_eq!(playout.volume, 40);
        playout.deafened = false;
        assert!(playout.enabled());
    }

    /// GIVEN WebRTC audio levels from silence to full scale, beyond it and
    /// not a number
    /// WHEN they are put on the meter's decibel scale
    /// THEN -60 dBFS and quieter is 0, full scale is the top, each 6 dB is
    /// ten steps, and bad values are silence.
    #[test]
    fn given_linear_levels_when_mapped_then_the_scale_is_decibels() {
        assert_eq!(sound_level(0.0), 0);
        assert_eq!(sound_level(-0.5), 0);
        assert_eq!(sound_level(f64::NAN), 0);
        assert_eq!(sound_level(0.000_1), 0);
        assert_eq!(sound_level(0.001), 0);
        assert_eq!(sound_level(0.01), 33);
        assert_eq!(sound_level(0.1), 67);
        assert_eq!(sound_level(0.5), 90);
        assert_eq!(sound_level(1.0), MAX_SOUND_LEVEL);
        assert_eq!(sound_level(4.0), MAX_SOUND_LEVEL);
        assert_eq!(sound_level(f64::INFINITY), MAX_SOUND_LEVEL);
    }

    /// GIVEN a run of levels with repeats within a second
    /// WHEN each goes through the gate
    /// THEN only changes are sent, and a call that stays silent sends none.
    #[test]
    fn given_levels_when_gated_then_only_changes_are_sent() {
        let mut gate = LevelGate::default();
        let start = Instant::now();
        let sent: Vec<u8> = [0, 0, 40, 40, 41, 0, 0]
            .into_iter()
            .zip(0..)
            .filter_map(|(level, tick)| gate.next(level, at(start, tick * 100)))
            .collect();
        assert_eq!(sent, [40, 41, 0]);
    }

    /// GIVEN a steady level above silence WHEN it stays the same THEN it is
    /// sent again once a second, so a page that attached later learns it;
    /// GIVEN steady silence THEN it is never sent again.
    #[test]
    fn given_a_steady_level_when_gated_then_it_is_repeated_each_second() {
        let mut gate = LevelGate::default();
        let start = Instant::now();
        let repeat = u64::try_from(SOUND_LEVEL_REPEAT.as_millis()).expect("short");
        assert!(gate.next(60, start).is_some());
        assert!(gate.next(60, at(start, repeat - 1)).is_none());
        assert_eq!(gate.next(60, at(start, repeat)), Some(60));
        assert!(gate.next(60, at(start, repeat + 100)).is_none());
        assert!(gate.next(0, at(start, repeat + 200)).is_some());
        assert!(gate.next(0, at(start, 10 * repeat)).is_none());
    }

    fn remote(connection: u8, identity: &str, linear: f64) -> (u8, String, Option<f64>) {
        (connection, identity.to_owned(), Some(linear))
    }

    /// GIVEN the own microphone and two remote participants, one with two
    /// tracks WHEN measured THEN the own level is `SoundLevel`, each remote
    /// one a `ParticipantLevel` of its loudest track, and an unchanged
    /// measurement sends nothing.
    #[test]
    fn given_own_and_remote_levels_then_each_has_its_own_message() {
        let mut meters = LevelMeters::default();
        let start = Instant::now();
        let levels = Levels {
            own: Some(0.5),
            remote: vec![
                remote(0, "@a:example.org:A", 0.1),
                remote(0, "@a:example.org:A", 1.0),
                remote(1, "@b:example.org:B", 0.5),
            ],
        };
        let sent = meters.next(&levels, start);
        assert_eq!(
            sent,
            [
                FromWorker::SoundLevel { level: 90 },
                FromWorker::ParticipantLevel {
                    connection: 0,
                    identity: "@a:example.org:A".to_owned(),
                    level: MAX_SOUND_LEVEL,
                },
                FromWorker::ParticipantLevel {
                    connection: 1,
                    identity: "@b:example.org:B".to_owned(),
                    level: 90,
                },
            ]
        );
        assert!(meters.next(&levels, at(start, 100)).is_empty());
    }

    /// GIVEN levels that were sent WHEN the next measurement could not read
    /// the own level or a participant's track in time THEN nothing is sent
    /// for them, so a slow read never shows as silence, and the participant
    /// is still measured: a later read level is sent as a change.
    #[test]
    fn given_an_unread_level_then_the_meter_keeps_its_level() {
        let mut meters = LevelMeters::default();
        let start = Instant::now();
        let heard = Levels {
            own: Some(0.5),
            remote: vec![remote(0, "@a:example.org:A", 0.5)],
        };
        assert_eq!(meters.next(&heard, start).len(), 2);
        let unread = Levels {
            own: None,
            remote: vec![
                (0, "@a:example.org:A".to_owned(), None),
                remote(0, "@a:example.org:A", 1.0),
            ],
        };
        assert!(meters.next(&unread, at(start, 100)).is_empty());
        let quiet = Levels {
            own: Some(0.0),
            remote: vec![remote(0, "@a:example.org:A", 0.0)],
        };
        assert_eq!(
            meters.next(&quiet, at(start, 200)),
            [
                FromWorker::SoundLevel { level: 0 },
                FromWorker::ParticipantLevel {
                    connection: 0,
                    identity: "@a:example.org:A".to_owned(),
                    level: 0,
                },
            ]
        );
    }

    /// GIVEN a participant that was heard WHEN it is no longer measured THEN
    /// it gets silence once and is forgotten; a bad identity or connection is
    /// never sent.
    #[test]
    fn given_a_participant_that_goes_then_silence_is_sent_once() {
        let mut meters = LevelMeters::default();
        let start = Instant::now();
        meters.next(
            &Levels {
                own: Some(0.0),
                remote: vec![
                    remote(0, "@a:example.org:A", 0.5),
                    remote(0, "", 0.5),
                    remote(9, "@c:example.org:C", 0.5),
                ],
            },
            start,
        );
        let gone = meters.next(&Levels::default(), at(start, 100));
        assert_eq!(
            gone,
            [FromWorker::ParticipantLevel {
                connection: 0,
                identity: "@a:example.org:A".to_owned(),
                level: 0,
            }]
        );
        assert!(meters.next(&Levels::default(), at(start, 200)).is_empty());
    }

    /// GIVEN more remote participants than the protocol allows WHEN measured
    /// THEN at most `MAX_PARTICIPANTS` are sent.
    #[test]
    fn given_too_many_participants_then_the_meters_stay_bounded() {
        let mut meters = LevelMeters::default();
        let levels = Levels {
            own: Some(0.0),
            remote: (0..MAX_PARTICIPANTS + 5)
                .map(|n| remote(0, &format!("@p{n}:example.org:D"), 0.5))
                .collect(),
        };
        assert_eq!(meters.next(&levels, Instant::now()).len(), MAX_PARTICIPANTS);
    }

    /// A roster with one track per entry: connection, identity, SID and a
    /// number as the track.
    fn roster(entries: &[(u8, &str, &str, u32)]) -> AudioRoster<u32> {
        let mut roster = AudioRoster::default();
        for (connection, identity, sid, track) in entries {
            assert!(roster.add(
                *connection,
                (*identity).to_owned(),
                (*sid).to_owned(),
                *track
            ));
        }
        roster
    }

    fn entry(connection: u8, identity: &str, track: u32) -> (u8, String, u32) {
        (connection, identity.to_owned(), track)
    }

    const A: &str = "@a:example.org:D";
    const B: &str = "@b:example.org:D";

    #[test]
    fn given_subscribed_audio_tracks_when_measured_then_they_come_in_subscription_order() {
        // GIVEN tracks on the own connection and on another SFU
        let roster = roster(&[(0, A, "TR_1", 1), (2, B, "TR_2", 2), (0, B, "TR_3", 3)]);

        // WHEN the meters read them, THEN each comes with its connection
        // and identity, in the order of the subscriptions
        assert_eq!(
            roster.measured(),
            vec![entry(0, A, 1), entry(2, B, 2), entry(0, B, 3)]
        );
    }

    #[test]
    fn given_a_kept_track_when_it_is_subscribed_again_then_it_is_replaced_in_its_place() {
        // GIVEN two kept tracks
        let mut roster = roster(&[(0, A, "TR_1", 1), (0, B, "TR_2", 2)]);

        // WHEN the first is subscribed again with a new track
        assert!(roster.add(0, A.to_owned(), "TR_1".to_owned(), 7));

        // THEN it keeps its place and is not kept twice
        assert_eq!(roster.measured(), vec![entry(0, A, 7), entry(0, B, 2)]);
    }

    #[test]
    fn given_kept_tracks_when_one_is_unsubscribed_then_only_that_track_goes() {
        // GIVEN two tracks of one participant, one of another, and the same
        // participant and SID on another connection
        let mut roster = roster(&[
            (0, A, "TR_1", 1),
            (0, A, "TR_2", 2),
            (0, B, "TR_1", 3),
            (1, A, "TR_1", 4),
        ]);

        // WHEN the first track is unsubscribed
        roster.remove(0, A, "TR_1");

        // THEN the others stay
        assert_eq!(
            roster.measured(),
            vec![entry(0, A, 2), entry(0, B, 3), entry(1, A, 4)]
        );
    }

    #[test]
    fn given_a_participant_with_tracks_when_it_leaves_then_its_tracks_on_that_connection_go() {
        // GIVEN a participant with two tracks, on two connections
        let mut roster = roster(&[
            (0, A, "TR_1", 1),
            (0, A, "TR_2", 2),
            (0, B, "TR_3", 3),
            (1, A, "TR_4", 4),
        ]);

        // WHEN it leaves the own connection
        roster.remove_participant(0, A);

        // THEN its tracks there go, and its track on the other connection
        // stays
        assert_eq!(roster.measured(), vec![entry(0, B, 3), entry(1, A, 4)]);
    }

    #[test]
    fn given_tracks_on_two_connections_when_one_goes_down_then_only_its_tracks_go() {
        // GIVEN tracks on the own connection and on two other SFUs
        let mut roster = roster(&[(0, A, "TR_1", 1), (1, A, "TR_2", 2), (3, B, "TR_3", 3)]);

        // WHEN connection 1 goes down
        roster.remove_connection(1);

        // THEN its track goes and the others stay
        assert_eq!(roster.measured(), vec![entry(0, A, 1), entry(3, B, 3)]);
    }

    #[test]
    fn given_an_invalid_identity_or_connection_when_a_track_comes_then_nothing_is_kept() {
        let mut roster = AudioRoster::default();
        let long = "a".repeat(MAX_IDENTITY_BYTES + 1);

        // GIVEN identities the protocol refuses, and a connection out of
        // range, WHEN their tracks come, THEN none is kept
        for (connection, identity) in [
            (0, ""),
            (0, "@a:example.org\n"),
            (0, long.as_str()),
            (MAX_OTHER_CONNECTIONS + 1, A),
        ] {
            assert!(!roster.add(connection, identity.to_owned(), "TR_1".to_owned(), 1));
        }
        assert!(roster.measured().is_empty());
    }

    #[test]
    fn given_more_tracks_than_are_measured_when_measured_ones_go_then_waiting_ones_move_up() {
        // GIVEN a full roster: the measured tracks and as many waiting
        let identities = names(MAX_KEPT_AUDIO_TRACKS);
        let mut roster = AudioRoster::default();
        for (track, identity) in (0_u32..).zip(&identities) {
            assert!(roster.add(0, identity.clone(), format!("TR_{track}"), track));
        }

        // WHEN one more comes, THEN it is refused, and only the first are
        // measured
        assert!(!roster.add(0, A.to_owned(), "TR_X".to_owned(), u32::MAX));
        let first: Vec<u32> = (0_u32..).take(MAX_MEASURED_TRACKS).collect();
        let tracks = |roster: &AudioRoster<u32>| -> Vec<u32> {
            roster
                .measured()
                .into_iter()
                .map(|(_, _, track)| track)
                .collect()
        };
        assert_eq!(tracks(&roster), first);

        // WHEN the measured participants leave
        for identity in identities.iter().take(MAX_MEASURED_TRACKS) {
            roster.remove_participant(0, identity);
        }

        // THEN the waiting tracks are measured, and the refused one is not
        let waiting: Vec<u32> = (0_u32..)
            .skip(MAX_MEASURED_TRACKS)
            .take(MAX_KEPT_AUDIO_TRACKS - MAX_MEASURED_TRACKS)
            .collect();
        assert_eq!(tracks(&roster), waiting);

        // AND a new track has room again
        assert!(roster.add(0, A.to_owned(), "TR_X".to_owned(), u32::MAX));
    }
}
