//! Bounded control protocol between the core and a contained call worker.
//!
//! Each message is the body of one media-protocol Control frame:
//! `[u8 version][u8 tag][fields]`. Integers are big-endian. A string is a
//! `u16` length and UTF-8 bytes. A byte field is a `u8` length and bytes.
//! Decoding rejects anything outside the limits below. Errors carry no text.

use std::io::{self, Read, Write};

use persephone_media_protocol::{
    BudgetPermit, FrameError, FrameHeader, FrameKind, ResourceBudgets, read_frame, write_frame,
};
use zeroize::Zeroizing;

mod stats;
pub use stats::{
    AudioReceiveStats, AudioSendStats, CallStats, ConnectionStats, MAX_STATS_CONNECTIONS,
    MAX_STATS_FPS, MAX_STATS_KBPS, MAX_STATS_MS, MAX_STATS_SIDE, QualityLimit, RemoteStats,
    VideoReceiveStats, VideoSendStats,
};

pub const VERSION: u8 = 12;
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Longest diagnostic record target (ADR-0058).
pub const MAX_DIAGNOSTIC_TARGET_BYTES: usize = 64;
/// Longest diagnostic record message (ADR-0058).
pub const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 2048;
/// Highest diagnostic level: 1 error, 2 warn, 3 info, 4 debug.
pub const MAX_DIAGNOSTIC_LEVEL: u8 = 4;
/// Transport crates that are too busy at `debug` (TLS and socket steps on
/// every connect): the core and the worker keep their records only from
/// `info` (ADR-0058). One list for both; it is not on the wire.
pub const QUIET_LOG_TARGETS: &[&str] = &[
    "h2",
    "hyper",
    "hyper_util",
    "mio",
    "reqwest",
    "rustls",
    "tokio",
    "tokio_util",
    "tungstenite",
    "tokio_tungstenite",
    "want",
];
pub const MAX_URL_BYTES: usize = 2048;
pub const MAX_JWT_BYTES: usize = 8192;
pub const MAX_IDENTITY_BYTES: usize = 512;
pub const MAX_TURN_URIS: usize = 8;
pub const MAX_TURN_URI_BYTES: usize = 512;
pub const MAX_TURN_CREDENTIAL_BYTES: usize = 512;
pub const MIN_KEY_BYTES: usize = 16;
pub const MAX_KEY_BYTES: usize = 64;
pub const MAX_PARTICIPANTS: usize = 32;
/// Most subscribe-only connections to other SFUs for one call (ADR-0054).
/// Connection 0 is the own SFU; the others are 1 to this number.
pub const MAX_OTHER_CONNECTIONS: u8 = 3;
/// A device ID as the platform reports it: an audio endpoint ID from
/// libwebrtc, or a camera's symbolic link.
pub const MAX_DEVICE_ID_BYTES: usize = 512;
/// A device name as the OS reports it, cut on a character boundary.
pub const MAX_DEVICE_NAME_BYTES: usize = 256;
/// Devices per list (microphones, speakers or cameras).
pub const MAX_DEVICES: usize = 32;
/// The longest side of a video picture, in pixels (ADR-0048).
pub const MAX_VIDEO_SIDE: u32 = 640;
/// The most pixels in one video picture.
pub const MAX_VIDEO_PIXELS: u32 = 640 * 360;
/// The most bytes in one video picture: RGBA, four bytes per pixel.
pub const MAX_VIDEO_BYTES: usize = MAX_VIDEO_PIXELS as usize * 4;
/// Video tiles in a call. Tile 0 is the own camera; tiles 1 to 4 are remote
/// cameras; tile 5 is the one decoded remote screen share (ADR-0053).
pub const VIDEO_TILES: u8 = 6;
/// The tile of the remote screen share.
pub const SCREEN_TILE: u8 = 5;
/// The most pictures per second for one camera tile.
pub const MAX_VIDEO_FPS: u32 = 15;
/// The longest side of a screen picture, in pixels (ADR-0053).
pub const MAX_SCREEN_SIDE: u32 = 1280;
/// The most pixels in one screen picture: 1280 x 720.
pub const MAX_SCREEN_PIXELS: u32 = 1280 * 720;
/// The most bytes in one screen picture: RGBA, four bytes per pixel.
pub const MAX_SCREEN_BYTES: usize = MAX_SCREEN_PIXELS as usize * 4;
/// The most pictures per second for the screen tile.
pub const MAX_SCREEN_FPS: u32 = 5;
/// The most monitors the core can name to the worker.
pub const MAX_SCREEN_MONITORS: u8 = 16;
/// The largest side of a monitor the core names, in pixels.
pub const MAX_MONITOR_SIDE: u32 = 16_384;

/// The longest side of a picture on this tile.
pub const fn max_side(tile: u8) -> u32 {
    if tile == SCREEN_TILE {
        MAX_SCREEN_SIDE
    } else {
        MAX_VIDEO_SIDE
    }
}

/// The most pixels in one picture on this tile.
pub const fn max_pixels(tile: u8) -> u32 {
    if tile == SCREEN_TILE {
        MAX_SCREEN_PIXELS
    } else {
        MAX_VIDEO_PIXELS
    }
}

/// The most pictures per second on this tile.
pub const fn max_fps(tile: u8) -> u32 {
    if tile == SCREEN_TILE {
        MAX_SCREEN_FPS
    } else {
        MAX_VIDEO_FPS
    }
}

/// A picture of `width` x `height` pixels is inside the limits of `tile`.
pub fn picture_fits(tile: u8, width: u32, height: u32) -> bool {
    tile < VIDEO_TILES
        && (1..=max_side(tile)).contains(&width)
        && (1..=max_side(tile)).contains(&height)
        && u64::from(width) * u64::from(height) <= u64::from(max_pixels(tile))
}
/// The largest rotation: quarter turns clockwise, 0 to 3.
pub const MAX_VIDEO_ROTATION: u8 = 3;
/// The highest playout volume, in percent: the sound as received (ADR-0055).
pub const MAX_VOLUME_PERCENT: u8 = 100;
/// The highest microphone gain, in percent: four times the captured sound
/// (ADR-0062).
pub const MAX_MIC_GAIN_PERCENT: u16 = 400;
/// The microphone gain moves in steps of this many percent (ADR-0062).
pub const MIC_GAIN_STEP_PERCENT: u16 = 5;
/// The microphone gain when the user chose none: the sound as captured.
pub const DEFAULT_MIC_GAIN_PERCENT: u16 = 100;

/// Whether `percent` is a microphone gain: 0 to [`MAX_MIC_GAIN_PERCENT`] in
/// whole steps of [`MIC_GAIN_STEP_PERCENT`] (ADR-0062).
pub const fn valid_mic_gain(percent: u16) -> bool {
    percent <= MAX_MIC_GAIN_PERCENT && percent.is_multiple_of(MIC_GAIN_STEP_PERCENT)
}
/// The highest received sound level (ADR-0055): 0 dBFS. 0 is -60 dBFS or
/// quieter, and each step is 0.6 dB.
pub const MAX_SOUND_LEVEL: u8 = 100;
/// The fixed length of a media-protocol frame header.
const FRAME_HEADER_LEN: usize = 17;

/// Why a message body was refused. There is no free text on purpose.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    UnknownVersion,
    UnknownTag,
    Truncated,
    TrailingBytes,
    /// The message or one field is over its limit.
    TooLarge,
    /// A required field is empty or under its minimum.
    TooShort,
    InvalidUtf8,
    /// A control character, or a non-graphic character in an ASCII field.
    InvalidCharacter,
    /// A bad boolean, failure code, URL scheme or URI prefix.
    InvalidValue,
}

/// A frame key with its key index.
#[derive(Clone, Eq, PartialEq)]
pub struct FrameKeyMsg {
    pub index: u8,
    pub bytes: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for FrameKeyMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameKeyMsg")
            .field("index", &self.index)
            .field("len", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct TurnServer {
    pub uri: String,
    pub username: Zeroizing<String>,
    pub credential: Zeroizing<String>,
}

impl std::fmt::Debug for TurnServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnServer")
            .field("uri", &self.uri)
            .field("username_len", &self.username.len())
            .field("credential_len", &self.credential.len())
            .finish_non_exhaustive()
    }
}

/// One microphone, speaker or camera. The ID is the platform's stable
/// device ID; the name is untrusted OS text for display only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaDevice {
    pub id: String,
    pub name: String,
}

impl MediaDevice {
    /// A device that fits the wire limits, or `None` when the ID does not:
    /// empty, over [`MAX_DEVICE_ID_BYTES`], or holding a control character.
    /// The name loses its control characters and is cut on a character
    /// boundary to [`MAX_DEVICE_NAME_BYTES`].
    pub fn bounded(id: &str, name: &str) -> Option<Self> {
        if id.is_empty() || id.len() > MAX_DEVICE_ID_BYTES || id.chars().any(char::is_control) {
            return None;
        }
        let mut kept = String::new();
        for c in name.chars().filter(|c| !c.is_control()) {
            if kept.len() + c.len_utf8() > MAX_DEVICE_NAME_BYTES {
                break;
            }
            kept.push(c);
        }
        Some(Self {
            id: id.to_owned(),
            name: kept,
        })
    }
}

/// The start of a Linux sound server device ID (ADR-0049): `pulse:` and then
/// the PulseAudio sink or source name. libwebrtc's own Linux IDs are list
/// positions (`index:N`), which are never listed; these names are stable.
pub const PULSE_PREFIX: &str = "pulse:";
/// The longest sound server device name taken (PulseAudio's `PA_NAME_MAX`).
pub const MAX_PULSE_NAME_BYTES: usize = 128;

/// Whether `name` can be a sound server device name: 1 to
/// [`MAX_PULSE_NAME_BYTES`] ASCII letters, digits, `.`, `-`, `_` or `:`. The
/// name goes into the worker's environment, so nothing else is taken. A
/// device with another name is not listed.
pub fn valid_pulse_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PULSE_NAME_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':'))
}

/// The sound server name inside a device ID, or `None` when `id` is not a
/// `pulse:` ID with a valid name.
pub fn pulse_name(id: &str) -> Option<&str> {
    id.strip_prefix(PULSE_PREFIX)
        .filter(|name| valid_pulse_name(name))
}

/// The device ID of the sound server device `name`, or `None` for an invalid
/// name.
pub fn pulse_id(name: &str) -> Option<String> {
    valid_pulse_name(name).then(|| format!("{PULSE_PREFIX}{name}"))
}

#[derive(Clone, Eq, PartialEq)]
pub enum ToWorker {
    Join {
        sfu_url: String,
        jwt: Zeroizing<String>,
        turn: Vec<TurnServer>,
        relay_only: bool,
        own_identity: String,
        own_key: FrameKeyMsg,
        /// The microphone to use; `None` is the system default.
        recording_device: Option<String>,
        /// The speaker to use; `None` is the system default.
        playout_device: Option<String>,
        /// The camera to use; `None` is the first camera the system lists.
        camera_device: Option<String>,
        /// Start the camera as the call connects.
        camera_on: bool,
        /// The playout volume of every remote audio track, 0 to
        /// [`MAX_VOLUME_PERCENT`].
        volume: u8,
        /// Whether the automatic microphone level (automatic gain control)
        /// is on (ADR-0062).
        auto_level: bool,
        /// The manual microphone gain in percent, applied before any
        /// processing: 0 to [`MAX_MIC_GAIN_PERCENT`] in steps of
        /// [`MIC_GAIN_STEP_PERCENT`] (ADR-0062).
        mic_gain: u16,
    },
    /// Only as the first message: report the devices, then exit.
    ListDevices,
    SetKey {
        identity: String,
        key: FrameKeyMsg,
    },
    SetOwnKey {
        key: FrameKeyMsg,
    },
    RemoveParticipant {
        identity: String,
    },
    SetMuted {
        muted: bool,
    },
    Leave,
    /// Start or stop the screen share (ADR-0053). The worker answers with
    /// [`FromWorker::Screen`]. A start carries what the core chose; the page
    /// never names a source.
    SetScreen {
        target: Option<ScreenTarget>,
        /// The core's number for the share: a new one for each start, the
        /// stopped share's for a stop. Every `Screen` state carries the
        /// number of the share it describes, so a late state of an older
        /// share is never taken for the newer one.
        share: u32,
    },
    /// Turn the camera on or off. The worker answers with
    /// [`FromWorker::Camera`].
    SetCamera {
        on: bool,
        /// A camera portal remote went over the camera channel for this
        /// camera-on, directly before it (Linux portal path). The worker
        /// takes it with this command and closes it if the command opens
        /// nothing. Only a camera-on carries one.
        remote: bool,
    },
    /// Connect to another SFU as subscriber only (ADR-0054). `index` is the
    /// connection number, 1 to [`MAX_OTHER_CONNECTIONS`]; the own SFU is
    /// connection 0. The worker publishes nothing on it and answers with
    /// [`FromWorker::Connection`].
    Subscribe {
        index: u8,
        url: String,
        jwt: Zeroizing<String>,
    },
    /// Stop or restart the playout of every remote audio track, including
    /// tracks subscribed later. It does not change the microphone. The
    /// worker answers with [`FromWorker::Deafened`].
    SetDeafened {
        deafened: bool,
    },
    /// Set the playout volume of every remote audio track, including tracks
    /// subscribed later: 0 to [`MAX_VOLUME_PERCENT`]. No answer.
    SetVolume {
        percent: u8,
    },
    /// Set the automatic microphone level and the manual microphone gain of
    /// the running call (ADR-0062). The gain applies at once. A gain over
    /// the limit or off the step is refused with the whole message. No
    /// answer.
    SetMicrophone {
        auto_level: bool,
        mic_gain: u16,
    },
    /// Start or stop sending [`FromWorker::Diagnostic`] records (ADR-0058).
    /// Off when the worker starts. No answer.
    SetDiagnostics {
        on: bool,
    },
    /// Only as the first message: test one device, then exit (ADR-0059).
    /// `device` is the chosen ID; `None` is the system default. The worker
    /// sends `Connected` once the device runs, then `SoundLevel` for a
    /// microphone, or `Camera` and tile 0 pictures for a camera, and `Left`
    /// when the test ends by itself or after `Leave`; `Failed` when the
    /// device cannot be used.
    TestDevice {
        test: DeviceTest,
        device: Option<String>,
    },
    /// Move the running call to another microphone and speaker, and choose
    /// the camera for the next camera start (ADR-0059). `None` is the system
    /// default. The worker answers with [`FromWorker::DevicesSet`], about the
    /// microphone and speaker, and keeps the call either way.
    SetDevices {
        recording: Option<String>,
        playout: Option<String>,
        camera: Option<String>,
    },
    /// Start or stop sending [`FromWorker::Stats`], at most one a second
    /// (ADR-0066). Off when the worker starts. No answer.
    SetStats {
        on: bool,
    },
}

/// The device that [`ToWorker::TestDevice`] tests. A closed set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum DeviceTest {
    Microphone = 1,
    Speaker = 2,
    Camera = 3,
}

impl TryFrom<u8> for DeviceTest {
    type Error = DecodeError;

    fn try_from(value: u8) -> Result<Self, DecodeError> {
        Ok(match value {
            1 => Self::Microphone,
            2 => Self::Speaker,
            3 => Self::Camera,
            _ => return Err(DecodeError::InvalidValue),
        })
    }
}

impl std::fmt::Debug for ToWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Join {
                sfu_url,
                jwt,
                turn,
                relay_only,
                own_identity,
                own_key,
                recording_device,
                playout_device,
                camera_device,
                camera_on,
                volume,
                auto_level,
                mic_gain,
            } => f
                .debug_struct("Join")
                .field("sfu_url", sfu_url)
                .field("jwt_len", &jwt.len())
                .field("turn", turn)
                .field("relay_only", relay_only)
                .field("own_identity", own_identity)
                .field("own_key", own_key)
                .field("recording_device", recording_device)
                .field("playout_device", playout_device)
                .field("camera_device", camera_device)
                .field("camera_on", camera_on)
                .field("volume", volume)
                .field("auto_level", auto_level)
                .field("mic_gain", mic_gain)
                .finish(),
            Self::ListDevices => f.write_str("ListDevices"),
            Self::SetKey { identity, key } => f
                .debug_struct("SetKey")
                .field("identity", identity)
                .field("key", key)
                .finish(),
            Self::SetOwnKey { key } => f.debug_struct("SetOwnKey").field("key", key).finish(),
            Self::RemoveParticipant { identity } => f
                .debug_struct("RemoveParticipant")
                .field("identity", identity)
                .finish(),
            Self::SetMuted { muted } => f.debug_struct("SetMuted").field("muted", muted).finish(),
            Self::Leave => f.write_str("Leave"),
            Self::SetCamera { on, remote } => f
                .debug_struct("SetCamera")
                .field("on", on)
                .field("remote", remote)
                .finish(),
            Self::SetScreen { target, share } => f
                .debug_struct("SetScreen")
                .field("target", target)
                .field("share", share)
                .finish(),
            Self::Subscribe { index, url, jwt } => f
                .debug_struct("Subscribe")
                .field("index", index)
                .field("url", url)
                .field("jwt_len", &jwt.len())
                .finish(),
            Self::SetDeafened { deafened } => f
                .debug_struct("SetDeafened")
                .field("deafened", deafened)
                .finish(),
            Self::SetMicrophone {
                auto_level,
                mic_gain,
            } => f
                .debug_struct("SetMicrophone")
                .field("auto_level", auto_level)
                .field("mic_gain", mic_gain)
                .finish(),
            Self::SetVolume { percent } => f
                .debug_struct("SetVolume")
                .field("percent", percent)
                .finish(),
            Self::SetDiagnostics { on } => {
                f.debug_struct("SetDiagnostics").field("on", on).finish()
            }
            Self::TestDevice { test, device } => f
                .debug_struct("TestDevice")
                .field("test", test)
                .field("device", device)
                .finish(),
            Self::SetDevices {
                recording,
                playout,
                camera,
            } => f
                .debug_struct("SetDevices")
                .field("camera", camera)
                .field("recording", recording)
                .field("playout", playout)
                .finish(),
            Self::SetStats { on } => f.debug_struct("SetStats").field("on", on).finish(),
        }
    }
}

/// Why a call worker failed. A closed set with no text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum FailureCode {
    Containment = 1,
    SignallingRefused = 2,
    Tls = 3,
    Network = 4,
    Decryption = 5,
    AudioDevice = 6,
    Protocol = 7,
    Internal = 8,
    /// The chosen microphone or speaker is not connected.
    DeviceNotFound = 9,
    /// This system cannot run the asked device test or device change
    /// (ADR-0059).
    Unsupported = 10,
    /// The system's privacy settings do not let the worker use the device
    /// (ADR-0059).
    DeviceDenied = 11,
    /// A device change failed, and the call runs on the devices it had: the
    /// old microphone was switched back and captures (ADR-0059).
    DeviceKept = 12,
}

impl TryFrom<u8> for FailureCode {
    type Error = DecodeError;

    fn try_from(value: u8) -> Result<Self, DecodeError> {
        Ok(match value {
            1 => Self::Containment,
            2 => Self::SignallingRefused,
            3 => Self::Tls,
            4 => Self::Network,
            5 => Self::Decryption,
            6 => Self::AudioDevice,
            7 => Self::Protocol,
            8 => Self::Internal,
            9 => Self::DeviceNotFound,
            10 => Self::Unsupported,
            11 => Self::DeviceDenied,
            12 => Self::DeviceKept,
            _ => return Err(DecodeError::InvalidValue),
        })
    }
}

/// What the core grants for one screen share (ADR-0053).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScreenTarget {
    /// Linux: the portal remote went over the camera channel with kind `s`,
    /// directly before this command. `node` is the PipeWire node ID.
    Portal { node: u32 },
    /// Windows: the monitor's index in display order and its size in pixels
    /// when the user chose it. The worker reads its own list when the share
    /// starts; another size at that index is "not found".
    Monitor { index: u8, width: u32, height: u32 },
}

/// The state of the screen share, as the worker reports it. A closed set
/// with no text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ScreenState {
    Off = 0,
    Starting = 1,
    On = 2,
    /// The chosen screen is gone, or has another size.
    NotFound = 3,
    /// Screen capture cannot run here, or capture failed.
    Unavailable = 4,
    /// The OS or the portal refused the capture.
    Refused = 5,
}

impl TryFrom<u8> for ScreenState {
    type Error = DecodeError;

    fn try_from(value: u8) -> Result<Self, DecodeError> {
        Ok(match value {
            0 => Self::Off,
            1 => Self::Starting,
            2 => Self::On,
            3 => Self::NotFound,
            4 => Self::Unavailable,
            5 => Self::Refused,
            _ => return Err(DecodeError::InvalidValue),
        })
    }
}

/// The state of the camera, as the worker reports it. A closed set with no
/// text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CameraState {
    Off = 0,
    On = 1,
    /// The chosen camera is not connected.
    NotFound = 2,
    /// No camera, the OS denied it, it is in use, or capture failed.
    Unavailable = 3,
}

impl TryFrom<u8> for CameraState {
    type Error = DecodeError;

    fn try_from(value: u8) -> Result<Self, DecodeError> {
        Ok(match value {
            0 => Self::Off,
            1 => Self::On,
            2 => Self::NotFound,
            3 => Self::Unavailable,
            _ => return Err(DecodeError::InvalidValue),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FromWorker {
    /// Containment self-check result. Zero means all controls are present,
    /// as for the media worker.
    Ready {
        containment_bits: u32,
    },
    Connected,
    /// A remote participant joined the room of this connection (0 is the
    /// own SFU, 1 to 3 are other SFUs, ADR-0054).
    ParticipantJoined {
        connection: u8,
        identity: String,
    },
    ParticipantLeft {
        connection: u8,
        identity: String,
    },
    Speaking {
        connection: u8,
        identity: String,
        speaking: bool,
    },
    Muted {
        muted: bool,
    },
    /// The answer to `SetDeafened`: remote audio is off (`true`) or on.
    Deafened {
        deafened: bool,
    },
    /// The loudest remote audio now, as WebRTC measures it for playout
    /// (after the call volume): 0 to [`MAX_SOUND_LEVEL`] (ADR-0055). Sent
    /// only when it changes.
    SoundLevel {
        level: u8,
    },
    /// The sound of one remote participant now, as WebRTC measures it for
    /// playout (after the call volume): 0 to [`MAX_SOUND_LEVEL`]. Sent when
    /// it changes, and an unchanged level above 0 again once a second.
    ParticipantLevel {
        connection: u8,
        identity: String,
        level: u8,
    },
    /// A remote participant's microphone is muted, or it has none (`true`),
    /// or it is on.
    ParticipantMuted {
        connection: u8,
        identity: String,
        muted: bool,
    },
    /// The answer to [`ToWorker::SetDevices`]: `None` when the call now uses
    /// the chosen devices, else why the change failed. `DeviceNotFound` and
    /// `Unsupported` change nothing. After `AudioDevice` the worker has
    /// tried to go back to the old devices, but the microphone may be the
    /// new one and the sound may have stopped. A failed speaker stays
    /// stopped until the call is joined again, so each later change answers
    /// `AudioDevice` (ADR-0059).
    DevicesSet {
        failure: Option<FailureCode>,
    },
    Failed {
        code: FailureCode,
    },
    Left,
    /// The reply to `ListDevices`.
    Devices {
        recording: Vec<MediaDevice>,
        playout: Vec<MediaDevice>,
        cameras: Vec<MediaDevice>,
    },
    /// The camera state. Sent after every `SetCamera`, after a join with
    /// the camera on, and when capture fails during a call.
    Camera {
        state: CameraState,
    },
    /// The screen share state. Sent after every `SetScreen`, when the share
    /// ends by itself, and when capture fails during a call.
    Screen {
        state: ScreenState,
        /// The `SetScreen` number of the share this state describes.
        share: u32,
    },
    /// A remote participant started or stopped a screen share that is not
    /// shown on tile 5. It is only a display hint (ADR-0053).
    ScreenSharer {
        connection: u8,
        identity: String,
        sharing: bool,
    },
    /// A remote participant's video arrived on this tile (1 to 5).
    VideoStarted {
        tile: u8,
        connection: u8,
        identity: String,
    },
    /// The video on this tile (0 to 5) ended.
    VideoStopped {
        tile: u8,
    },
    /// Announces one picture for this tile (0 to 5). Always followed at once
    /// by exactly one `RgbaPixels` frame: use [`read_worker_item`] and
    /// [`write_video_frame`]. `rotation` is quarter turns clockwise, 0 to 3.
    VideoFrame {
        tile: u8,
        rotation: u8,
    },
    /// The connection to another SFU (1 to 3) is up, or it ended or never
    /// came up (ADR-0054). It never ends the call.
    Connection {
        connection: u8,
        up: bool,
    },
    /// One record for the diagnostic log, sent only while
    /// [`ToWorker::SetDiagnostics`] is on (ADR-0058). The one free-text
    /// message: it is untrusted, has no control characters, is at most
    /// [`MAX_DIAGNOSTIC_MESSAGE_BYTES`], and the core only redacts it and
    /// writes it to the log, never to the UI. `level` is 1 (error) to
    /// [`MAX_DIAGNOSTIC_LEVEL`] (debug).
    Diagnostic {
        level: u8,
        target: String,
        message: String,
    },
    /// The call's statistics, as numbers with units (ADR-0066). Sent at
    /// most once a second, and only while [`ToWorker::SetStats`] is on.
    Stats(CallStats),
}

// Tags of core to worker messages.
const TO_JOIN: u8 = 1;
const TO_SET_KEY: u8 = 2;
const TO_SET_OWN_KEY: u8 = 3;
const TO_REMOVE_PARTICIPANT: u8 = 4;
const TO_SET_MUTED: u8 = 5;
const TO_LEAVE: u8 = 6;
const TO_LIST_DEVICES: u8 = 7;
const TO_SET_CAMERA: u8 = 8;
const TO_SET_SCREEN: u8 = 9;
const TO_SUBSCRIBE: u8 = 10;
const TO_SET_DEAFENED: u8 = 11;
const TO_SET_VOLUME: u8 = 12;
const TO_SET_DIAGNOSTICS: u8 = 13;
const TO_TEST_DEVICE: u8 = 14;
const TO_SET_DEVICES: u8 = 15;
const TO_SET_MICROPHONE: u8 = 16;
const TO_SET_STATS: u8 = 17;

// Tags of worker to core messages.
const FROM_READY: u8 = 1;
const FROM_CONNECTED: u8 = 2;
const FROM_PARTICIPANT_JOINED: u8 = 3;
const FROM_PARTICIPANT_LEFT: u8 = 4;
const FROM_SPEAKING: u8 = 5;
const FROM_MUTED: u8 = 6;
const FROM_FAILED: u8 = 7;
const FROM_LEFT: u8 = 8;
const FROM_DEVICES: u8 = 9;
const FROM_CAMERA: u8 = 10;
const FROM_VIDEO_STARTED: u8 = 11;
const FROM_VIDEO_STOPPED: u8 = 12;
const FROM_VIDEO_FRAME: u8 = 13;
const FROM_SCREEN: u8 = 14;
const FROM_SCREEN_SHARER: u8 = 15;
const FROM_CONNECTION: u8 = 16;
const FROM_DEAFENED: u8 = 17;
const FROM_DIAGNOSTIC: u8 = 18;
const FROM_SOUND_LEVEL: u8 = 19;
const FROM_PARTICIPANT_LEVEL: u8 = 20;
const FROM_PARTICIPANT_MUTED: u8 = 21;
const FROM_DEVICES_SET: u8 = 22;
const FROM_STATS: u8 = 23;

#[derive(Clone, Copy)]
enum CharRule {
    /// Any text without control characters.
    NoControl,
    /// ASCII graphic characters only.
    AsciiGraphic,
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], DecodeError> {
        if self.rest.len() < count {
            return Err(DecodeError::Truncated);
        }
        let (head, tail) = self.rest.split_at(count);
        self.rest = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    /// A microphone gain: see [`valid_mic_gain`].
    fn mic_gain(&mut self) -> Result<u16, DecodeError> {
        let percent = self.u16()?;
        if valid_mic_gain(percent) {
            Ok(percent)
        } else {
            Err(DecodeError::InvalidValue)
        }
    }

    fn bool(&mut self) -> Result<bool, DecodeError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError::InvalidValue),
        }
    }

    /// Reads a string. The length is checked against `max` before the bytes
    /// are touched, and an empty string is refused unless `allow_empty`.
    fn str(
        &mut self,
        max: usize,
        rule: CharRule,
        allow_empty: bool,
    ) -> Result<&'a str, DecodeError> {
        let length = self.take(2)?;
        let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
        if length > max {
            return Err(DecodeError::TooLarge);
        }
        if length == 0 && !allow_empty {
            return Err(DecodeError::TooShort);
        }
        let text = std::str::from_utf8(self.take(length)?).map_err(|_| DecodeError::InvalidUtf8)?;
        let valid = match rule {
            CharRule::NoControl => !text.chars().any(char::is_control),
            CharRule::AsciiGraphic => text.bytes().all(|byte| byte.is_ascii_graphic()),
        };
        if valid {
            Ok(text)
        } else {
            Err(DecodeError::InvalidCharacter)
        }
    }

    fn identity(&mut self) -> Result<String, DecodeError> {
        Ok(self
            .str(MAX_IDENTITY_BYTES, CharRule::NoControl, false)?
            .to_owned())
    }

    /// An optional device ID: empty means the system default.
    fn device_choice(&mut self) -> Result<Option<String>, DecodeError> {
        let id = self.str(MAX_DEVICE_ID_BYTES, CharRule::NoControl, true)?;
        Ok((!id.is_empty()).then(|| id.to_owned()))
    }

    /// A connection number: 0 is the own SFU, 1 to 3 are other SFUs.
    fn connection(&mut self) -> Result<u8, DecodeError> {
        let connection = self.u8()?;
        if connection <= MAX_OTHER_CONNECTIONS {
            Ok(connection)
        } else {
            Err(DecodeError::InvalidValue)
        }
    }

    /// A playout volume: 0 to [`MAX_VOLUME_PERCENT`].
    fn volume(&mut self) -> Result<u8, DecodeError> {
        let percent = self.u8()?;
        if percent <= MAX_VOLUME_PERCENT {
            Ok(percent)
        } else {
            Err(DecodeError::InvalidValue)
        }
    }

    /// A received sound level: 0 to [`MAX_SOUND_LEVEL`].
    fn sound_level(&mut self) -> Result<u8, DecodeError> {
        let level = self.u8()?;
        if level <= MAX_SOUND_LEVEL {
            Ok(level)
        } else {
            Err(DecodeError::InvalidValue)
        }
    }

    /// A connection to another SFU: 1 to 3.
    fn other_connection(&mut self) -> Result<u8, DecodeError> {
        match self.connection()? {
            0 => Err(DecodeError::InvalidValue),
            connection => Ok(connection),
        }
    }

    /// A tile number from `first` up to the last tile.
    fn tile(&mut self, first: u8) -> Result<u8, DecodeError> {
        let tile = self.u8()?;
        if (first..VIDEO_TILES).contains(&tile) {
            Ok(tile)
        } else {
            Err(DecodeError::InvalidValue)
        }
    }

    fn devices(&mut self) -> Result<Vec<MediaDevice>, DecodeError> {
        let count = usize::from(self.u8()?);
        if count > MAX_DEVICES {
            return Err(DecodeError::TooLarge);
        }
        let mut devices = Vec::with_capacity(count);
        for _ in 0..count {
            let id = self
                .str(MAX_DEVICE_ID_BYTES, CharRule::NoControl, false)?
                .to_owned();
            let name = self
                .str(MAX_DEVICE_NAME_BYTES, CharRule::NoControl, true)?
                .to_owned();
            devices.push(MediaDevice { id, name });
        }
        Ok(devices)
    }

    fn secret(&mut self, max: usize, rule: CharRule) -> Result<Zeroizing<String>, DecodeError> {
        Ok(Zeroizing::new(self.str(max, rule, false)?.to_owned()))
    }

    fn key(&mut self) -> Result<FrameKeyMsg, DecodeError> {
        let index = self.u8()?;
        let length = usize::from(self.u8()?);
        if length > MAX_KEY_BYTES {
            return Err(DecodeError::TooLarge);
        }
        if length < MIN_KEY_BYTES {
            return Err(DecodeError::TooShort);
        }
        Ok(FrameKeyMsg {
            index,
            bytes: Zeroizing::new(self.take(length)?.to_vec()),
        })
    }

    fn finish(self) -> Result<(), DecodeError> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes)
        }
    }
}

/// Starts a reader after the version byte and returns the tag.
fn open(body: &[u8]) -> Result<(u8, Reader<'_>), DecodeError> {
    if body.len() > MAX_MESSAGE_BYTES {
        return Err(DecodeError::TooLarge);
    }
    let mut reader = Reader { rest: body };
    if reader.u8()? != VERSION {
        return Err(DecodeError::UnknownVersion);
    }
    let tag = reader.u8()?;
    Ok((tag, reader))
}

fn put_str(out: &mut Vec<u8>, text: &str) {
    // A field over u16::MAX is cut here, and the decoder refuses it anyway.
    let bytes = &text.as_bytes()[..text.len().min(usize::from(u16::MAX))];
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn put_devices(out: &mut Vec<u8>, devices: &[MediaDevice]) {
    let devices = &devices[..devices.len().min(usize::from(u8::MAX))];
    out.push(devices.len() as u8);
    for device in devices {
        put_str(out, &device.id);
        put_str(out, &device.name);
    }
}

fn put_key(out: &mut Vec<u8>, key: &FrameKeyMsg) {
    let bytes = &key.bytes[..key.bytes.len().min(usize::from(u8::MAX))];
    out.push(key.index);
    out.push(bytes.len() as u8);
    out.extend_from_slice(bytes);
}

fn validate_sfu_url(url: &str) -> Result<(), DecodeError> {
    if url
        .strip_prefix("wss://")
        .is_some_and(|rest| !rest.is_empty())
    {
        return Ok(());
    }
    #[cfg(feature = "dev-disposable-servers")]
    for prefix in ["ws://127.0.0.1", "ws://[::1]"] {
        if let Some(rest) = url.strip_prefix(prefix)
            && matches!(rest.bytes().next(), None | Some(b':' | b'/'))
        {
            return Ok(());
        }
    }
    Err(DecodeError::InvalidValue)
}

impl ToWorker {
    /// Encodes the message. The buffer holds the JWT, TURN credentials and
    /// frame keys, so it is cleared when dropped.
    pub fn encode(&self) -> Zeroizing<Vec<u8>> {
        let mut out = Zeroizing::new(vec![VERSION]);
        match self {
            Self::Join {
                sfu_url,
                jwt,
                turn,
                relay_only,
                own_identity,
                own_key,
                recording_device,
                playout_device,
                camera_device,
                camera_on,
                volume,
                auto_level,
                mic_gain,
            } => {
                out.push(TO_JOIN);
                put_str(&mut out, sfu_url);
                put_str(&mut out, jwt);
                out.push(turn.len().min(usize::from(u8::MAX)) as u8);
                for server in turn.iter().take(usize::from(u8::MAX)) {
                    put_str(&mut out, &server.uri);
                    put_str(&mut out, &server.username);
                    put_str(&mut out, &server.credential);
                }
                out.push(u8::from(*relay_only));
                put_str(&mut out, own_identity);
                put_key(&mut out, own_key);
                put_str(&mut out, recording_device.as_deref().unwrap_or(""));
                put_str(&mut out, playout_device.as_deref().unwrap_or(""));
                put_str(&mut out, camera_device.as_deref().unwrap_or(""));
                out.push(u8::from(*camera_on));
                out.push(*volume);
                out.push(u8::from(*auto_level));
                out.extend_from_slice(&mic_gain.to_be_bytes());
            }
            Self::ListDevices => out.push(TO_LIST_DEVICES),
            Self::SetKey { identity, key } => {
                out.push(TO_SET_KEY);
                put_str(&mut out, identity);
                put_key(&mut out, key);
            }
            Self::SetOwnKey { key } => {
                out.push(TO_SET_OWN_KEY);
                put_key(&mut out, key);
            }
            Self::RemoveParticipant { identity } => {
                out.push(TO_REMOVE_PARTICIPANT);
                put_str(&mut out, identity);
            }
            Self::SetMuted { muted } => {
                out.push(TO_SET_MUTED);
                out.push(u8::from(*muted));
            }
            Self::Leave => out.push(TO_LEAVE),
            Self::SetCamera { on, remote } => {
                out.push(TO_SET_CAMERA);
                out.push(u8::from(*on));
                out.push(u8::from(*remote));
            }
            Self::Subscribe { index, url, jwt } => {
                out.push(TO_SUBSCRIBE);
                out.push(*index);
                put_str(&mut out, url);
                put_str(&mut out, jwt);
            }
            Self::SetScreen { target, share } => {
                out.push(TO_SET_SCREEN);
                out.extend_from_slice(&share.to_be_bytes());
                match target {
                    None => out.push(0),
                    Some(ScreenTarget::Portal { node }) => {
                        out.push(1);
                        out.extend_from_slice(&node.to_be_bytes());
                    }
                    Some(ScreenTarget::Monitor {
                        index,
                        width,
                        height,
                    }) => {
                        out.push(2);
                        out.push(*index);
                        out.extend_from_slice(&width.to_be_bytes());
                        out.extend_from_slice(&height.to_be_bytes());
                    }
                }
            }
            Self::SetDeafened { deafened } => {
                out.push(TO_SET_DEAFENED);
                out.push(u8::from(*deafened));
            }
            Self::SetVolume { percent } => {
                out.push(TO_SET_VOLUME);
                out.push(*percent);
            }
            Self::SetMicrophone {
                auto_level,
                mic_gain,
            } => {
                out.push(TO_SET_MICROPHONE);
                out.push(u8::from(*auto_level));
                out.extend_from_slice(&mic_gain.to_be_bytes());
            }
            Self::SetDiagnostics { on } => {
                out.push(TO_SET_DIAGNOSTICS);
                out.push(u8::from(*on));
            }
            Self::TestDevice { test, device } => {
                out.push(TO_TEST_DEVICE);
                out.push(*test as u8);
                put_str(&mut out, device.as_deref().unwrap_or(""));
            }
            Self::SetDevices {
                recording,
                playout,
                camera,
            } => {
                out.push(TO_SET_DEVICES);
                put_str(&mut out, recording.as_deref().unwrap_or(""));
                put_str(&mut out, playout.as_deref().unwrap_or(""));
                put_str(&mut out, camera.as_deref().unwrap_or(""));
            }
            Self::SetStats { on } => {
                out.push(TO_SET_STATS);
                out.push(u8::from(*on));
            }
        }
        out
    }

    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let (tag, mut reader) = open(body)?;
        let message = match tag {
            TO_JOIN => {
                let sfu_url = reader.str(MAX_URL_BYTES, CharRule::AsciiGraphic, false)?;
                validate_sfu_url(sfu_url)?;
                let sfu_url = sfu_url.to_owned();
                let jwt = reader.secret(MAX_JWT_BYTES, CharRule::AsciiGraphic)?;
                let count = usize::from(reader.u8()?);
                if count > MAX_TURN_URIS {
                    return Err(DecodeError::TooLarge);
                }
                let mut turn = Vec::with_capacity(count);
                for _ in 0..count {
                    let uri = reader.str(MAX_TURN_URI_BYTES, CharRule::AsciiGraphic, false)?;
                    if !(uri.starts_with("turn:") || uri.starts_with("turns:")) {
                        return Err(DecodeError::InvalidValue);
                    }
                    turn.push(TurnServer {
                        uri: uri.to_owned(),
                        username: reader.secret(MAX_TURN_CREDENTIAL_BYTES, CharRule::NoControl)?,
                        credential: reader
                            .secret(MAX_TURN_CREDENTIAL_BYTES, CharRule::NoControl)?,
                    });
                }
                Self::Join {
                    sfu_url,
                    jwt,
                    turn,
                    relay_only: reader.bool()?,
                    own_identity: reader.identity()?,
                    own_key: reader.key()?,
                    recording_device: reader.device_choice()?,
                    playout_device: reader.device_choice()?,
                    camera_device: reader.device_choice()?,
                    camera_on: reader.bool()?,
                    volume: reader.volume()?,
                    auto_level: reader.bool()?,
                    mic_gain: reader.mic_gain()?,
                }
            }
            TO_LIST_DEVICES => Self::ListDevices,
            TO_SET_KEY => Self::SetKey {
                identity: reader.identity()?,
                key: reader.key()?,
            },
            TO_SET_OWN_KEY => Self::SetOwnKey { key: reader.key()? },
            TO_REMOVE_PARTICIPANT => Self::RemoveParticipant {
                identity: reader.identity()?,
            },
            TO_SET_MUTED => Self::SetMuted {
                muted: reader.bool()?,
            },
            TO_LEAVE => Self::Leave,
            TO_SET_CAMERA => {
                let on = reader.bool()?;
                let remote = reader.bool()?;
                // Only a camera-on carries a portal remote.
                if remote && !on {
                    return Err(DecodeError::InvalidValue);
                }
                Self::SetCamera { on, remote }
            }
            TO_SUBSCRIBE => {
                let index = reader.other_connection()?;
                let url = reader.str(MAX_URL_BYTES, CharRule::AsciiGraphic, false)?;
                validate_sfu_url(url)?;
                let url = url.to_owned();
                Self::Subscribe {
                    index,
                    url,
                    jwt: reader.secret(MAX_JWT_BYTES, CharRule::AsciiGraphic)?,
                }
            }
            TO_SET_SCREEN => {
                let share = reader.u32()?;
                let target = match reader.u8()? {
                    0 => None,
                    1 => Some(ScreenTarget::Portal {
                        node: reader.u32()?,
                    }),
                    2 => {
                        let index = reader.u8()?;
                        let width = reader.u32()?;
                        let height = reader.u32()?;
                        if index >= MAX_SCREEN_MONITORS
                            || !(1..=MAX_MONITOR_SIDE).contains(&width)
                            || !(1..=MAX_MONITOR_SIDE).contains(&height)
                        {
                            return Err(DecodeError::InvalidValue);
                        }
                        Some(ScreenTarget::Monitor {
                            index,
                            width,
                            height,
                        })
                    }
                    _ => return Err(DecodeError::InvalidValue),
                };
                Self::SetScreen { target, share }
            }
            TO_SET_DEAFENED => Self::SetDeafened {
                deafened: reader.bool()?,
            },
            TO_SET_VOLUME => Self::SetVolume {
                percent: reader.volume()?,
            },
            TO_SET_MICROPHONE => Self::SetMicrophone {
                auto_level: reader.bool()?,
                mic_gain: reader.mic_gain()?,
            },
            TO_SET_DIAGNOSTICS => Self::SetDiagnostics { on: reader.bool()? },
            TO_TEST_DEVICE => Self::TestDevice {
                test: DeviceTest::try_from(reader.u8()?)?,
                device: reader.device_choice()?,
            },
            TO_SET_DEVICES => Self::SetDevices {
                recording: reader.device_choice()?,
                playout: reader.device_choice()?,
                camera: reader.device_choice()?,
            },
            TO_SET_STATS => Self::SetStats { on: reader.bool()? },
            _ => return Err(DecodeError::UnknownTag),
        };
        reader.finish()?;
        Ok(message)
    }
}

impl FromWorker {
    /// A [`FromWorker::Devices`] reply that fits one message. Each list may
    /// hold [`MAX_DEVICES`] devices, but three full lists of the longest IDs
    /// and names encode to more than [`MAX_MESSAGE_BYTES`], which the writer
    /// refuses. Devices are therefore dropped from the end of the longest
    /// list until the reply fits. Build every `Devices` reply with this.
    pub fn devices(
        mut recording: Vec<MediaDevice>,
        mut playout: Vec<MediaDevice>,
        mut cameras: Vec<MediaDevice>,
    ) -> Self {
        fn list_len(list: &[MediaDevice]) -> usize {
            // The count, then each ID and name with its two-byte length.
            1 + list
                .iter()
                .map(|device| 4 + device.id.len() + device.name.len())
                .sum::<usize>()
        }
        loop {
            // The version and the tag, then the three lists.
            let size = 2 + list_len(&recording) + list_len(&playout) + list_len(&cameras);
            if size <= MAX_MESSAGE_BYTES {
                break;
            }
            let longest = [&mut recording, &mut playout, &mut cameras]
                .into_iter()
                .max_by_key(|list| list.len())
                .expect("three lists");
            longest.pop();
        }
        Self::Devices {
            recording,
            playout,
            cameras,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![VERSION];
        match self {
            Self::Ready { containment_bits } => {
                out.push(FROM_READY);
                out.extend_from_slice(&containment_bits.to_be_bytes());
            }
            Self::Connected => out.push(FROM_CONNECTED),
            Self::ParticipantJoined {
                connection,
                identity,
            } => {
                out.push(FROM_PARTICIPANT_JOINED);
                out.push(*connection);
                put_str(&mut out, identity);
            }
            Self::ParticipantLeft {
                connection,
                identity,
            } => {
                out.push(FROM_PARTICIPANT_LEFT);
                out.push(*connection);
                put_str(&mut out, identity);
            }
            Self::Speaking {
                connection,
                identity,
                speaking,
            } => {
                out.push(FROM_SPEAKING);
                out.push(*connection);
                put_str(&mut out, identity);
                out.push(u8::from(*speaking));
            }
            Self::Muted { muted } => {
                out.push(FROM_MUTED);
                out.push(u8::from(*muted));
            }
            Self::Deafened { deafened } => {
                out.push(FROM_DEAFENED);
                out.push(u8::from(*deafened));
            }
            Self::SoundLevel { level } => {
                out.push(FROM_SOUND_LEVEL);
                out.push(*level);
            }
            Self::ParticipantLevel {
                connection,
                identity,
                level,
            } => {
                out.push(FROM_PARTICIPANT_LEVEL);
                out.push(*connection);
                put_str(&mut out, identity);
                out.push(*level);
            }
            Self::ParticipantMuted {
                connection,
                identity,
                muted,
            } => {
                out.push(FROM_PARTICIPANT_MUTED);
                out.push(*connection);
                put_str(&mut out, identity);
                out.push(u8::from(*muted));
            }
            Self::DevicesSet { failure } => {
                out.push(FROM_DEVICES_SET);
                out.push(failure.map_or(0, |code| code as u8));
            }
            Self::Failed { code } => {
                out.push(FROM_FAILED);
                out.push(*code as u8);
            }
            Self::Left => out.push(FROM_LEFT),
            Self::Devices {
                recording,
                playout,
                cameras,
            } => {
                out.push(FROM_DEVICES);
                put_devices(&mut out, recording);
                put_devices(&mut out, playout);
                put_devices(&mut out, cameras);
            }
            Self::Camera { state } => {
                out.push(FROM_CAMERA);
                out.push(*state as u8);
            }
            Self::Screen { state, share } => {
                out.push(FROM_SCREEN);
                out.push(*state as u8);
                out.extend_from_slice(&share.to_be_bytes());
            }
            Self::ScreenSharer {
                connection,
                identity,
                sharing,
            } => {
                out.push(FROM_SCREEN_SHARER);
                out.push(*connection);
                put_str(&mut out, identity);
                out.push(u8::from(*sharing));
            }
            Self::VideoStarted {
                tile,
                connection,
                identity,
            } => {
                out.push(FROM_VIDEO_STARTED);
                out.push(*tile);
                out.push(*connection);
                put_str(&mut out, identity);
            }
            Self::Connection { connection, up } => {
                out.push(FROM_CONNECTION);
                out.push(*connection);
                out.push(u8::from(*up));
            }
            Self::Diagnostic {
                level,
                target,
                message,
            } => {
                out.push(FROM_DIAGNOSTIC);
                out.push(*level);
                put_str(&mut out, target);
                put_str(&mut out, message);
            }
            Self::Stats(stats) => {
                out.push(FROM_STATS);
                stats::put_stats(&mut out, stats);
            }
            Self::VideoStopped { tile } => {
                out.push(FROM_VIDEO_STOPPED);
                out.push(*tile);
            }
            Self::VideoFrame { tile, rotation } => {
                out.push(FROM_VIDEO_FRAME);
                out.push(*tile);
                out.push(*rotation);
            }
        }
        out
    }

    pub fn decode(body: &[u8]) -> Result<Self, DecodeError> {
        let (tag, mut reader) = open(body)?;
        let message = match tag {
            FROM_READY => Self::Ready {
                containment_bits: reader.u32()?,
            },
            FROM_CONNECTED => Self::Connected,
            FROM_PARTICIPANT_JOINED => Self::ParticipantJoined {
                connection: reader.connection()?,
                identity: reader.identity()?,
            },
            FROM_PARTICIPANT_LEFT => Self::ParticipantLeft {
                connection: reader.connection()?,
                identity: reader.identity()?,
            },
            FROM_SPEAKING => Self::Speaking {
                connection: reader.connection()?,
                identity: reader.identity()?,
                speaking: reader.bool()?,
            },
            FROM_MUTED => Self::Muted {
                muted: reader.bool()?,
            },
            FROM_DEAFENED => Self::Deafened {
                deafened: reader.bool()?,
            },
            FROM_SOUND_LEVEL => Self::SoundLevel {
                level: reader.sound_level()?,
            },
            FROM_PARTICIPANT_LEVEL => Self::ParticipantLevel {
                connection: reader.connection()?,
                identity: reader.identity()?,
                level: reader.sound_level()?,
            },
            FROM_PARTICIPANT_MUTED => Self::ParticipantMuted {
                connection: reader.connection()?,
                identity: reader.identity()?,
                muted: reader.bool()?,
            },
            FROM_DEVICES_SET => Self::DevicesSet {
                failure: match reader.u8()? {
                    0 => None,
                    code => Some(FailureCode::try_from(code)?),
                },
            },
            FROM_FAILED => Self::Failed {
                code: FailureCode::try_from(reader.u8()?)?,
            },
            FROM_LEFT => Self::Left,
            FROM_DEVICES => Self::Devices {
                recording: reader.devices()?,
                playout: reader.devices()?,
                cameras: reader.devices()?,
            },
            FROM_CAMERA => Self::Camera {
                state: CameraState::try_from(reader.u8()?)?,
            },
            FROM_SCREEN => Self::Screen {
                state: ScreenState::try_from(reader.u8()?)?,
                share: reader.u32()?,
            },
            FROM_SCREEN_SHARER => Self::ScreenSharer {
                connection: reader.connection()?,
                identity: reader.identity()?,
                sharing: reader.bool()?,
            },
            FROM_VIDEO_STARTED => Self::VideoStarted {
                tile: reader.tile(1)?,
                connection: reader.connection()?,
                identity: reader.identity()?,
            },
            FROM_CONNECTION => Self::Connection {
                connection: reader.other_connection()?,
                up: reader.bool()?,
            },
            FROM_DIAGNOSTIC => {
                let level = reader.u8()?;
                if !(1..=MAX_DIAGNOSTIC_LEVEL).contains(&level) {
                    return Err(DecodeError::InvalidValue);
                }
                Self::Diagnostic {
                    level,
                    target: reader
                        .str(MAX_DIAGNOSTIC_TARGET_BYTES, CharRule::NoControl, false)?
                        .to_owned(),
                    message: reader
                        .str(MAX_DIAGNOSTIC_MESSAGE_BYTES, CharRule::NoControl, true)?
                        .to_owned(),
                }
            }
            FROM_VIDEO_STOPPED => Self::VideoStopped {
                tile: reader.tile(0)?,
            },
            FROM_VIDEO_FRAME => {
                let tile = reader.tile(0)?;
                let rotation = reader.u8()?;
                if rotation > MAX_VIDEO_ROTATION {
                    return Err(DecodeError::InvalidValue);
                }
                Self::VideoFrame { tile, rotation }
            }
            FROM_STATS => Self::Stats(stats::read_stats(&mut reader)?),
            _ => return Err(DecodeError::UnknownTag),
        };
        reader.finish()?;
        Ok(message)
    }
}

/// A message type that travels in a Control frame.
///
/// The encoded body is cleared when dropped. On the reading side the
/// media-protocol frame keeps its own copy of the body, which is not cleared;
/// for `ToWorker` that copy exists only in the worker, which holds the same
/// secrets for the call anyway.
pub trait Message: Sized {
    fn to_body(&self) -> Zeroizing<Vec<u8>>;
    fn from_body(body: &[u8]) -> Result<Self, DecodeError>;
}

impl Message for ToWorker {
    fn to_body(&self) -> Zeroizing<Vec<u8>> {
        self.encode()
    }
    fn from_body(body: &[u8]) -> Result<Self, DecodeError> {
        Self::decode(body)
    }
}

impl Message for FromWorker {
    fn to_body(&self) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(self.encode())
    }
    fn from_body(body: &[u8]) -> Result<Self, DecodeError> {
        Self::decode(body)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum ReadError {
    Frame(FrameError),
    /// The frame was not a Control frame.
    NotControl,
    Decode(DecodeError),
    /// After a `VideoFrame`, the next frame was not an `RgbaPixels` frame.
    NotPixels,
    /// After a `VideoFrame`, the pixel frame is over the video limits. Its
    /// body was not read.
    VideoLimit,
}

/// What the core reads from the worker: a message, or a video picture.
#[derive(Debug, Eq, PartialEq)]
pub enum WorkerItem {
    Message(FromWorker),
    /// A [`FromWorker::VideoFrame`] with its pixel frame: `width` by `height`
    /// RGBA pixels, bytes in the order R, G, B, A.
    Video {
        tile: u8,
        rotation: u8,
        width: u32,
        height: u32,
        rgba: Pixels,
    },
}

/// The RGBA bytes of one received picture and the budget reservation that
/// covers them. The reservation ends only when the bytes are dropped, so a
/// kept picture stays charged to the reader's [`ResourceBudgets`].
pub struct Pixels {
    bytes: Vec<u8>,
    _permit: BudgetPermit,
}

impl Pixels {
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }
}

impl AsRef<[u8]> for Pixels {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl std::ops::Deref for Pixels {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}

/// Pictures compare by their bytes; the reservation is not part of the value.
impl PartialEq for Pixels {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for Pixels {}

impl std::fmt::Debug for Pixels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Pixels({} bytes)", self.bytes.len())
    }
}

/// Writes one message as a Control frame and flushes it.
pub fn write_message<W: Write, M: Message>(writer: &mut W, message: &M) -> io::Result<()> {
    let body = message.to_body();
    if body.len() > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "call message is too large",
        ));
    }
    let header = FrameHeader {
        kind: FrameKind::Control,
        length: body.len() as u64,
        width: 0,
        height: 0,
    };
    write_frame(writer, header, &body)
}

/// Reads one Control frame under the control budget and decodes it. The
/// budget permit is held only while the body is decoded.
pub fn read_message<R: Read, M: Message>(
    reader: &mut R,
    budgets: &ResourceBudgets,
) -> Result<M, ReadError> {
    let frame = read_frame(reader, budgets).map_err(ReadError::Frame)?;
    if frame.header().kind != FrameKind::Control {
        return Err(ReadError::NotControl);
    }
    M::from_body(frame.body()).map_err(ReadError::Decode)
}

/// The header of a pixel frame is inside the limits of its tile.
fn video_header_fits(tile: u8, header: &FrameHeader) -> bool {
    header.kind == FrameKind::RgbaPixels
        && picture_fits(tile, header.width, header.height)
        && header.length == u64::from(header.width) * u64::from(header.height) * 4
}

/// Reads one message from the worker (the core side). After a
/// [`FromWorker::VideoFrame`], the next frame must be an `RgbaPixels` frame
/// inside the video limits. That header is checked before the body is
/// allocated or its budget reserved. A missing, wrong-kind or over-limit
/// pixel frame is an error, and the caller must treat the worker as failed.
pub fn read_worker_item<R: Read>(
    reader: &mut R,
    budgets: &ResourceBudgets,
) -> Result<WorkerItem, ReadError> {
    let message: FromWorker = read_message(reader, budgets)?;
    let FromWorker::VideoFrame { tile, rotation } = message else {
        return Ok(WorkerItem::Message(message));
    };
    let mut wire = [0; FRAME_HEADER_LEN];
    reader
        .read_exact(&mut wire)
        .map_err(|error| ReadError::Frame(FrameError::Io(error.kind())))?;
    let header = FrameHeader::decode(wire).map_err(ReadError::Frame)?;
    if header.kind != FrameKind::RgbaPixels {
        return Err(ReadError::NotPixels);
    }
    if !video_header_fits(tile, &header) {
        return Err(ReadError::VideoLimit);
    }
    // The header was checked; give it back to the budgeted reader together
    // with the body that follows it.
    let frame = read_frame(&mut (&wire[..]).chain(reader), budgets).map_err(ReadError::Frame)?;
    let (bytes, permit) = frame.into_parts();
    Ok(WorkerItem::Video {
        tile,
        rotation,
        width: header.width,
        height: header.height,
        rgba: Pixels {
            bytes,
            _permit: permit,
        },
    })
}

/// Writes a [`FromWorker::VideoFrame`] and its pixel frame as one unit (the
/// worker side). Everything is checked before anything is written: the tile,
/// the rotation, the size limits, and that `rgba` is exactly
/// `width * height * 4` bytes. The caller holds its output lock across this
/// call, so no other frame comes between the two.
pub fn write_video_frame<W: Write>(
    writer: &mut W,
    tile: u8,
    rotation: u8,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> io::Result<()> {
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidInput, message.to_owned());
    if tile >= VIDEO_TILES || rotation > MAX_VIDEO_ROTATION {
        return Err(invalid("video tile or rotation out of range"));
    }
    let header = FrameHeader {
        kind: FrameKind::RgbaPixels,
        length: rgba.len() as u64,
        width,
        height,
    };
    if !video_header_fits(tile, &header) {
        return Err(invalid("video picture outside the video limits"));
    }
    write_message(writer, &FromWorker::VideoFrame { tile, rotation })?;
    write_frame(writer, header, rgba)
}

#[cfg(test)]
mod tests {
    use super::stats::tests::full_stats;
    use super::*;
    use std::io::Cursor;

    fn key(index: u8, len: usize) -> FrameKeyMsg {
        FrameKeyMsg {
            index,
            bytes: Zeroizing::new(vec![0xA5; len]),
        }
    }

    fn device(id: &str, name: &str) -> MediaDevice {
        MediaDevice {
            id: id.to_owned(),
            name: name.to_owned(),
        }
    }

    /// GIVEN sound server device names WHEN checked THEN real PulseAudio and
    /// PipeWire names pass, and empty, long, spaced, `=`, NUL, slash and
    /// non-ASCII names do not; an ID round-trips only with the prefix.
    #[test]
    fn given_pulse_names_then_only_safe_names_make_ids() {
        for good in [
            "alsa_output.pci-0000_00_1f.3.analog-stereo",
            "alsa_input.usb-Logitech_USB_Headset-00.mono-fallback",
            "bluez_output.00_1B_66_A1_B2_C3.1",
            "auto_null",
            "a:b",
            &"x".repeat(MAX_PULSE_NAME_BYTES),
        ] {
            assert!(valid_pulse_name(good), "{good}");
            let id = pulse_id(good).expect(good);
            assert_eq!(pulse_name(&id), Some(good));
        }
        for bad in [
            "",
            "two words",
            "a=b",
            "a\0b",
            "../x",
            "café",
            "a\nb",
            &"x".repeat(MAX_PULSE_NAME_BYTES + 1),
        ] {
            assert!(!valid_pulse_name(bad), "{bad:?}");
            assert_eq!(pulse_id(bad), None, "{bad:?}");
        }
        assert_eq!(pulse_name("auto_null"), None, "no prefix");
        assert_eq!(pulse_name("pulse:"), None, "no name");
        assert_eq!(pulse_name("index:0"), None);
        assert_eq!(pulse_name("pulse:a b"), None);
        // The longest ID fits the wire.
        assert!(PULSE_PREFIX.len() + MAX_PULSE_NAME_BYTES <= MAX_DEVICE_ID_BYTES);
    }

    fn turn(uri: &str) -> TurnServer {
        TurnServer {
            uri: uri.to_owned(),
            username: Zeroizing::new("SECRETUSER".to_owned()),
            credential: Zeroizing::new("SECRETCRED".to_owned()),
        }
    }

    fn join() -> ToWorker {
        ToWorker::Join {
            sfu_url: "wss://sfu.example.org/livekit".to_owned(),
            jwt: Zeroizing::new("SECRETJWT.payload.sig".to_owned()),
            turn: vec![
                turn("turn:turn.example.org:3478"),
                turn("turns:t.example.org"),
            ],
            relay_only: true,
            own_identity: "@alice:example.org:DEVICE".to_owned(),
            own_key: key(3, 16),
            recording_device: Some("{0.0.1.00000000}.{6a2c}".to_owned()),
            playout_device: None,
            camera_device: Some("usb#vid_046d&pid_0825#cam".to_owned()),
            camera_on: true,
            volume: 65,
            auto_level: false,
            mic_gain: 250,
        }
    }

    /// Join with one field replaced, to test a limit.
    fn join_with(
        url: &str,
        jwt: &str,
        turns: Vec<TurnServer>,
        identity: &str,
        own_key: FrameKeyMsg,
    ) -> Vec<u8> {
        ToWorker::Join {
            sfu_url: url.to_owned(),
            jwt: Zeroizing::new(jwt.to_owned()),
            turn: turns,
            relay_only: false,
            own_identity: identity.to_owned(),
            own_key,
            recording_device: None,
            playout_device: None,
            camera_device: None,
            camera_on: false,
            volume: MAX_VOLUME_PERCENT,
            auto_level: true,
            mic_gain: DEFAULT_MIC_GAIN_PERCENT,
        }
        .encode()
        .to_vec()
    }

    fn subscribe(index: u8, url: &str) -> ToWorker {
        ToWorker::Subscribe {
            index,
            url: url.to_owned(),
            jwt: Zeroizing::new("a.b.c".to_owned()),
        }
    }

    fn all_to_worker() -> Vec<ToWorker> {
        vec![
            join(),
            ToWorker::SetKey {
                identity: "@bob:example.org:D".to_owned(),
                key: key(255, 64),
            },
            ToWorker::SetOwnKey { key: key(0, 32) },
            ToWorker::RemoveParticipant {
                identity: "@bob:example.org:D".to_owned(),
            },
            ToWorker::SetMuted { muted: true },
            ToWorker::Leave,
            ToWorker::ListDevices,
            ToWorker::SetCamera {
                on: true,
                remote: false,
            },
            ToWorker::SetCamera {
                on: true,
                remote: true,
            },
            subscribe(1, "wss://sfu.other.example"),
            subscribe(3, "wss://sfu.example.org:7881/livekit?x=1"),
            ToWorker::SetCamera {
                on: false,
                remote: false,
            },
            ToWorker::SetScreen {
                target: None,
                share: 0,
            },
            ToWorker::SetScreen {
                target: Some(ScreenTarget::Portal {
                    node: 4_294_967_295,
                }),
                share: 4_294_967_295,
            },
            ToWorker::SetScreen {
                target: Some(ScreenTarget::Monitor {
                    index: 15,
                    width: 16_384,
                    height: 1,
                }),
                share: 7,
            },
            ToWorker::SetDeafened { deafened: true },
            ToWorker::SetDeafened { deafened: false },
            ToWorker::SetVolume { percent: 0 },
            ToWorker::SetVolume {
                percent: MAX_VOLUME_PERCENT,
            },
            ToWorker::SetDiagnostics { on: true },
            ToWorker::SetDiagnostics { on: false },
            ToWorker::SetStats { on: true },
            ToWorker::SetStats { on: false },
            ToWorker::TestDevice {
                test: DeviceTest::Microphone,
                device: None,
            },
            ToWorker::TestDevice {
                test: DeviceTest::Speaker,
                device: Some("{0.0.0.00000000}.{6a2c}".to_owned()),
            },
            ToWorker::TestDevice {
                test: DeviceTest::Camera,
                device: Some("usb#cam".to_owned()),
            },
            ToWorker::SetDevices {
                recording: Some("m".to_owned()),
                playout: None,
                camera: None,
            },
            ToWorker::SetDevices {
                recording: None,
                playout: Some("p".to_owned()),
                camera: Some("usb#cam".to_owned()),
            },
        ]
    }

    fn all_from_worker() -> Vec<FromWorker> {
        let mut all = vec![
            FromWorker::Ready {
                containment_bits: 0x8000_0021,
            },
            FromWorker::Connected,
            FromWorker::ParticipantJoined {
                connection: 0,
                identity: "@bob:example.org:D".to_owned(),
            },
            FromWorker::ParticipantLeft {
                connection: 0,
                identity: "ünï".to_owned(),
            },
            FromWorker::Speaking {
                connection: 0,
                identity: "@bob:example.org:D".to_owned(),
                speaking: true,
            },
            FromWorker::Muted { muted: false },
            FromWorker::Deafened { deafened: true },
            FromWorker::Deafened { deafened: false },
            FromWorker::SoundLevel { level: 0 },
            FromWorker::SoundLevel {
                level: MAX_SOUND_LEVEL,
            },
            FromWorker::ParticipantLevel {
                connection: 2,
                identity: "@bob:example.org:D".to_owned(),
                level: MAX_SOUND_LEVEL,
            },
            FromWorker::ParticipantMuted {
                connection: 0,
                identity: "@bob:example.org:D".to_owned(),
                muted: true,
            },
            FromWorker::DevicesSet { failure: None },
            FromWorker::DevicesSet {
                failure: Some(FailureCode::DeviceNotFound),
            },
            FromWorker::Left,
            FromWorker::Devices {
                recording: vec![device("{0.0.1.00000000}.{6a2c}", "Microphone (Headset)")],
                playout: vec![device("p", ""), device("q", "Lautsprecher (Ü)")],
                cameras: vec![device("usb#cam", "Integrated Camera")],
            },
            FromWorker::Devices {
                recording: Vec::new(),
                playout: Vec::new(),
                cameras: Vec::new(),
            },
            FromWorker::Camera {
                state: CameraState::Off,
            },
            FromWorker::Camera {
                state: CameraState::On,
            },
            FromWorker::Camera {
                state: CameraState::NotFound,
            },
            FromWorker::Camera {
                state: CameraState::Unavailable,
            },
            FromWorker::Screen {
                state: ScreenState::Off,
                share: 3,
            },
            FromWorker::Screen {
                state: ScreenState::Starting,
                share: 3,
            },
            FromWorker::Screen {
                state: ScreenState::On,
                share: 3,
            },
            FromWorker::Screen {
                state: ScreenState::NotFound,
                share: 3,
            },
            FromWorker::Screen {
                state: ScreenState::Unavailable,
                share: 3,
            },
            FromWorker::Screen {
                state: ScreenState::Refused,
                share: 3,
            },
            FromWorker::ScreenSharer {
                connection: 0,
                identity: "@bob:example.org:D".to_owned(),
                sharing: true,
            },
            FromWorker::ScreenSharer {
                connection: 0,
                identity: "ünï".to_owned(),
                sharing: false,
            },
            FromWorker::VideoStarted {
                tile: 1,
                connection: 0,
                identity: "@bob:example.org:D".to_owned(),
            },
            FromWorker::VideoStarted {
                tile: 4,
                connection: 0,
                identity: "ünï".to_owned(),
            },
            FromWorker::VideoStarted {
                tile: SCREEN_TILE,
                connection: 0,
                identity: "@bob:example.org:D".to_owned(),
            },
            FromWorker::ParticipantJoined {
                connection: 3,
                identity: "@carol:other.example:D".to_owned(),
            },
            FromWorker::Speaking {
                connection: 2,
                identity: "@carol:other.example:D".to_owned(),
                speaking: true,
            },
            FromWorker::VideoStarted {
                tile: 2,
                connection: 1,
                identity: "@carol:other.example:D".to_owned(),
            },
            FromWorker::Connection {
                connection: 1,
                up: true,
            },
            FromWorker::Connection {
                connection: 3,
                up: false,
            },
            FromWorker::Diagnostic {
                level: 4,
                target: "livekit::room".to_owned(),
                message: "track subscribed: TR_x from 1qaz".to_owned(),
            },
            FromWorker::Diagnostic {
                level: 1,
                target: "t".to_owned(),
                message: String::new(),
            },
            FromWorker::Stats(CallStats::default()),
            FromWorker::Stats(full_stats()),
            FromWorker::VideoStopped { tile: 0 },
            FromWorker::VideoStopped { tile: 4 },
            FromWorker::VideoStopped { tile: SCREEN_TILE },
            FromWorker::VideoFrame {
                tile: 0,
                rotation: 0,
            },
            FromWorker::VideoFrame {
                tile: 4,
                rotation: 3,
            },
            FromWorker::VideoFrame {
                tile: SCREEN_TILE,
                rotation: 0,
            },
        ];
        for code in 1..=12 {
            all.push(FromWorker::Failed {
                code: FailureCode::try_from(code).unwrap(),
            });
        }
        all
    }

    /// GIVEN every core-to-worker and worker-to-core message
    /// WHEN each is encoded and decoded
    /// THEN the original message returns.
    #[test]
    fn given_every_message_when_round_tripped_then_it_is_unchanged() {
        for message in all_to_worker() {
            assert_eq!(ToWorker::decode(&message.encode()), Ok(message));
        }
        for message in all_from_worker() {
            assert_eq!(FromWorker::decode(&message.encode()), Ok(message));
        }
    }

    /// GIVEN a message written as a frame
    /// WHEN it is read back with the control budget
    /// THEN it decodes and the budget is released.
    #[test]
    fn given_framed_message_when_read_then_it_decodes_and_budget_is_released() {
        let mut wire = Vec::new();
        write_message(&mut wire, &join()).unwrap();
        let budgets = ResourceBudgets::default();
        let read: ToWorker = read_message(&mut Cursor::new(wire), &budgets).unwrap();
        assert_eq!(read, join());
        assert_eq!(budgets.aggregate_used(), 0);
    }

    /// GIVEN a frame that is not a Control frame
    /// WHEN it is read as a call message
    /// THEN the read fails with `NotControl`.
    #[test]
    fn given_encoded_image_frame_when_read_then_not_control_is_returned() {
        let mut wire = Vec::new();
        let header = FrameHeader {
            kind: FrameKind::EncodedImage,
            length: 2,
            width: 0,
            height: 0,
        };
        write_frame(&mut wire, header, &[1, 1]).unwrap();
        let result: Result<FromWorker, _> =
            read_message(&mut Cursor::new(wire), &ResourceBudgets::default());
        assert_eq!(result, Err(ReadError::NotControl));
    }

    /// GIVEN a frame whose body is not a valid message
    /// WHEN it is read
    /// THEN the decode error is returned.
    #[test]
    fn given_garbage_control_frame_when_read_then_decode_error_is_returned() {
        let mut wire = Vec::new();
        let header = FrameHeader {
            kind: FrameKind::Control,
            length: 2,
            width: 0,
            height: 0,
        };
        write_frame(&mut wire, header, &[VERSION + 1, 1]).unwrap();
        let result: Result<FromWorker, _> =
            read_message(&mut Cursor::new(wire), &ResourceBudgets::default());
        assert_eq!(result, Err(ReadError::Decode(DecodeError::UnknownVersion)));
    }

    /// GIVEN each field one byte over its limit
    /// WHEN the Join message is decoded
    /// THEN it is rejected as too large.
    #[test]
    fn given_field_over_limit_when_decoded_then_it_is_rejected() {
        let url = format!("wss://{}", "a".repeat(MAX_URL_BYTES + 1 - 6));
        let ok_url = format!("wss://{}", "a".repeat(MAX_URL_BYTES - 6));
        let jwt = "j".repeat(MAX_JWT_BYTES + 1);
        let ok_jwt = "j".repeat(MAX_JWT_BYTES);
        let identity = "i".repeat(MAX_IDENTITY_BYTES + 1);
        let ok_identity = "i".repeat(MAX_IDENTITY_BYTES);
        let uri = format!("turn:{}", "u".repeat(MAX_TURN_URI_BYTES + 1 - 5));
        let ok_uri = format!("turn:{}", "u".repeat(MAX_TURN_URI_BYTES - 5));
        let long_secret = Zeroizing::new("s".repeat(MAX_TURN_CREDENTIAL_BYTES + 1));
        let short = "x";

        let good = join_with(
            &ok_url,
            &ok_jwt,
            vec![turn(&ok_uri)],
            &ok_identity,
            key(0, 64),
        );
        assert!(ToWorker::decode(&good).is_ok());

        let cases = [
            join_with(&url, short, vec![], short, key(0, 16)),
            join_with("wss://h", &jwt, vec![], short, key(0, 16)),
            join_with("wss://h", short, vec![], &identity, key(0, 16)),
            join_with("wss://h", short, vec![turn(&uri)], short, key(0, 16)),
            join_with(
                "wss://h",
                short,
                vec![TurnServer {
                    username: long_secret.clone(),
                    ..turn("turn:a")
                }],
                short,
                key(0, 16),
            ),
            join_with(
                "wss://h",
                short,
                vec![TurnServer {
                    credential: long_secret,
                    ..turn("turn:a")
                }],
                short,
                key(0, 16),
            ),
            join_with(
                "wss://h",
                short,
                (0..=MAX_TURN_URIS).map(|_| turn("turn:a")).collect(),
                short,
                key(0, 16),
            ),
            join_with("wss://h", short, vec![], short, key(0, 65)),
        ];
        for case in cases {
            assert_eq!(ToWorker::decode(&case), Err(DecodeError::TooLarge));
        }
        let long_identity = FromWorker::ParticipantJoined {
            connection: 0,
            identity: identity.clone(),
        };
        assert_eq!(
            FromWorker::decode(&long_identity.encode()),
            Err(DecodeError::TooLarge)
        );
    }

    /// GIVEN key lengths of 15 and 65 bytes, and an empty identity
    /// WHEN the messages are decoded
    /// THEN each is rejected.
    #[test]
    fn given_bad_key_length_or_empty_identity_when_decoded_then_it_is_rejected() {
        let short = ToWorker::SetOwnKey { key: key(0, 15) }.encode();
        assert_eq!(ToWorker::decode(&short), Err(DecodeError::TooShort));
        let long = ToWorker::SetOwnKey { key: key(0, 65) }.encode();
        assert_eq!(ToWorker::decode(&long), Err(DecodeError::TooLarge));
        let empty = ToWorker::RemoveParticipant {
            identity: String::new(),
        };
        assert_eq!(
            ToWorker::decode(&empty.encode()),
            Err(DecodeError::TooShort)
        );
    }

    /// GIVEN a valid Join message
    /// WHEN it is cut at every byte offset
    /// THEN each shorter input is rejected and none panics.
    #[test]
    fn given_truncated_join_at_every_offset_when_decoded_then_it_is_rejected() {
        let full = join().encode();
        assert!(ToWorker::decode(&full).is_ok());
        for cut in 0..full.len() {
            assert!(ToWorker::decode(&full[..cut]).is_err(), "cut at {cut}");
        }
    }

    /// GIVEN a valid message with one extra byte
    /// WHEN it is decoded
    /// THEN it is rejected for trailing bytes.
    #[test]
    fn given_trailing_byte_when_decoded_then_it_is_rejected() {
        for message in all_to_worker() {
            let mut body = message.encode();
            body.push(0);
            assert_eq!(ToWorker::decode(&body), Err(DecodeError::TrailingBytes));
        }
        for message in all_from_worker() {
            let mut body = message.encode();
            body.push(0);
            assert_eq!(FromWorker::decode(&body), Err(DecodeError::TrailingBytes));
        }
    }

    /// GIVEN an unknown version, an unknown tag, or an oversize body
    /// WHEN it is decoded
    /// THEN it is rejected.
    #[test]
    fn given_unknown_version_tag_or_oversize_body_when_decoded_then_it_is_rejected() {
        assert_eq!(
            ToWorker::decode(&[1, TO_LEAVE]),
            Err(DecodeError::UnknownVersion)
        );
        assert_eq!(
            ToWorker::decode(&[VERSION, 0]),
            Err(DecodeError::UnknownTag)
        );
        assert_eq!(
            ToWorker::decode(&[VERSION, 18]),
            Err(DecodeError::UnknownTag)
        );
        assert_eq!(
            FromWorker::decode(&[VERSION, 24]),
            Err(DecodeError::UnknownTag)
        );
        assert_eq!(FromWorker::decode(&[]), Err(DecodeError::Truncated));
        let oversize = vec![VERSION; MAX_MESSAGE_BYTES + 1];
        assert_eq!(FromWorker::decode(&oversize), Err(DecodeError::TooLarge));
    }

    /// GIVEN a failure code outside the closed set, or a boolean other than 0 or 1
    /// WHEN the message is decoded
    /// THEN it is rejected.
    #[test]
    fn given_invalid_code_or_bool_when_decoded_then_it_is_rejected() {
        assert_eq!(
            FromWorker::decode(&[VERSION, FROM_FAILED, 0]),
            Err(DecodeError::InvalidValue)
        );
        assert_eq!(
            FromWorker::decode(&[VERSION, FROM_FAILED, 13]),
            Err(DecodeError::InvalidValue)
        );
        assert_eq!(
            ToWorker::decode(&[VERSION, TO_SET_MUTED, 2]),
            Err(DecodeError::InvalidValue)
        );
    }

    /// GIVEN an identity that is not valid UTF-8
    /// WHEN the message is decoded
    /// THEN it is rejected.
    #[test]
    fn given_invalid_utf8_identity_when_decoded_then_it_is_rejected() {
        let body = [VERSION, FROM_PARTICIPANT_LEFT, 0, 0, 2, 0xC3, 0x28];
        assert_eq!(FromWorker::decode(&body), Err(DecodeError::InvalidUtf8));
    }

    /// GIVEN identities with control characters
    /// WHEN the messages are decoded
    /// THEN they are rejected.
    #[test]
    fn given_control_character_in_identity_when_decoded_then_it_is_rejected() {
        for bad in ["a\nb", "a\0b", "a\u{7f}b", "a\u{85}b"] {
            let message = FromWorker::ParticipantJoined {
                connection: 0,
                identity: bad.to_owned(),
            };
            assert_eq!(
                FromWorker::decode(&message.encode()),
                Err(DecodeError::InvalidCharacter),
                "{bad:?}"
            );
        }
    }

    /// GIVEN a JWT with a space
    /// WHEN the Join message is decoded
    /// THEN it is rejected as not ASCII graphic.
    #[test]
    fn given_jwt_with_space_when_decoded_then_it_is_rejected() {
        let body = join_with("wss://h", "a b", vec![], "id", key(0, 16));
        assert_eq!(ToWorker::decode(&body), Err(DecodeError::InvalidCharacter));
    }

    /// GIVEN SFU URLs with a plain or lookalike scheme
    /// WHEN the Join message is decoded
    /// THEN every URL other than `wss://` host is rejected.
    #[test]
    fn given_non_wss_sfu_url_when_decoded_then_it_is_rejected() {
        let mut bad = vec![
            "https://sfu.example.org",
            "ws://sfu.example.org",
            "wss://",
            "WSS://sfu.example.org",
            "ws://127.0.0.1.evil.example",
            "ws://[::1]x",
        ];
        if !cfg!(feature = "dev-disposable-servers") {
            bad.extend(["ws://127.0.0.1:7880", "ws://[::1]:7880"]);
        }
        for url in bad {
            let body = join_with(url, "t", vec![], "id", key(0, 16));
            assert_eq!(
                ToWorker::decode(&body),
                Err(DecodeError::InvalidValue),
                "{url}"
            );
        }
    }

    /// GIVEN loopback `ws://` URLs and the dev feature
    /// WHEN the Join message is decoded
    /// THEN it is accepted.
    #[cfg(feature = "dev-disposable-servers")]
    #[test]
    fn given_dev_feature_and_loopback_ws_url_when_decoded_then_it_is_accepted() {
        for url in ["ws://127.0.0.1:7880", "ws://127.0.0.1", "ws://[::1]:7880/x"] {
            let body = join_with(url, "t", vec![], "id", key(0, 16));
            assert!(ToWorker::decode(&body).is_ok(), "{url}");
        }
    }

    /// GIVEN a TURN URI without a turn or turns prefix
    /// WHEN the Join message is decoded
    /// THEN it is rejected.
    #[test]
    fn given_bad_turn_prefix_when_decoded_then_it_is_rejected() {
        for uri in ["stun:s.example.org", "http://x", "turnx:y", "TURN:y"] {
            let body = join_with("wss://h", "t", vec![turn(uri)], "id", key(0, 16));
            assert_eq!(
                ToWorker::decode(&body),
                Err(DecodeError::InvalidValue),
                "{uri}"
            );
        }
    }

    /// GIVEN messages that hold secrets
    /// WHEN they are formatted with Debug
    /// THEN no secret text or key byte value appears.
    #[test]
    fn given_secret_messages_when_debug_formatted_then_no_secret_is_printed() {
        let set_key = ToWorker::SetKey {
            identity: "bob".to_owned(),
            key: FrameKeyMsg {
                index: 1,
                bytes: Zeroizing::new(vec![0xAB; 16]),
            },
        };
        let shown = format!("{:?}\n{:#?}\n{:?}", join(), join(), set_key);
        for secret in [
            "SECRETJWT",
            "SECRETUSER",
            "SECRETCRED",
            "165",
            "171",
            "0xa5",
            "0xab",
        ] {
            assert!(!shown.contains(secret), "leaked {secret}");
        }
        let server = format!("{:?}", turn("turn:a"));
        assert!(!server.contains("SECRET"));
        assert!(server.contains("turn:a"));
    }

    /// Join with only the speaker replaced.
    fn join_with_playout(playout: &str) -> ToWorker {
        let ToWorker::Join {
            sfu_url,
            jwt,
            turn,
            relay_only,
            own_identity,
            own_key,
            ..
        } = join()
        else {
            unreachable!()
        };
        ToWorker::Join {
            sfu_url,
            jwt,
            turn,
            relay_only,
            own_identity,
            own_key,
            recording_device: None,
            playout_device: Some(playout.to_owned()),
            camera_device: None,
            camera_on: false,
            volume: MAX_VOLUME_PERCENT,
            auto_level: true,
            mic_gain: DEFAULT_MIC_GAIN_PERCENT,
        }
    }

    /// GIVEN three full device lists with the longest IDs and names
    /// WHEN the reply is built
    /// THEN it fits one message, keeps the most devices that fit, and
    /// small lists are unchanged.
    #[test]
    fn given_three_full_device_lists_when_built_then_the_reply_fits() {
        let full = |prefix: char| -> Vec<MediaDevice> {
            (0..MAX_DEVICES)
                .map(|n| {
                    let id = format!("{prefix}{n:02}").repeat(MAX_DEVICE_ID_BYTES / 3);
                    device(&id, &"n".repeat(MAX_DEVICE_NAME_BYTES))
                })
                .collect()
        };
        let unfitted = FromWorker::Devices {
            recording: full('r'),
            playout: full('p'),
            cameras: full('c'),
        };
        assert!(unfitted.encode().len() > MAX_MESSAGE_BYTES);
        let fitted = FromWorker::devices(full('r'), full('p'), full('c'));
        let encoded = fitted.encode();
        assert!(encoded.len() <= MAX_MESSAGE_BYTES);
        assert!(write_message(&mut Vec::new(), &fitted).is_ok());
        assert_eq!(FromWorker::decode(&encoded), Ok(fitted.clone()));
        let FromWorker::Devices {
            recording,
            playout,
            cameras,
        } = fitted
        else {
            panic!("not a device list");
        };
        // Lists shrink evenly, and one more device would not fit.
        let lens = [recording.len(), playout.len(), cameras.len()];
        assert!(lens.iter().max().unwrap() - lens.iter().min().unwrap() <= 1);
        assert!(
            encoded.len() + 4 + MAX_DEVICE_ID_BYTES + MAX_DEVICE_NAME_BYTES > MAX_MESSAGE_BYTES
        );
        // Lists that fit are kept as they are.
        let small = FromWorker::devices(
            vec![device("r", "R")],
            vec![device("p", "P")],
            vec![device("c", "C")],
        );
        assert_eq!(
            small,
            FromWorker::Devices {
                recording: vec![device("r", "R")],
                playout: vec![device("p", "P")],
                cameras: vec![device("c", "C")],
            }
        );
    }

    /// GIVEN device lists or device IDs over their limits, or with a
    /// control character
    /// WHEN the messages are decoded
    /// THEN each is rejected.
    #[test]
    fn given_device_fields_over_limit_when_decoded_then_they_are_rejected() {
        let long_id = "d".repeat(MAX_DEVICE_ID_BYTES + 1);
        let long_name = "n".repeat(MAX_DEVICE_NAME_BYTES + 1);
        let list = |recording: Vec<MediaDevice>| FromWorker::Devices {
            recording,
            playout: Vec::new(),
            cameras: Vec::new(),
        };
        let full_id = "d".repeat(MAX_DEVICE_ID_BYTES);
        let full_name = "n".repeat(MAX_DEVICE_NAME_BYTES);
        let at_limit = list(
            (0..MAX_DEVICES)
                .map(|_| device(&full_id, &full_name))
                .collect(),
        );
        assert!(FromWorker::decode(&at_limit.encode()).is_ok());
        let cases = [
            (
                list((0..=MAX_DEVICES).map(|_| device("d", "n")).collect()),
                DecodeError::TooLarge,
            ),
            (list(vec![device(&long_id, "n")]), DecodeError::TooLarge),
            (list(vec![device("d", &long_name)]), DecodeError::TooLarge),
            (list(vec![device("", "n")]), DecodeError::TooShort),
            (
                list(vec![device("d\nx", "n")]),
                DecodeError::InvalidCharacter,
            ),
            (
                list(vec![device("d", "n\u{1b}[2J")]),
                DecodeError::InvalidCharacter,
            ),
        ];
        for (case, expected) in cases {
            assert_eq!(
                FromWorker::decode(&case.encode()),
                Err(expected),
                "{case:?}"
            );
        }
        assert_eq!(
            ToWorker::decode(&join_with_playout(&long_id).encode()),
            Err(DecodeError::TooLarge)
        );
        assert_eq!(
            ToWorker::decode(&join_with_playout("a\u{0}b").encode()),
            Err(DecodeError::InvalidCharacter)
        );
    }

    /// GIVEN a Join with no device chosen
    /// WHEN it is encoded and decoded
    /// THEN both devices stay the system default (`None`), not an empty ID.
    #[test]
    fn given_default_devices_when_round_tripped_then_they_stay_none() {
        let body = join_with("wss://h", "t", vec![], "id", key(0, 16));
        let Ok(ToWorker::Join {
            recording_device,
            playout_device,
            ..
        }) = ToWorker::decode(&body)
        else {
            panic!("join decodes");
        };
        assert_eq!((recording_device, playout_device), (None, None));
    }

    /// GIVEN OS device text with control characters, an over-long name, or
    /// a bad ID
    /// WHEN it is bounded for the wire
    /// THEN controls are removed, the name is cut on a character boundary,
    /// and a bad ID gives no device.
    #[test]
    fn given_os_device_text_when_bounded_then_it_fits_the_wire() {
        let named = MediaDevice::bounded("id", "Mic\u{1b}[31m (USB)\n").unwrap();
        assert_eq!(named.name, "Mic[31m (USB)");
        let wide = "é".repeat(MAX_DEVICE_NAME_BYTES);
        let cut = MediaDevice::bounded("id", &wide).unwrap();
        assert!(cut.name.len() <= MAX_DEVICE_NAME_BYTES);
        assert_eq!(cut.name.chars().count(), MAX_DEVICE_NAME_BYTES / 2);
        let long_id = "i".repeat(MAX_DEVICE_ID_BYTES + 1);
        for bad in ["", "a\tb", long_id.as_str()] {
            assert_eq!(MediaDevice::bounded(bad, "n"), None, "{bad:?}");
        }
        let fits = FromWorker::Devices {
            recording: vec![cut],
            playout: vec![named],
            cameras: Vec::new(),
        };
        assert!(FromWorker::decode(&fits.encode()).is_ok());
    }

    /// A video picture of `width` x `height` with a pattern in every byte.
    fn picture(width: u32, height: u32) -> Vec<u8> {
        (0..width as usize * height as usize * 4)
            .map(|i| (i % 251) as u8)
            .collect()
    }

    fn video_wire(tile: u8, rotation: u8, width: u32, height: u32) -> Vec<u8> {
        let mut wire = Vec::new();
        write_video_frame(
            &mut wire,
            tile,
            rotation,
            width,
            height,
            &picture(width, height),
        )
        .unwrap();
        wire
    }

    fn read_item(wire: Vec<u8>) -> (Result<WorkerItem, ReadError>, u64) {
        let budgets = ResourceBudgets::default();
        let mut reader = Cursor::new(wire);
        let result = read_worker_item(&mut reader, &budgets);
        // A returned picture stays charged while it lives; nothing else does.
        let held = match &result {
            Ok(WorkerItem::Video { rgba, .. }) => rgba.len() as u64,
            _ => 0,
        };
        assert_eq!(budgets.aggregate_used(), held);
        (result, reader.position())
    }

    fn pixel_header(width: u32, height: u32, length: u64) -> Vec<u8> {
        FrameHeader {
            kind: FrameKind::RgbaPixels,
            length,
            width,
            height,
        }
        .encode()
        .to_vec()
    }

    fn control_wire(message: &FromWorker) -> Vec<u8> {
        let mut wire = Vec::new();
        write_message(&mut wire, message).unwrap();
        wire
    }

    /// GIVEN a picture at the largest size and one at the smallest
    /// WHEN it is written and read back
    /// THEN the tile, rotation, size and every pixel byte are unchanged.
    #[test]
    fn given_video_frame_when_written_then_read_back_unchanged() {
        for (tile, rotation, width, height) in [
            (0, 0, 640, 360),
            (4, 3, 1, 1),
            (2, 1, 360, 640),
            (SCREEN_TILE, 0, 1280, 720),
            (SCREEN_TILE, 0, 720, 1280),
        ] {
            let (item, _) = read_item(video_wire(tile, rotation, width, height));
            let Ok(WorkerItem::Video {
                tile: got_tile,
                rotation: got_rotation,
                width: got_width,
                height: got_height,
                rgba,
            }) = item
            else {
                panic!("not a picture: {item:?}");
            };
            assert_eq!(
                (got_tile, got_rotation, got_width, got_height),
                (tile, rotation, width, height)
            );
            assert_eq!(rgba.as_slice(), picture(width, height).as_slice());
        }
        assert_eq!(MAX_VIDEO_BYTES, 921_600);
    }

    /// GIVEN a picture read from the worker
    /// WHEN it is kept, then dropped
    /// THEN its bytes stay charged to the budget while it is kept, and the
    /// charge ends with the picture.
    #[test]
    fn given_kept_picture_then_its_reservation_lives_as_long_as_the_bytes() {
        let budgets = ResourceBudgets::default();
        let mut reader = Cursor::new(video_wire(1, 0, 4, 2));
        let item = read_worker_item(&mut reader, &budgets).expect("a picture");
        let Some(bytes) = (match &item {
            WorkerItem::Video { rgba, .. } => Some(rgba.len() as u64),
            WorkerItem::Message(_) => None,
        }) else {
            panic!("not a picture");
        };
        assert_eq!(bytes, 4 * 2 * 4);
        assert_eq!(budgets.aggregate_used(), bytes);
        drop(item);
        assert_eq!(budgets.aggregate_used(), 0);
    }

    /// GIVEN a plain message
    /// WHEN it is read as a worker item
    /// THEN it comes back as a message and no pixel frame is expected.
    #[test]
    fn given_plain_message_when_read_as_item_then_it_is_a_message() {
        let message = FromWorker::Camera {
            state: CameraState::On,
        };
        let (item, _) = read_item(control_wire(&message));
        assert_eq!(item, Ok(WorkerItem::Message(message)));
    }

    /// GIVEN a video announcement followed by each wrong frame
    /// WHEN it is read
    /// THEN it fails, and no body past the checked header is read.
    #[test]
    fn given_wrong_frame_after_video_frame_then_read_fails_before_the_body() {
        let announce = FromWorker::VideoFrame {
            tile: 1,
            rotation: 0,
        };
        let mut control = control_wire(&announce);
        let end = control.len() as u64;
        // A control frame instead of pixels.
        let mut wire = control.clone();
        wire.extend(control_wire(&FromWorker::Left));
        let (result, position) = read_item(wire);
        assert_eq!(result, Err(ReadError::NotPixels));
        assert_eq!(position, end + FRAME_HEADER_LEN as u64);
        // Pixels over the side limit, the pixel limit, or with a bad length.
        for (width, height, length) in [
            (641, 1, 641 * 4),
            (1, 641, 641 * 4),
            (640, 361, 640 * 361 * 4),
            (8192, 1, 8192 * 4),
            (2, 2, 15),
        ] {
            let mut wire = control.clone();
            wire.extend(pixel_header(width, height, length));
            wire.extend(vec![0; 64]);
            let (result, position) = read_item(wire);
            assert!(
                matches!(
                    result,
                    Err(ReadError::VideoLimit)
                        | Err(ReadError::Frame(FrameError::PixelLengthMismatch))
                ),
                "{width}x{height}: {result:?}"
            );
            assert_eq!(position, end + FRAME_HEADER_LEN as u64);
        }
        // Zero dimensions are a frame error.
        control.extend(pixel_header(0, 4, 16));
        let (result, _) = read_item(control);
        assert_eq!(result, Err(ReadError::Frame(FrameError::ZeroDimension)));
    }

    /// GIVEN a video announcement with a missing or cut-off pixel frame
    /// WHEN it is read
    /// THEN the read fails with an I/O error.
    #[test]
    fn given_truncated_pixel_frame_when_read_then_it_fails() {
        let full = video_wire(0, 0, 4, 4);
        let control_len = control_wire(&FromWorker::VideoFrame {
            tile: 0,
            rotation: 0,
        })
        .len();
        for cut in [
            control_len,
            control_len + 5,
            control_len + 17,
            full.len() - 1,
        ] {
            let (result, _) = read_item(full[..cut].to_vec());
            assert!(
                matches!(result, Err(ReadError::Frame(FrameError::Io(_)))),
                "cut at {cut}: {result:?}"
            );
        }
    }

    /// GIVEN bad values in a video announcement
    /// WHEN it is decoded
    /// THEN it is rejected.
    #[test]
    fn given_bad_tile_or_rotation_when_decoded_then_it_is_rejected() {
        let invalid = DecodeError::InvalidValue;
        let cases: [(&[u8], DecodeError); 13] = [
            (&[VERSION, FROM_VIDEO_FRAME, 6, 0], invalid),
            (&[VERSION, FROM_VIDEO_FRAME, 0, 4], invalid),
            (&[VERSION, FROM_VIDEO_FRAME, 0], DecodeError::Truncated),
            (&[VERSION, FROM_VIDEO_STOPPED, 6], invalid),
            (&[VERSION, FROM_VIDEO_STARTED, 0, 0, 0, 1, 97], invalid),
            (&[VERSION, FROM_VIDEO_STARTED, 6, 0, 0, 1, 97], invalid),
            (&[VERSION, FROM_SCREEN, 6], invalid),
            (&[VERSION, FROM_SCREEN], DecodeError::Truncated),
            (&[VERSION, FROM_SCREEN_SHARER, 0, 0, 1, 97, 2], invalid),
            (
                &[VERSION, FROM_SCREEN_SHARER, 0, 0, 0, 1],
                DecodeError::TooShort,
            ),
            (
                &[VERSION, FROM_SCREEN_SHARER, 0, 0, 1, 97],
                DecodeError::Truncated,
            ),
            (
                &[VERSION, FROM_VIDEO_STARTED, 1, 0, 0, 0],
                DecodeError::TooShort,
            ),
            (&[VERSION, FROM_CAMERA, 4], invalid),
        ];
        for (body, expected) in cases {
            assert_eq!(FromWorker::decode(body), Err(expected), "{body:?}");
        }
        assert_eq!(
            ToWorker::decode(&[VERSION, TO_SET_CAMERA, 2, 0]),
            Err(invalid)
        );
        assert_eq!(
            ToWorker::decode(&[VERSION, TO_SET_CAMERA, 1, 2]),
            Err(invalid)
        );
        // A portal remote comes only with a camera-on.
        assert_eq!(
            ToWorker::decode(&[VERSION, TO_SET_CAMERA, 0, 1]),
            Err(invalid)
        );
        assert_eq!(
            ToWorker::decode(&[VERSION, TO_SET_CAMERA]),
            Err(DecodeError::Truncated)
        );
        assert_eq!(
            ToWorker::decode(&[VERSION, TO_SET_CAMERA, 1]),
            Err(DecodeError::Truncated)
        );
        let long = FromWorker::VideoStarted {
            tile: 1,
            connection: 0,
            identity: "i".repeat(MAX_IDENTITY_BYTES + 1),
        };
        assert_eq!(
            FromWorker::decode(&long.encode()),
            Err(DecodeError::TooLarge)
        );
        let control = FromWorker::VideoStarted {
            tile: 1,
            connection: 0,
            identity: "a\nb".to_owned(),
        };
        assert_eq!(
            FromWorker::decode(&control.encode()),
            Err(DecodeError::InvalidCharacter)
        );
    }

    /// GIVEN a picture outside the limits, or a bad tile or rotation
    /// WHEN the worker tries to write it
    /// THEN nothing is written.
    #[test]
    fn given_bad_video_frame_when_written_then_nothing_is_written() {
        let ok = picture(2, 2);
        let big = vec![0; 641 * 640 * 4];
        let cases: [(u8, u8, u32, u32, &[u8]); 8] = [
            (6, 0, 2, 2, &ok),
            (0, 4, 2, 2, &ok),
            (0, 0, 0, 2, &[]),
            (0, 0, 2, 2, &ok[..15]),
            (0, 0, 2, 2, &[0; 17]),
            (0, 0, 641, 1, &big[..641 * 4]),
            (0, 0, 1, 641, &big[..641 * 4]),
            (0, 0, 640, 361, &big[..640 * 361 * 4]),
        ];
        for (tile, rotation, width, height, body) in cases {
            let mut wire = Vec::new();
            assert!(write_video_frame(&mut wire, tile, rotation, width, height, body).is_err());
            assert!(wire.is_empty(), "{tile} {rotation} {width}x{height}");
        }
    }

    /// GIVEN the screen tile and a camera tile
    /// WHEN their limits are asked
    /// THEN the screen tile allows 1280x720 at 5 per second, and cameras keep
    /// 640x360 at 15 per second.
    #[test]
    fn given_tiles_when_limits_asked_then_screen_differs_from_camera() {
        assert_eq!((VIDEO_TILES, SCREEN_TILE, VERSION), (6, 5, 12));
        assert_eq!(MAX_SCREEN_BYTES, 3_686_400);
        assert_eq!(MAX_SCREEN_PIXELS, 921_600);
        for tile in 0..SCREEN_TILE {
            assert_eq!(
                (max_side(tile), max_pixels(tile), max_fps(tile)),
                (640, 640 * 360, 15)
            );
        }
        assert_eq!(
            (
                max_side(SCREEN_TILE),
                max_pixels(SCREEN_TILE),
                max_fps(SCREEN_TILE)
            ),
            (1280, 1280 * 720, 5)
        );
        assert!(picture_fits(SCREEN_TILE, 1280, 720));
        assert!(!picture_fits(SCREEN_TILE, 1281, 720));
        assert!(!picture_fits(SCREEN_TILE, 1280, 721));
        assert!(!picture_fits(0, 1280, 720));
        assert!(!picture_fits(VIDEO_TILES, 1, 1));
        assert!(!picture_fits(SCREEN_TILE, 0, 1));
    }

    /// GIVEN a screen-size picture announced for a camera tile, and a
    /// picture over the screen limit announced for the screen tile
    /// WHEN each is read from the worker
    /// THEN both stop at the header with `VideoLimit` and no body is read.
    #[test]
    fn given_picture_over_the_limit_of_its_tile_then_it_is_refused_before_the_body() {
        for (tile, width, height) in [
            (1, 1280, 720),
            (0, 641, 1),
            (SCREEN_TILE, 1281, 1),
            (SCREEN_TILE, 1280, 721),
            (SCREEN_TILE, 1, 1281),
        ] {
            let mut wire = control_wire(&FromWorker::VideoFrame { tile, rotation: 0 });
            let end = wire.len() as u64;
            wire.extend(pixel_header(
                width,
                height,
                u64::from(width) * u64::from(height) * 4,
            ));
            wire.extend(vec![0; 64]);
            let (result, position) = read_item(wire);
            assert_eq!(
                result,
                Err(ReadError::VideoLimit),
                "{tile} {width}x{height}"
            );
            assert_eq!(position, end + FRAME_HEADER_LEN as u64);
        }
    }

    /// GIVEN a screen-size picture for a camera tile or one over the screen
    /// limit for the screen tile
    /// WHEN the worker tries to write it
    /// THEN nothing is written; a 1280x720 screen picture is written.
    #[test]
    fn given_screen_size_picture_when_written_then_the_tile_limit_decides() {
        for (tile, width, height) in [(0, 1280, 720), (4, 641, 360), (SCREEN_TILE, 1281, 720)] {
            let body = vec![0; width as usize * height as usize * 4];
            let mut wire = Vec::new();
            assert!(write_video_frame(&mut wire, tile, 0, width, height, &body).is_err());
            assert!(wire.is_empty(), "{tile} {width}x{height}");
        }
        let mut wire = Vec::new();
        let body = vec![7; MAX_SCREEN_BYTES];
        write_video_frame(&mut wire, SCREEN_TILE, 0, 1280, 720, &body).unwrap();
        assert!(!wire.is_empty());
    }

    /// GIVEN bad values in a SetScreen
    /// WHEN it is decoded
    /// THEN a bad kind, monitor index or monitor size is rejected, and a cut
    /// message is truncated.
    #[test]
    fn given_bad_set_screen_when_decoded_then_it_is_rejected() {
        let invalid = DecodeError::InvalidValue;
        let monitor = |index: u8, width: u32, height: u32| {
            let mut body = vec![VERSION, TO_SET_SCREEN, 0, 0, 0, 9, 2, index];
            body.extend(width.to_be_bytes());
            body.extend(height.to_be_bytes());
            body
        };
        assert_eq!(
            ToWorker::decode(&[VERSION, TO_SET_SCREEN, 0, 0, 0, 1, 3]),
            Err(invalid)
        );
        assert_eq!(
            ToWorker::decode(&[VERSION, TO_SET_SCREEN]),
            Err(DecodeError::Truncated)
        );
        assert_eq!(
            ToWorker::decode(&[VERSION, TO_SET_SCREEN, 0, 0, 1]),
            Err(DecodeError::Truncated)
        );
        assert_eq!(
            ToWorker::decode(&[VERSION, TO_SET_SCREEN, 0, 0, 0, 1, 1, 0, 0]),
            Err(DecodeError::Truncated)
        );
        assert_eq!(
            ToWorker::decode(&[VERSION, TO_SET_SCREEN, 0, 0, 0, 1, 0, 0]),
            Err(DecodeError::TrailingBytes)
        );
        assert_eq!(
            FromWorker::decode(&[VERSION, FROM_SCREEN, 0, 0, 0]),
            Err(DecodeError::Truncated)
        );
        assert_eq!(ToWorker::decode(&monitor(16, 1920, 1080)), Err(invalid));
        assert_eq!(ToWorker::decode(&monitor(0, 0, 1080)), Err(invalid));
        assert_eq!(ToWorker::decode(&monitor(0, 1920, 0)), Err(invalid));
        assert_eq!(ToWorker::decode(&monitor(0, 16_385, 1)), Err(invalid));
        assert_eq!(ToWorker::decode(&monitor(0, 1, 16_385)), Err(invalid));
        assert_eq!(
            ToWorker::decode(&monitor(0, 1920, 1080)),
            Ok(ToWorker::SetScreen {
                target: Some(ScreenTarget::Monitor {
                    index: 0,
                    width: 1920,
                    height: 1080
                }),
                share: 9,
            })
        );
    }

    /// GIVEN a message made for wire version 4
    /// WHEN it is decoded
    /// THEN the version is refused.
    #[test]
    fn given_old_wire_version_when_decoded_then_it_is_refused() {
        assert_eq!(
            ToWorker::decode(&[4, TO_LEAVE]),
            Err(DecodeError::UnknownVersion)
        );
        assert_eq!(
            FromWorker::decode(&[4, FROM_LEFT]),
            Err(DecodeError::UnknownVersion)
        );
    }

    /// GIVEN a Join with a camera chosen, or none
    /// WHEN it is encoded and decoded
    /// THEN the camera ID and flag are kept, and no camera stays `None`.
    #[test]
    fn given_join_camera_fields_when_round_tripped_then_they_are_kept() {
        let Ok(ToWorker::Join {
            camera_device,
            camera_on,
            ..
        }) = ToWorker::decode(&join().encode())
        else {
            panic!("join decodes");
        };
        assert_eq!(camera_device.as_deref(), Some("usb#vid_046d&pid_0825#cam"));
        assert!(camera_on);
        let none = join_with("wss://h", "t", vec![], "id", key(0, 16));
        let Ok(ToWorker::Join {
            camera_device,
            camera_on,
            ..
        }) = ToWorker::decode(&none)
        else {
            panic!("join decodes");
        };
        assert_eq!((camera_device, camera_on), (None, false));
        // A version 2 or 3 Join is refused.
        for version in [2, 3] {
            let mut old = join().encode().to_vec();
            old[0] = version;
            assert_eq!(ToWorker::decode(&old), Err(DecodeError::UnknownVersion));
        }
    }

    /// GIVEN a camera ID or camera list over its limits
    /// WHEN the messages are decoded
    /// THEN each is rejected.
    #[test]
    fn given_camera_fields_over_limit_when_decoded_then_they_are_rejected() {
        let long = "c".repeat(MAX_DEVICE_ID_BYTES + 1);
        let with_camera = |camera: &str| {
            let ToWorker::Join {
                sfu_url,
                jwt,
                turn,
                relay_only,
                own_identity,
                own_key,
                ..
            } = join()
            else {
                unreachable!()
            };
            ToWorker::Join {
                sfu_url,
                jwt,
                turn,
                relay_only,
                own_identity,
                own_key,
                recording_device: None,
                playout_device: None,
                camera_device: Some(camera.to_owned()),
                camera_on: true,
                volume: 0,
                auto_level: true,
                mic_gain: DEFAULT_MIC_GAIN_PERCENT,
            }
            .encode()
        };
        assert!(ToWorker::decode(&with_camera(&"c".repeat(MAX_DEVICE_ID_BYTES))).is_ok());
        assert_eq!(
            ToWorker::decode(&with_camera(&long)),
            Err(DecodeError::TooLarge)
        );
        assert_eq!(
            ToWorker::decode(&with_camera("a\nb")),
            Err(DecodeError::InvalidCharacter)
        );
        let cameras = |cameras: Vec<MediaDevice>| FromWorker::Devices {
            recording: Vec::new(),
            playout: Vec::new(),
            cameras,
        };
        let full: Vec<MediaDevice> = (0..MAX_DEVICES).map(|_| device("c", "n")).collect();
        assert!(FromWorker::decode(&cameras(full).encode()).is_ok());
        for (list, expected) in [
            (
                (0..=MAX_DEVICES).map(|_| device("c", "n")).collect(),
                DecodeError::TooLarge,
            ),
            (vec![device(&long, "n")], DecodeError::TooLarge),
            (vec![device("", "n")], DecodeError::TooShort),
            (vec![device("c\u{0}", "n")], DecodeError::InvalidCharacter),
        ] {
            assert_eq!(FromWorker::decode(&cameras(list).encode()), Err(expected));
        }
    }

    /// GIVEN a Subscribe with each connection index and a `wss://` URL
    /// WHEN it is encoded and decoded
    /// THEN it is the same message.
    #[test]
    fn given_subscribe_when_round_tripped_then_it_is_kept() {
        for index in 1..=MAX_OTHER_CONNECTIONS {
            let message = subscribe(index, "wss://sfu.other.example");
            assert_eq!(ToWorker::decode(&message.encode()), Ok(message));
        }
    }

    /// GIVEN a Subscribe with index 0 or above 3, or a URL that is not
    /// `wss://`, or a JWT with a space, or no JWT
    /// WHEN it is decoded
    /// THEN it is rejected.
    #[test]
    fn given_bad_subscribe_when_decoded_then_it_is_rejected() {
        for index in [0, 4, 255] {
            let body = subscribe(index, "wss://sfu.other.example").encode();
            assert_eq!(
                ToWorker::decode(&body),
                Err(DecodeError::InvalidValue),
                "{index}"
            );
        }
        for url in ["https://x", "ws://x.example", "wss://", "WSS://x.example"] {
            let body = subscribe(1, url).encode();
            assert_eq!(
                ToWorker::decode(&body),
                Err(DecodeError::InvalidValue),
                "{url}"
            );
        }
        let spaced = ToWorker::Subscribe {
            index: 1,
            url: "wss://x.example".to_owned(),
            jwt: Zeroizing::new("a b".to_owned()),
        };
        assert_eq!(
            ToWorker::decode(&spaced.encode()),
            Err(DecodeError::InvalidCharacter)
        );
        let empty = ToWorker::Subscribe {
            index: 1,
            url: "wss://x.example".to_owned(),
            jwt: Zeroizing::new(String::new()),
        };
        assert_eq!(
            ToWorker::decode(&empty.encode()),
            Err(DecodeError::TooShort)
        );
    }

    /// GIVEN a Subscribe with a URL or JWT over its limit, a cut body, or
    /// trailing bytes
    /// WHEN it is decoded
    /// THEN it is rejected before anything large is kept.
    #[test]
    fn given_oversized_cut_or_padded_subscribe_when_decoded_then_it_is_rejected() {
        let long_url = format!("wss://{}", "a".repeat(MAX_URL_BYTES));
        assert_eq!(
            ToWorker::decode(&subscribe(1, &long_url).encode()),
            Err(DecodeError::TooLarge)
        );
        let long_jwt = ToWorker::Subscribe {
            index: 1,
            url: "wss://x.example".to_owned(),
            jwt: Zeroizing::new("a".repeat(MAX_JWT_BYTES + 1)),
        };
        assert_eq!(
            ToWorker::decode(&long_jwt.encode()),
            Err(DecodeError::TooLarge)
        );
        let full = subscribe(2, "wss://x.example").encode();
        for cut in 2..full.len() {
            assert!(ToWorker::decode(&full[..cut]).is_err(), "cut at {cut}");
        }
        let mut padded = full.to_vec();
        padded.push(0);
        assert_eq!(ToWorker::decode(&padded), Err(DecodeError::TrailingBytes));
    }

    /// GIVEN a Subscribe
    /// WHEN it is formatted with Debug
    /// THEN the JWT is not shown.
    #[test]
    fn given_subscribe_when_debug_formatted_then_the_jwt_is_hidden() {
        let message = ToWorker::Subscribe {
            index: 1,
            url: "wss://x.example".to_owned(),
            jwt: Zeroizing::new("secret.jwt.text".to_owned()),
        };
        let text = format!("{message:?}");
        assert!(!text.contains("secret"), "{text}");
        assert!(text.contains("jwt_len"));
    }

    /// GIVEN worker messages with a connection above 3, or a Connection
    /// message for connection 0
    /// WHEN they are decoded
    /// THEN they are rejected.
    #[test]
    fn given_bad_connection_numbers_when_decoded_then_they_are_rejected() {
        let invalid = DecodeError::InvalidValue;
        for connection in [4, 255] {
            for body in [
                vec![VERSION, FROM_PARTICIPANT_JOINED, connection, 0, 1, 97],
                vec![VERSION, FROM_PARTICIPANT_LEFT, connection, 0, 1, 97],
                vec![VERSION, FROM_SPEAKING, connection, 0, 1, 97, 1],
                vec![VERSION, FROM_SCREEN_SHARER, connection, 0, 1, 97, 1],
                vec![VERSION, FROM_VIDEO_STARTED, 1, connection, 0, 1, 97],
                vec![VERSION, FROM_CONNECTION, connection, 1],
            ] {
                assert_eq!(FromWorker::decode(&body), Err(invalid), "{body:?}");
            }
        }
        assert_eq!(
            FromWorker::decode(&[VERSION, FROM_CONNECTION, 0, 1]),
            Err(invalid)
        );
        assert_eq!(
            FromWorker::decode(&[VERSION, FROM_CONNECTION, 1, 2]),
            Err(invalid)
        );
        assert_eq!(
            FromWorker::decode(&[VERSION, FROM_CONNECTION, 1]),
            Err(DecodeError::Truncated)
        );
    }

    /// GIVEN a message of the previous wire versions (6 to 11)
    /// WHEN it is decoded in either direction
    /// THEN the version is refused.
    #[test]
    fn given_older_wire_versions_when_decoded_then_they_are_refused() {
        assert_eq!(VERSION, 12);
        for old in [6, 7, 8, 9, 10, 11] {
            assert_eq!(
                ToWorker::decode(&[old, TO_LEAVE]),
                Err(DecodeError::UnknownVersion)
            );
            assert_eq!(
                FromWorker::decode(&[old, FROM_CONNECTED]),
                Err(DecodeError::UnknownVersion)
            );
        }
    }

    /// GIVEN a volume over 100 percent, a bad deafen flag, or a truncated
    /// volume or deafen message
    /// WHEN each is decoded
    /// THEN it is rejected; 0 and 100 percent are accepted.
    #[test]
    fn given_bad_volume_or_deafen_fields_when_decoded_then_they_are_rejected() {
        let invalid = Err(DecodeError::InvalidValue);
        assert_eq!(ToWorker::decode(&[VERSION, TO_SET_VOLUME, 101]), invalid);
        assert_eq!(ToWorker::decode(&[VERSION, TO_SET_VOLUME, 255]), invalid);
        assert_eq!(
            ToWorker::decode(&[VERSION, TO_SET_VOLUME]),
            Err(DecodeError::Truncated)
        );
        assert_eq!(
            ToWorker::decode(&[VERSION, TO_SET_VOLUME, 0]),
            Ok(ToWorker::SetVolume { percent: 0 })
        );
        assert_eq!(
            ToWorker::decode(&[VERSION, TO_SET_VOLUME, 100]),
            Ok(ToWorker::SetVolume { percent: 100 })
        );
        assert_eq!(ToWorker::decode(&[VERSION, TO_SET_DEAFENED, 2]), invalid);
        assert_eq!(
            FromWorker::decode(&[VERSION, FROM_DEAFENED, 2]),
            Err(DecodeError::InvalidValue)
        );
        assert_eq!(
            FromWorker::decode(&[VERSION, FROM_DEAFENED]),
            Err(DecodeError::Truncated)
        );
        assert_eq!(
            ToWorker::decode(&[VERSION, TO_SET_DEAFENED, 1, 0]),
            Err(DecodeError::TrailingBytes)
        );
    }

    /// GIVEN a Join whose volume byte is over 100 percent
    /// WHEN it is decoded
    /// THEN it is rejected; the encoded volume round-trips.
    #[test]
    fn given_join_volume_when_decoded_then_it_is_bounded() {
        let mut body = join().encode().to_vec();
        let Ok(ToWorker::Join { volume, .. }) = ToWorker::decode(&body) else {
            panic!("join decodes");
        };
        assert_eq!(volume, 65);
        // The volume, the automatic level flag and the two gain bytes end it.
        let at = body.len() - 4;
        body[at] = MAX_VOLUME_PERCENT + 1;
        assert_eq!(ToWorker::decode(&body), Err(DecodeError::InvalidValue));
    }

    /// GIVEN a Join and a SetMicrophone with each gain and flag value
    /// WHEN each is encoded and decoded
    /// THEN the values round-trip; a gain over 400 percent, off the 5 percent
    /// step, a bad flag or a short or long message is refused.
    #[test]
    fn given_microphone_settings_when_decoded_then_they_are_bounded() {
        let invalid = Err(DecodeError::InvalidValue);
        for gain in [0, 5, 100, 395, MAX_MIC_GAIN_PERCENT] {
            let message = ToWorker::SetMicrophone {
                auto_level: gain % 2 == 0,
                mic_gain: gain,
            };
            assert_eq!(ToWorker::decode(&message.encode()), Ok(message));
        }
        let [high, low] = 250_u16.to_be_bytes();
        assert_eq!(
            ToWorker::decode(&[VERSION, TO_SET_MICROPHONE, 1, high, low]),
            Ok(ToWorker::SetMicrophone {
                auto_level: true,
                mic_gain: 250
            })
        );
        for gain in [401_u16, 405, 7, 101, 1000, u16::MAX] {
            let [high, low] = gain.to_be_bytes();
            assert_eq!(
                ToWorker::decode(&[VERSION, TO_SET_MICROPHONE, 1, high, low]),
                invalid,
                "{gain}"
            );
        }
        assert_eq!(
            ToWorker::decode(&[VERSION, TO_SET_MICROPHONE, 2, 0, 100]),
            invalid
        );
        assert_eq!(
            ToWorker::decode(&[VERSION, TO_SET_MICROPHONE, 1, 0]),
            Err(DecodeError::Truncated)
        );
        assert_eq!(
            ToWorker::decode(&[VERSION, TO_SET_MICROPHONE, 1, 0, 100, 0]),
            Err(DecodeError::TrailingBytes)
        );
        // A Join with a gain over the limit is refused as a whole.
        let mut body = join().encode().to_vec();
        let end = body.len();
        body[end - 2..].copy_from_slice(&405_u16.to_be_bytes());
        assert_eq!(ToWorker::decode(&body), invalid);
        body[end - 2..].copy_from_slice(&MAX_MIC_GAIN_PERCENT.to_be_bytes());
        assert!(matches!(
            ToWorker::decode(&body),
            Ok(ToWorker::Join {
                mic_gain: MAX_MIC_GAIN_PERCENT,
                ..
            })
        ));
        assert!(valid_mic_gain(0) && valid_mic_gain(400));
        assert!(!valid_mic_gain(2) && !valid_mic_gain(405));
    }

    /// GIVEN a sound level over the top, or without its byte
    /// WHEN it is decoded
    /// THEN it is rejected; the top level itself decodes.
    #[test]
    fn given_sound_level_when_decoded_then_it_is_bounded() {
        assert_eq!(
            FromWorker::decode(&[VERSION, FROM_SOUND_LEVEL, MAX_SOUND_LEVEL]),
            Ok(FromWorker::SoundLevel {
                level: MAX_SOUND_LEVEL
            })
        );
        assert_eq!(
            FromWorker::decode(&[VERSION, FROM_SOUND_LEVEL, MAX_SOUND_LEVEL + 1]),
            Err(DecodeError::InvalidValue)
        );
        assert!(FromWorker::decode(&[VERSION, FROM_SOUND_LEVEL]).is_err());
    }

    /// GIVEN version 10 messages with a value outside their closed sets
    /// WHEN they are decoded
    /// THEN each is rejected: an unknown device test, a participant level
    /// over the top or on a connection past the last, and an unknown
    /// failure code in `DevicesSet`.
    #[test]
    fn given_device_and_level_messages_out_of_range_then_they_are_rejected() {
        for test in [0, 4, u8::MAX] {
            assert_eq!(
                ToWorker::decode(&[VERSION, TO_TEST_DEVICE, test, 0, 0]),
                Err(DecodeError::InvalidValue),
                "test {test}"
            );
        }
        let level = |connection: u8, level: u8| {
            let mut body = vec![VERSION, FROM_PARTICIPANT_LEVEL, connection];
            put_str(&mut body, "@bob:example.org:D");
            body.push(level);
            FromWorker::decode(&body)
        };
        assert!(level(0, MAX_SOUND_LEVEL).is_ok());
        assert_eq!(
            level(0, MAX_SOUND_LEVEL + 1),
            Err(DecodeError::InvalidValue)
        );
        assert_eq!(
            level(MAX_OTHER_CONNECTIONS + 1, 0),
            Err(DecodeError::InvalidValue)
        );
        assert_eq!(
            FromWorker::decode(&[VERSION, FROM_DEVICES_SET, 200]),
            Err(DecodeError::InvalidValue)
        );
        assert!(FromWorker::decode(&[VERSION, FROM_DEVICES_SET]).is_err());
    }

    /// GIVEN diagnostic records with a bad level, an empty or long target, a
    /// long message or a control character WHEN decoded THEN each is
    /// rejected.
    #[test]
    fn given_bad_diagnostic_records_when_decoded_then_rejected() {
        let record = |level: u8, target: &str, message: &str| {
            FromWorker::Diagnostic {
                level,
                target: target.to_owned(),
                message: message.to_owned(),
            }
            .encode()
        };
        let long_target = "t".repeat(MAX_DIAGNOSTIC_TARGET_BYTES + 1);
        let long_message = "m".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES + 1);
        for (body, error) in [
            (record(0, "t", "m"), DecodeError::InvalidValue),
            (
                record(MAX_DIAGNOSTIC_LEVEL + 1, "t", "m"),
                DecodeError::InvalidValue,
            ),
            (record(1, "", "m"), DecodeError::TooShort),
            (record(1, &long_target, "m"), DecodeError::TooLarge),
            (record(1, "t", &long_message), DecodeError::TooLarge),
            (record(1, "t", "line\nbreak"), DecodeError::InvalidCharacter),
        ] {
            assert_eq!(FromWorker::decode(&body), Err(error));
        }
    }
}
