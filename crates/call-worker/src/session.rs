//! One call: joins the LiveKit room and runs until the core ends it or the
//! room fails. All LiveKit types stay in this module.

use std::collections::HashMap;
use std::sync::{Arc, RwLock, RwLockReadGuard};
use std::task::Poll;
use std::time::Duration;

use livekit::{
    Room, RoomError, RoomEvent, RoomOptions,
    e2ee::{
        E2eeOptions,
        key_provider::{
            DEFAULT_RATCHET_SALT, KeyDerivationAlgorithm, KeyProvider, KeyProviderOptions,
        },
        manager::E2eeManager,
    },
    id::ParticipantIdentity,
    options::{TrackPublishOptions, VideoCodec, VideoEncoding},
    prelude::{
        AudioProcessingOptions, LocalAudioTrack, LocalTrack, LocalTrackPublication,
        LocalVideoTrack, Participant, PlatformAudio, PlayoutDeviceId, RecordingDeviceId,
        RemoteAudioTrack, RemoteParticipant, RemoteTrack, RemoteTrackPublication, TrackKind,
        TrackSid, TrackSource,
    },
    rtc_engine::EngineError,
    webrtc::{
        native::yuv_helper,
        peer_connection_factory::{IceServer, IceTransportsType, RtcConfiguration},
        stats::RtcStats,
        video_frame::{
            I420Buffer, NV12Buffer, VideoBuffer, VideoFormatType, VideoFrame, VideoRotation,
            native::VideoFrameBufferExt,
        },
        video_source::{RtcVideoSource, VideoResolution, native::NativeVideoSource},
        video_stream::native::NativeVideoStream,
        video_track::RtcVideoTrack,
    },
};
use livekit_signaling::SignalError;
use persephone_call_protocol::{
    CameraState, DeviceTest, FailureCode, FrameKeyMsg, FromWorker, MAX_OTHER_CONNECTIONS,
    MAX_PARTICIPANTS, MAX_SCREEN_FPS, MAX_VIDEO_FPS, MAX_VIDEO_PIXELS, MAX_VIDEO_SIDE, SCREEN_TILE,
    ScreenState, ScreenTarget, ToWorker, VIDEO_TILES,
};
use tokio::sync::mpsc::{self, Receiver, UnboundedReceiver};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval};
use tokio_stream::StreamExt;
use zeroize::Zeroizing;

use crate::camera::{self, CapturedFrame, Format};
use crate::containment;
use crate::mapping::{
    ConnectFault, LevelMeters, Levels, Playout, RoomChange, Slot, Slots, SoundPath,
    SpeakingTracker, apply, bounded_device_list, choose_device, failure_for_connect,
    system_default, valid_identity,
};
#[cfg(target_os = "linux")]
use crate::mapping::{pulse_device_list, sound_path};
use crate::output::{Closed, Output, PictureSink};
use crate::screen;
use crate::sound_defaults;
use crate::sound_test;
use crate::video::{
    Added, OWN_TILE, RateLimiter, ScreenQueue, TileAssigner, fit_for_tile, fit_publish_box,
    fit_within_box, quarter_turns, rgba_len,
};
use crate::{EXIT_DONE, EXIT_FAILED, Input};

/// Element Call's key ring size (livekit-client keyringSize).
const KEY_RING_SIZE: i32 = 256;
/// Element Call's ratchet window size (livekit-client ratchetWindowSize).
const RATCHET_WINDOW_SIZE: i32 = 10;
/// No tolerance for decryption failures before a key is marked bad (-1).
const FAILURE_TOLERANCE: i32 = -1;
/// The most a published screen share encodes, in bits per second.
const SCREEN_MAX_BITRATE: u64 = 1_500_000;
/// How often deferred Speaking changes are looked at.
const SPEAKING_TICK: Duration = Duration::from_millis(100);
/// The longest one sound level measurement may take (ADR-0055).
const SOUND_MEASURE_TIMEOUT: Duration = Duration::from_millis(500);
/// The time one measurement has to read statistics: the own microphone
/// first, then the remote tracks. It is shorter than
/// [`SOUND_MEASURE_TIMEOUT`], so a slow remote read never drops the own
/// level (ADR-0059).
const SOUND_READ_BUDGET: Duration = Duration::from_millis(400);
const _: () = assert!(SOUND_READ_BUDGET.as_millis() < SOUND_MEASURE_TIMEOUT.as_millis());
/// The most remote audio tracks measured at once: two for each participant
/// (a microphone and a screen share's sound).
const MAX_MEASURED_TRACKS: usize = 2 * MAX_PARTICIPANTS;
/// How long a camera test shows the camera when nothing stops it.
const CAMERA_TEST_TIME: Duration = Duration::from_secs(30);

/// Why the call ended.
enum End {
    Left,
    Failed(FailureCode),
}

/// Runs the call that `first` (a `Join`) asks for. Returns the exit code.
pub async fn run(first: ToWorker, input: &mut Receiver<Input>, output: &Output) -> u8 {
    let ToWorker::Join {
        sfu_url,
        jwt,
        turn,
        relay_only,
        own_identity,
        mut own_key,
        recording_device,
        playout_device,
        camera_device,
        camera_on,
        volume,
    } = first
    else {
        return fail(output, FailureCode::Protocol).await;
    };

    let provider = KeyProvider::new(KeyProviderOptions {
        ratchet_window_size: RATCHET_WINDOW_SIZE,
        ratchet_salt: DEFAULT_RATCHET_SALT.as_bytes().to_vec(),
        failure_tolerance: FAILURE_TOLERANCE,
        key_ring_size: KEY_RING_SIZE,
        key_derivation_algorithm: KeyDerivationAlgorithm::HKDF,
    });
    let own = ParticipantIdentity::from(own_identity);
    let own_index = i32::from(own_key.index);
    if !set_key(&provider, &own, &mut own_key) {
        return fail(output, FailureCode::Internal).await;
    }

    let mut rtc_config = RtcConfiguration::default();
    rtc_config.ice_servers = turn
        .into_iter()
        .map(|mut server| IceServer {
            urls: vec![server.uri],
            // Moved out, not copied: the Zeroizing wrapper then holds an empty string.
            username: std::mem::take(&mut *server.username),
            password: std::mem::take(&mut *server.credential),
        })
        .collect();
    rtc_config.ice_transport_type = if relay_only {
        IceTransportsType::Relay
    } else {
        IceTransportsType::All
    };
    let mut options = RoomOptions::default();
    options.auto_subscribe = true;
    options.adaptive_stream = false;
    options.dynacast = false;
    options.encryption = Some(E2eeOptions {
        encryption_type: livekit::e2ee::EncryptionType::Gcm,
        key_provider: provider.clone(),
    });
    options.rtc_config = rtc_config;

    // A microphone or speaker chosen by sound server name is checked before
    // audio starts; libwebrtc then opens its default stream (ADR-0049).
    let joined_sound = (recording_device.clone(), playout_device.clone());
    let paths = match sound_paths(recording_device.as_deref(), playout_device.as_deref()).await {
        Ok(paths) => paths,
        Err(code) => return fail(output, code).await,
    };
    let audio = match start_audio() {
        Ok(audio) => audio,
        Err(code) => return fail(output, code).await,
    };
    fn listed(wanted: Option<&str>, path: SoundPath) -> Option<&str> {
        wanted.filter(|_| path == SoundPath::Listed)
    }
    // Read once, before the event loop: a change during the call reuses it.
    let media_defaults = MediaDefaults::read();
    let sound_devices = match select_devices(
        &audio,
        &media_defaults,
        listed(recording_device.as_deref(), paths[0]),
        listed(playout_device.as_deref(), paths[1]),
    ) {
        Ok(devices) => devices,
        Err(code) => return fail(output, code).await,
    };
    // Linux stage 2 (ADR-0049): after the sound server connection and before
    // anything goes to the SFU. A failure ends the call.
    if !containment::apply_stage2() && !cfg!(feature = "dev-uncontained") {
        return fail(output, FailureCode::Containment).await;
    }
    // The same options (key provider, TURN servers) serve every connection to
    // another SFU (ADR-0054).
    let other_options = options.clone();
    let connected = Room::connect(&sfu_url, &jwt, options).await;
    // The token is not needed after the connection attempt.
    drop(jwt);
    let (room, mut events) = match connected {
        Ok(connected) => connected,
        Err(error) => return fail(output, classify(&error)).await,
    };
    let publication = match publish_microphone(&room, &audio).await {
        Ok(publication) => publication,
        Err(code) => {
            let _ = room.close().await;
            return fail(output, code).await;
        }
    };
    // The sender's frame cryptor exists once `publish_track` returns.
    let own_sids = vec![publication.sid()];
    use_own_key_index(room.e2ee_manager(), &own_sids, own_index);
    if output.send(FromWorker::Connected).await.is_err() {
        let _ = room.close().await;
        return EXIT_FAILED;
    }
    let mut tracker = SpeakingTracker::default();
    for (identity, participant) in room.remote_participants() {
        let change = RoomChange::Joined(0, identity.to_string(), microphone_on(&participant));
        for message in apply(change, &mut tracker) {
            if output.send(message).await.is_err() {
                let _ = room.close().await;
                return EXIT_FAILED;
            }
        }
    }

    let (camera_failed_tx, mut camera_failed) = mpsc::channel::<u64>(1);
    // At most one camera open is in flight, so one slot is enough.
    let (open_tx, mut open_done) = mpsc::channel::<Opened<OpenedCamera, CameraState>>(1);
    // The same for the screen share (ADR-0053).
    let screen_ends = Arc::new(ScreenEnd::default());
    let (screen_open_tx, screen_open_done) = mpsc::channel::<Opened<OpenedScreen, ScreenState>>(1);
    let mut screens = ScreenInbox {
        ended: screen_ends.clone(),
        opened: screen_open_done,
    };
    // At most one connect is in flight for each other SFU, so this many
    // results can wait.
    let (connect_tx, connect_done) = mpsc::channel::<Connected>(usize::from(MAX_OTHER_CONNECTIONS));
    // At most one sound level measurement runs, so one result can wait.
    let (sound_tx, mut sound_done) = mpsc::channel::<Option<Levels>>(1);
    let mut others_inbox = OtherInbox {
        events: OtherEvents::default(),
        connected: connect_done,
    };
    let mut call = Call {
        room: &room,
        provider: &provider,
        identity: &own,
        audio: &audio,
        sound_devices,
        media_defaults,
        output,
        own_sids,
        publication: &publication,
        camera_device,
        joined_sound,
        camera: None,
        stalled: None,
        gate: CameraGate::new(own_index),
        camera_failed: camera_failed_tx,
        open_tx,
        screen: None,
        screen_stalled: None,
        screen_gate: CameraGate::new(own_index),
        screen_ends,
        screen_end_reason: None,
        screen_next: NextStart::default(),
        screen_share: 0,
        screen_open_tx,
        remote: RemoteVideo::default(),
        other_options,
        connect_tx,
        others: Others::default(),
        playout: Playout::new(volume),
        sound: SoundProbe::new(sound_tx),
    };
    // Tracks subscribed during the connect get the volume now; later ones
    // when their subscription is handled.
    call.apply_playout();
    let mut end = None;
    if camera_on
        && let Some(state) = call.begin_camera(camera::Grant::default())
        && output.send(FromWorker::Camera { state }).await.is_err()
    {
        end = Some(End::Failed(FailureCode::Internal));
    }
    let end = match end {
        Some(end) => end,
        None => {
            event_loop(
                &mut events,
                input,
                output,
                &mut call,
                &mut camera_failed,
                &mut open_done,
                &mut screens,
                &mut others_inbox,
                &mut tracker,
                &mut sound_done,
            )
            .await
        }
    };
    call.shutdown(&mut open_done, &mut screens).await;
    // Every connection to another SFU ends with the call (ADR-0054).
    call.close_others(&mut others_inbox).await;
    let _ = room.close().await;
    drop(audio);
    match end {
        End::Left => {
            // The core may already be gone; nothing more to do then.
            let _ = output.send(FromWorker::Left).await;
            EXIT_DONE
        }
        End::Failed(code) => fail(output, code).await,
    }
}

/// Lists the microphones and speakers, answers once and ends. It starts no
/// recording or playout and connects to nothing.
pub async fn list_devices(output: &Output) -> u8 {
    let audio = match PlatformAudio::new() {
        Ok(audio) => audio,
        Err(_) => return fail(output, FailureCode::AudioDevice).await,
    };
    let recording_devices: Vec<_> = audio.recording_devices().collect();
    let playout_devices: Vec<_> = audio.playout_devices().collect();
    let recording = bounded_device_list(
        recording_devices
            .iter()
            .map(|device| (device.id.as_str(), device.name.as_str())),
    );
    let playout = bounded_device_list(
        playout_devices
            .iter()
            .map(|device| (device.id.as_str(), device.name.as_str())),
    );
    drop(audio);
    // Linux: the sound server's own names replace libwebrtc's positions,
    // which are never listed (ADR-0049). Without an answer the lists stay
    // as they are: system default only.
    #[cfg(target_os = "linux")]
    let (recording, playout) = match tokio::task::spawn_blocking(crate::pulse::query).await {
        Ok(Some(lists)) => (
            pulse_device_list(lists.sources.iter().map(|(n, d)| (n.as_str(), d.as_str()))),
            pulse_device_list(lists.sinks.iter().map(|(n, d)| (n.as_str(), d.as_str()))),
        ),
        _ => (recording, playout),
    };
    // Listing cameras opens none. It runs off the runtime threads: the
    // platform calls block.
    let cameras = tokio::task::spawn_blocking(camera::list)
        .await
        .unwrap_or_default();
    if output
        .send(FromWorker::devices(recording, playout, cameras))
        .await
        .is_err()
    {
        return EXIT_FAILED;
    }
    EXIT_DONE
}

/// Linux (ADR-0049): how the chosen microphone and speaker are opened. A
/// `pulse:` choice needs its name in `PULSE_SOURCE` or `PULSE_SINK` (set by
/// the launcher) and on the sound server now (`mapping::sound_path`).
#[cfg(target_os = "linux")]
async fn sound_paths(
    recording: Option<&str>,
    playout: Option<&str>,
) -> Result<[SoundPath; 2], FailureCode> {
    let source = launch_name("PULSE_SOURCE")?;
    let sink = launch_name("PULSE_SINK")?;
    let named = [recording, playout]
        .into_iter()
        .flatten()
        .any(|id| persephone_call_protocol::pulse_name(id).is_some());
    let lists = if named {
        tokio::task::spawn_blocking(crate::pulse::query)
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    fn names(list: &[(String, String)]) -> Vec<&str> {
        list.iter().map(|(name, _)| name.as_str()).collect()
    }
    let sources = lists.as_ref().map(|lists| names(&lists.sources));
    let sinks = lists.as_ref().map(|lists| names(&lists.sinks));
    Ok([
        sound_path(recording, source.as_deref(), sources.as_deref())?,
        sound_path(playout, sink.as_deref(), sinks.as_deref())?,
    ])
}

/// One sound server name from the environment. A value that is not UTF-8
/// cannot match any choice.
#[cfg(target_os = "linux")]
fn launch_name(key: &str) -> Result<Option<String>, FailureCode> {
    match std::env::var(key) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(FailureCode::Protocol),
    }
}

/// Other systems have no sound server names: every choice goes through
/// libwebrtc's list, where a `pulse:` ID is not found.
#[cfg(not(target_os = "linux"))]
async fn sound_paths(
    _recording: Option<&str>,
    _playout: Option<&str>,
) -> Result<[SoundPath; 2], FailureCode> {
    Ok([SoundPath::Listed; 2])
}

/// The microphone and speaker a call uses: the platform IDs it opened, or
/// `None` where it left libwebrtc's own default (no listed system default).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SoundDevices {
    recording: Option<String>,
    playout: Option<String>,
    /// After a failed change the call may not use `recording`, or capture
    /// may have stopped, so the next change switches even to the same
    /// choice.
    recording_lost: bool,
    /// The same for the speaker: going back to the old one failed.
    playout_lost: bool,
}

/// The system's default speaker and microphone for media, as
/// `sound_defaults::media_defaults` read them when the call started.
/// Reading them is a blocking system query, so a change during the call
/// uses this copy and never runs it in the event loop.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct MediaDefaults {
    playout: Option<String>,
    recording: Option<String>,
}

impl MediaDefaults {
    /// Reads the defaults. It blocks, so call it before the event loop.
    fn read() -> Self {
        let (playout, recording) = sound_defaults::media_defaults();
        Self { playout, recording }
    }
}

/// Resolves the chosen microphone and speaker to listed device IDs. `None`
/// is the system's default for media where it is listed (`defaults`), else
/// `None`. A chosen device that is missing is `DeviceNotFound`, never
/// another device. Nothing changes here.
fn resolve_devices(
    audio: &PlatformAudio,
    defaults: &MediaDefaults,
    recording: Option<&str>,
    playout: Option<&str>,
) -> Result<SoundDevices, FailureCode> {
    let (default_playout, default_recording) = (&defaults.playout, &defaults.recording);
    let recording_devices: Vec<_> = audio.recording_devices().collect();
    let ids = || recording_devices.iter().map(|device| device.id.as_str());
    let recording = match recording {
        Some(_) => choose_device(recording, ids())?,
        None => system_default(default_recording.as_deref(), ids()),
    };
    let playout_devices: Vec<_> = audio.playout_devices().collect();
    let ids = || playout_devices.iter().map(|device| device.id.as_str());
    let playout = match playout {
        Some(_) => choose_device(playout, ids())?,
        None => system_default(default_playout.as_deref(), ids()),
    };
    Ok(SoundDevices {
        recording,
        playout,
        ..SoundDevices::default()
    })
}

/// The listed microphone with this platform ID, as libwebrtc names it.
/// `resolve_devices` checked it, so `None` means it left since then.
fn recording_id(audio: &PlatformAudio, id: &str) -> Result<RecordingDeviceId, FailureCode> {
    audio
        .recording_devices()
        .find(|device| device.id.as_str() == id)
        .map(|device| device.id)
        .ok_or(FailureCode::DeviceNotFound)
}

/// The listed speaker with this platform ID; see `recording_id`.
fn playout_id(audio: &PlatformAudio, id: &str) -> Result<PlayoutDeviceId, FailureCode> {
    audio
        .playout_devices()
        .find(|device| device.id.as_str() == id)
        .map(|device| device.id)
        .ok_or(FailureCode::DeviceNotFound)
}

/// Applies the chosen microphone and speaker before the sound starts. Both
/// choices are checked before either device changes.
fn select_devices(
    audio: &PlatformAudio,
    defaults: &MediaDefaults,
    recording: Option<&str>,
    playout: Option<&str>,
) -> Result<SoundDevices, FailureCode> {
    let wanted = resolve_devices(audio, defaults, recording, playout)?;
    if let Some(id) = &wanted.recording {
        audio
            .set_recording_device(&recording_id(audio, id)?)
            .map_err(|_| FailureCode::DeviceNotFound)?;
    }
    if let Some(id) = &wanted.playout {
        audio
            .set_playout_device(&playout_id(audio, id)?)
            .map_err(|_| FailureCode::DeviceNotFound)?;
    }
    Ok(wanted)
}

/// What a change during a call does, from the devices in use and the
/// resolved choice: `Ok(None)` changes nothing, `Ok(Some(..))` switches to
/// those devices. A `None` choice that names no listed device cannot move a
/// call off a device it opened by ID, so that is `Unsupported`; the choice
/// then applies to the next call.
fn device_switch(
    current: &SoundDevices,
    wanted: &SoundDevices,
) -> Result<Option<SoundDevices>, FailureCode> {
    let stranded =
        |wanted: &Option<String>, current: &Option<String>| wanted.is_none() && current.is_some();
    if stranded(&wanted.recording, &current.recording)
        || stranded(&wanted.playout, &current.playout)
    {
        return Err(FailureCode::Unsupported);
    }
    Ok((wanted != current).then(|| wanted.clone()))
}

/// Whether a `SetDevices` sound choice is no change where no device changes
/// during a call (Linux, ADR-0049): the same choice as the join. Elsewhere
/// every choice goes to `switch_devices`, which finds what changes and
/// switches a lost device again.
fn sound_unchanged_here(
    joined: &(Option<String>, Option<String>),
    recording: &Option<String>,
    playout: &Option<String>,
) -> bool {
    cfg!(target_os = "linux") && joined.0 == *recording && joined.1 == *playout
}

/// Moves the running call to other devices (`SetDevices`, ADR-0059). Both
/// choices are checked before either device changes; `apply_switch` then
/// changes them. On Linux the sound server stream was chosen when the
/// worker started (ADR-0049), so a change applies to the next call only.
fn switch_devices(
    audio: &PlatformAudio,
    defaults: &MediaDefaults,
    current: &mut SoundDevices,
    recording: Option<&str>,
    playout: Option<&str>,
) -> Result<(), FailureCode> {
    if cfg!(target_os = "linux") {
        return Err(FailureCode::Unsupported);
    }
    let wanted = resolve_devices(audio, defaults, recording, playout)?;
    let Some(next) = device_switch(current, &wanted)? else {
        return Ok(());
    };
    // Both IDs are found before either device changes.
    if let Some(id) = &next.recording {
        recording_id(audio, id)?;
    }
    if let Some(id) = &next.playout {
        playout_id(audio, id)?;
    }
    apply_switch(current, next, &mut LiveAudio(audio))
}

/// The sound device calls a change during a call makes.
trait SwitchAudio {
    /// Switches the microphone; `false` when it failed.
    fn switch_recording(&mut self, id: &str) -> bool;
    /// Switches the speaker; `false` when it failed.
    fn switch_playout(&mut self, id: &str) -> bool;
    /// Starts the microphone again where a failed switch stopped it; `true`
    /// when capture is set up after it.
    fn restart_recording(&mut self) -> bool;
}

/// `SwitchAudio` on LiveKit's platform audio. LiveKit 0.11 stops a device
/// before it opens the new one, and a failed open leaves it stopped; a
/// switch while the device is stopped only selects it. A stopped
/// microphone starts again with `start_recording`. The call always has a
/// published microphone (the join fails without one, and mute only mutes
/// the publication), so this never opens a microphone the call did not
/// use. Playout has no public start, so a stopped speaker stays stopped
/// until the call is joined again (ADR-0059).
struct LiveAudio<'a>(&'a PlatformAudio);

impl SwitchAudio for LiveAudio<'_> {
    fn switch_recording(&mut self, id: &str) -> bool {
        recording_id(self.0, id).is_ok_and(|id| self.0.switch_recording_device(&id).is_ok())
    }

    fn switch_playout(&mut self, id: &str) -> bool {
        playout_id(self.0, id).is_ok_and(|id| self.0.switch_playout_device(&id).is_ok())
    }

    fn restart_recording(&mut self) -> bool {
        (self.0.is_recording_initialized() || self.0.start_recording().is_ok())
            && self.0.is_recording_initialized()
    }
}

/// Switches the microphone, then the speaker, to `next`, and keeps
/// `current` as the devices the call uses. Each failure is `AudioDevice`.
///
/// * A microphone switch counts only when capture runs after it, so a
///   lost microphone that LiveKit only selects is started again or stays
///   lost. A failed microphone may have stopped capture: the old
///   microphone is switched back and started again. Where that fails, or
///   where there is no old ID (libwebrtc's own default), capture starts on
///   what is selected, and `recording_lost` is set.
/// * A failed speaker stopped playout, which cannot start again, so
///   `playout_lost` is set. The old speaker is selected again for when
///   playout starts. The microphone, if it moved, goes back too. Where it
///   cannot, `current` names the new microphone and `recording_lost` is
///   set; with no old ID the new microphone stays.
/// * A lost microphone chosen as libwebrtc's own default (no ID) has
///   capture started on what is selected, or stays lost.
/// * A lost speaker is only selected by a later switch, or not at all when
///   the choice has no ID: it stays lost and the change reports
///   `AudioDevice`, so the page asks to rejoin.
///
/// A lost device is switched again by the next change, even to the same
/// choice.
fn apply_switch(
    current: &mut SoundDevices,
    next: SoundDevices,
    audio: &mut impl SwitchAudio,
) -> Result<(), FailureCode> {
    let recording = next
        .recording
        .clone()
        .filter(|_| next.recording != current.recording || current.recording_lost);
    let playout = next
        .playout
        .clone()
        .filter(|_| next.playout != current.playout || current.playout_lost);
    if let Some(id) = &recording
        && !(audio.switch_recording(id) && audio.restart_recording())
    {
        current.recording_lost = !restore_recording(audio, current.recording.as_deref());
        return Err(FailureCode::AudioDevice);
    }
    // A lost microphone with no ID to switch to (libwebrtc's own default):
    // capture starts on what is selected, or the microphone stays lost.
    if recording.is_none() && current.recording_lost && !audio.restart_recording() {
        return Err(FailureCode::AudioDevice);
    }
    let speaker = playout.as_deref().map(|id| audio.switch_playout(id));
    if speaker == Some(false) {
        if let Some(old) = current.playout.as_deref() {
            audio.switch_playout(old);
        }
        current.playout_lost = true;
        if recording.is_some() {
            match current.recording.as_deref() {
                // libwebrtc's own default has no ID to go back to; the new
                // microphone runs.
                None => current.recording.clone_from(&next.recording),
                Some(old) => {
                    let back = restore_recording(audio, Some(old));
                    if !back {
                        current.recording.clone_from(&next.recording);
                    }
                    current.recording_lost = !back;
                }
            }
        }
        return Err(FailureCode::AudioDevice);
    }
    if current.playout_lost {
        // Selected only, or nothing to select (no ID): playout stays
        // stopped. The microphone, if it moved, runs.
        current.recording.clone_from(&next.recording);
        current.recording_lost = false;
        current.playout.clone_from(&next.playout);
        return Err(FailureCode::AudioDevice);
    }
    *current = next;
    Ok(())
}

/// Puts the microphone back on `old` and starts capture: `true` when the
/// old microphone runs again. Otherwise capture is started on what is
/// selected, so the call keeps a microphone where one opens.
fn restore_recording(audio: &mut impl SwitchAudio, old: Option<&str>) -> bool {
    let back = old.is_some_and(|old| audio.switch_recording(old)) && audio.restart_recording();
    if !back {
        audio.restart_recording();
    }
    back
}

/// What a device test's thread reports.
enum TestNote {
    Started,
    Level(f64),
    Done(Result<(), FailureCode>),
}

/// Runs one device test (`TestDevice`, ADR-0059), then ends. It connects
/// to nothing. The microphone and speaker tests use `sound_test`; the camera
/// test shows the camera on tile 0. `Leave` or a closed input ends a test
/// early; any other message but the diagnostic switch is a protocol failure.
pub async fn test_device(first: ToWorker, input: &mut Receiver<Input>, output: &Output) -> u8 {
    let ToWorker::TestDevice { test, device } = first else {
        return fail(output, FailureCode::Protocol).await;
    };
    // The test needs no new socket, so the second stage comes first (Linux).
    if !containment::apply_stage2() && !cfg!(feature = "dev-uncontained") {
        return fail(output, FailureCode::Containment).await;
    }
    match test {
        DeviceTest::Camera => test_camera(device, input, output).await,
        DeviceTest::Microphone => test_sound(SoundTest::Microphone, device, input, output).await,
        DeviceTest::Speaker => test_sound(SoundTest::Speaker, device, input, output).await,
    }
}

/// The device tests that use sound, so a camera test can never reach the
/// tone or the microphone.
#[derive(Clone, Copy)]
enum SoundTest {
    Microphone,
    Speaker,
}

/// What the input asks during a device test.
enum TestInput {
    /// Go on.
    Continue,
    /// End the test.
    Stop,
    Bad,
}

fn test_input(message: Option<Input>) -> TestInput {
    match message {
        Some(Input::Message(ToWorker::SetDiagnostics { on })) => {
            crate::diagnostics::global().set_on(on);
            TestInput::Continue
        }
        Some(Input::Message(ToWorker::Leave)) | Some(Input::Eof) | None => TestInput::Stop,
        Some(Input::Message(_)) | Some(Input::Bad) => TestInput::Bad,
    }
}

async fn test_sound(
    test: SoundTest,
    device: Option<String>,
    input: &mut Receiver<Input>,
    output: &Output,
) -> u8 {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (notes_tx, mut notes) = mpsc::channel::<TestNote>(16);
    let thread_stop = stop.clone();
    let spawned = std::thread::Builder::new()
        .name("device-test".to_owned())
        .spawn(move || {
            let started = notes_tx.clone();
            let levels = notes_tx.clone();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match test {
                SoundTest::Microphone => sound_test::listen(
                    device.as_deref(),
                    &thread_stop,
                    || {
                        let _ = started.try_send(TestNote::Started);
                    },
                    // A full queue drops a level; the next one follows.
                    |level| {
                        let _ = levels.try_send(TestNote::Level(level));
                    },
                ),
                SoundTest::Speaker => {
                    sound_test::play_tone(device.as_deref(), &thread_stop, || {
                        let _ = started.try_send(TestNote::Started);
                    })
                }
            }))
            .unwrap_or(Err(FailureCode::Internal));
            let _ = notes_tx.blocking_send(TestNote::Done(result));
        });
    if spawned.is_err() {
        return fail(output, FailureCode::Internal).await;
    }
    let mut meters = LevelMeters::default();
    let mut bad = false;
    loop {
        tokio::select! {
            note = notes.recv() => match note {
                Some(TestNote::Started) => {
                    if output.send(FromWorker::Connected).await.is_err() {
                        stop.store(true, std::sync::atomic::Ordering::Release);
                    }
                }
                Some(TestNote::Level(linear)) => {
                    if let Some(message) = meters.own(linear, std::time::Instant::now())
                        && output.send(message).await.is_err()
                    {
                        stop.store(true, std::sync::atomic::Ordering::Release);
                    }
                }
                Some(TestNote::Done(Ok(()))) if !bad => {
                    let _ = output.send(FromWorker::Left).await;
                    return EXIT_DONE;
                }
                Some(TestNote::Done(Ok(()))) => return fail(output, FailureCode::Protocol).await,
                Some(TestNote::Done(Err(code))) => return fail(output, code).await,
                None => return fail(output, FailureCode::Internal).await,
            },
            message = input.recv(), if !stop.load(std::sync::atomic::Ordering::Acquire) => {
                match test_input(message) {
                    TestInput::Continue => {}
                    TestInput::Stop => stop.store(true, std::sync::atomic::Ordering::Release),
                    TestInput::Bad => {
                        bad = true;
                        stop.store(true, std::sync::atomic::Ordering::Release);
                    }
                }
            }
        }
    }
}

async fn test_camera(device: Option<String>, input: &mut Receiver<Input>, output: &Output) -> u8 {
    let pictures = output.pictures();
    let mut limiter = RateLimiter::default();
    let (ended_tx, mut ended) = mpsc::channel::<()>(1);
    let (open_tx, mut opened) = mpsc::channel::<Opened<camera::Capture, CameraState>>(1);
    // No portal grant comes with a test: the portal path reports
    // `Unavailable`, the other paths open the camera directly.
    let started = spawn_open(open_tx, 0, CameraState::Unavailable, move || {
        camera::open(
            device.as_deref(),
            camera::Grant::default(),
            move |frame| {
                if !limiter.admit(std::time::Instant::now()) {
                    return;
                }
                if let Some(picture) = fitted_i420(&frame)
                    && let Some((width, height, rgba)) = to_rgba(OWN_TILE, &picture)
                {
                    pictures.put(OWN_TILE, 0, width, height, rgba);
                }
            },
            move || {
                let _ = ended_tx.try_send(());
            },
        )
    });
    if started.is_err() {
        return fail(output, FailureCode::Internal).await;
    }
    // The open blocks on its thread; `Leave` meanwhile ends the test, and the
    // late result is dropped there, which releases the camera.
    let capture = loop {
        tokio::select! {
            done = opened.recv() => match done.map(|done| done.result) {
                Some(Ok(capture)) => break capture,
                Some(Err(state)) => {
                    if output.send(FromWorker::Camera { state }).await.is_ok() {
                        let _ = output.send(FromWorker::Left).await;
                    }
                    return EXIT_DONE;
                }
                None => return fail(output, FailureCode::Internal).await,
            },
            message = input.recv() => match test_input(message) {
                TestInput::Continue => {}
                TestInput::Stop => {
                    let _ = output.send(FromWorker::Left).await;
                    return EXIT_DONE;
                }
                TestInput::Bad => return fail(output, FailureCode::Protocol).await,
            },
        }
    };
    let mut code = None;
    if output.send(FromWorker::Connected).await.is_err()
        || output
            .send(FromWorker::Camera {
                state: CameraState::On,
            })
            .await
            .is_err()
    {
        code = Some(FailureCode::Internal);
    }
    let limit = tokio::time::sleep(CAMERA_TEST_TIME);
    tokio::pin!(limit);
    while code.is_none() {
        tokio::select! {
            () = &mut limit => break,
            _ = ended.recv() => break,
            message = input.recv() => match test_input(message) {
                TestInput::Continue => {}
                TestInput::Stop => break,
                TestInput::Bad => code = Some(FailureCode::Protocol),
            },
        }
    }
    // Stopping joins the capture thread, which blocks.
    let held = tokio::task::spawn_blocking(move || capture.stop().is_some())
        .await
        .unwrap_or(true);
    output.clear_picture(OWN_TILE);
    if let Some(code) = code {
        return fail(output, code).await;
    }
    let state = if held {
        CameraState::Unavailable
    } else {
        CameraState::Off
    };
    for message in [
        FromWorker::VideoStopped { tile: OWN_TILE },
        FromWorker::Camera { state },
        FromWorker::Left,
    ] {
        if output.send(message).await.is_err() {
            return EXIT_FAILED;
        }
    }
    EXIT_DONE
}

/// Everything the call changes after it is connected.
struct Call<'a> {
    room: &'a Room,
    provider: &'a KeyProvider,
    identity: &'a ParticipantIdentity,
    /// The microphone and speaker, for a change during the call.
    audio: &'a PlatformAudio,
    /// The devices the call uses now (ADR-0059).
    sound_devices: SoundDevices,
    /// The system defaults read when the call started, for "System default"
    /// during the call.
    media_defaults: MediaDefaults,
    output: &'a Output,
    /// The microphone's publication.
    publication: &'a LocalTrackPublication,
    /// The SIDs of every own track (microphone and camera).
    own_sids: Vec<TrackSid>,
    /// The camera the next camera start opens: the join's choice, then the
    /// newest `SetDevices` choice (ADR-0059).
    camera_device: Option<String>,
    /// The microphone and speaker choice of the join. On Linux no device
    /// changes during a call, so a `SetDevices` with this choice (a change of
    /// the camera only) is no sound change there.
    joined_sound: (Option<String>, Option<String>),
    camera: Option<LocalCamera>,
    /// A capture that did not end in time after a stop (a blocked read on a
    /// stalled camera). Its frames are already revoked. No new camera opens
    /// while it runs.
    stalled: Option<camera::Capture>,
    /// Decides what an open request and its completion do. It also holds the
    /// key index every own track encrypts with.
    gate: CameraGate,
    /// The capture thread reports its generation here when it ends by itself.
    camera_failed: mpsc::Sender<u64>,
    /// The open thread reports its result here.
    open_tx: mpsc::Sender<Opened<OpenedCamera, CameraState>>,
    /// The own screen share while it is on (ADR-0053).
    screen: Option<LocalScreen>,
    /// A screen capture that did not end in time after a stop.
    screen_stalled: Option<camera::Capture>,
    /// The same open rules as the camera, for the screen share.
    screen_gate: CameraGate,
    /// A capture thread reports its generation and why it ended here.
    screen_ends: Arc<ScreenEnd>,
    /// Why the capture of the open in flight ended before the open
    /// completed, with its generation.
    screen_end_reason: Option<(u64, screen::Ended)>,
    /// A start that came while a stopped open was still in flight, with its
    /// share number. It runs when that open ends, so its replies follow the
    /// stop's.
    screen_next: NextStart<screen::Grant>,
    /// The core's number for the share that runs or opens now. Every
    /// `Screen` state about it carries this number.
    screen_share: u32,
    screen_open_tx: mpsc::Sender<Opened<OpenedScreen, ScreenState>>,
    remote: RemoteVideo,
    /// The room options for a connection to another SFU (ADR-0054).
    other_options: RoomOptions,
    /// A connect task reports here.
    connect_tx: mpsc::Sender<Connected>,
    /// The connections to other SFUs by index (1 to 3). An index is used
    /// once: a failed or ended connection is never reopened.
    others: Others,
    /// The volume and deafen state of every remote audio track, on every
    /// connection (ADR-0055).
    playout: Playout,
    /// The level meters' measurement of the own and received sound
    /// (ADR-0055, ADR-0059).
    sound: SoundProbe,
}

/// Measures the own and the received sound for the level meters (ADR-0055,
/// ADR-0059). At most one measurement runs, on its own task, so the event
/// loop never waits for it.
struct SoundProbe {
    /// A measurement reports its levels here, or `None` when it did not
    /// finish.
    done: mpsc::Sender<Option<Levels>>,
    task: Option<JoinHandle<()>>,
    meters: LevelMeters,
}

/// One measurement's report. Dropping it sends the levels, or `None`, so
/// the task reports once however it ends: done, timed out or unwound. Only
/// one measurement runs and the slot is freed only by its report, so the
/// channel of one place has room.
struct SoundReport {
    done: mpsc::Sender<Option<Levels>>,
    levels: Option<Levels>,
}

impl Drop for SoundReport {
    fn drop(&mut self) {
        let _ = self.done.try_send(self.levels.take());
    }
}

impl SoundProbe {
    fn new(done: mpsc::Sender<Option<Levels>>) -> Self {
        Self {
            done,
            task: None,
            meters: LevelMeters::default(),
        }
    }

    /// Whether a measurement runs: its report has not reached [`Self::end`].
    fn is_busy(&self) -> bool {
        self.task.is_some()
    }

    /// Starts `measure` on its own task, stopped after `limit`, unless a
    /// measurement runs. Its report comes back through `done` exactly once:
    /// the level, or `None` when it timed out or unwound.
    fn start<F>(&mut self, measure: F, limit: Duration) -> bool
    where
        F: std::future::Future<Output = Levels> + Send + 'static,
    {
        if self.is_busy() {
            return false;
        }
        let done = self.done.clone();
        self.task = Some(tokio::spawn(async move {
            // Reports when dropped, also if the measurement unwinds, so the
            // slot is always freed. A timeout reports `None`, not silence.
            let mut report = SoundReport { done, levels: None };
            report.levels = tokio::time::timeout(limit, measure).await.ok();
        }));
        true
    }

    /// A measurement's report arrived: the slot is free. One that did not
    /// finish (`None`) changes nothing, so the meters keep their levels
    /// until the next one.
    fn end(&mut self, levels: Option<Levels>) -> Vec<FromWorker> {
        self.task = None;
        match levels {
            Some(levels) => self.meters.next(&levels, std::time::Instant::now()),
            None => Vec::new(),
        }
    }
}

/// The levels of the own microphone and of each remote participant.
///
/// The own level is the `audioLevel` of the microphone's media source: the
/// sound as it is sent, so it is silence while muted. A remote level is the
/// loudest `audioLevel` of the participant's inbound streams. WebRTC
/// measures it on the audio as it is played, after the call volume:
/// `ChannelReceive::GetAudioFrameWithInfo` scales the frame by the output
/// gain (`set_volume`) before `output_audio_level_.ComputeLevel`, and a
/// disabled (deafened) track sets that gain to 0
/// (`AudioRtpReceiver::Reconfigure`). A track whose statistics cannot be
/// read counts as silent. A read that does not finish by `deadline` is
/// `None`: its meter keeps its level, so one slow read never holds back the
/// own level or the other tracks' levels, and never shows as silence.
async fn measure_levels(
    own: Option<LocalAudioTrack>,
    remote: Vec<(u8, String, RemoteAudioTrack)>,
    deadline: tokio::time::Instant,
) -> Levels {
    let mut levels = Levels {
        own: Some(0.0),
        remote: Vec::new(),
    };
    if let Some(track) = own {
        levels.own = read_by(deadline, track.get_stats()).await.map(|stats| {
            stats
                .into_iter()
                .flatten()
                .filter_map(|stat| match stat {
                    RtcStats::MediaSource(source) if source.source.kind == "audio" => {
                        Some(source.audio.audio_level)
                    }
                    _ => None,
                })
                .fold(0.0_f64, f64::max)
        });
    }
    let (sources, reads): (Vec<_>, Vec<_>) = remote
        .into_iter()
        .map(|(connection, identity, track)| {
            (
                (connection, identity),
                async move { track.get_stats().await },
            )
        })
        .unzip();
    for ((connection, identity), stats) in sources.into_iter().zip(read_all(deadline, reads).await)
    {
        let loudest = stats.map(|stats| {
            stats
                .into_iter()
                .flatten()
                .filter_map(|stat| match stat {
                    RtcStats::InboundRtp(inbound) => Some(inbound.inbound.audio_level),
                    _ => None,
                })
                .fold(0.0_f64, f64::max)
        });
        levels.remote.push((connection, identity, loudest));
    }
    levels
}

/// Runs every read at once, each bounded by `deadline` (`read_by`), and
/// gives their outputs in order: `None` for a read that did not finish in
/// time. One slow read never keeps another from starting. The reads are at
/// most [`MAX_MEASURED_TRACKS`], and dropping this ends them all.
async fn read_all<F>(deadline: tokio::time::Instant, reads: Vec<F>) -> Vec<Option<F::Output>>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let mut out: Vec<Option<F::Output>> =
        std::iter::repeat_with(|| None).take(reads.len()).collect();
    let mut running = tokio::task::JoinSet::new();
    for (index, read) in reads.into_iter().enumerate() {
        running.spawn(async move { (index, read_by(deadline, read).await) });
    }
    while let Some(done) = running.join_next().await {
        if let Ok((index, output)) = done {
            out[index] = output;
        }
    }
    out
}

/// `read`'s output when it finishes by `deadline`, else `None`. A read
/// whose deadline has passed is never started.
async fn read_by<F: std::future::Future>(
    deadline: tokio::time::Instant,
    read: F,
) -> Option<F::Output> {
    if tokio::time::Instant::now() >= deadline {
        return None;
    }
    tokio::time::timeout_at(deadline, read).await.ok()
}

/// The connections to other SFUs (ADR-0054). They subscribe only: nothing
/// here publishes a track or sets an own key index.
type Others = Slots<JoinHandle<()>, Room>;

/// The result of one connect task.
struct Connected {
    index: u8,
    result: Result<(Room, UnboundedReceiver<RoomEvent>), ()>,
}

/// The event receivers of the rooms of other SFUs. The SDK queue of each is
/// unbounded, as for the own room; one loop drains all of them.
#[derive(Default)]
struct OtherEvents {
    receivers: [Option<UnboundedReceiver<RoomEvent>>; MAX_OTHER_CONNECTIONS as usize],
    /// Where the next poll starts, so no room starves the others.
    next_start: usize,
}

impl OtherEvents {
    fn insert(&mut self, index: u8, events: UnboundedReceiver<RoomEvent>) {
        if let Some(slot) = usize::from(index)
            .checked_sub(1)
            .and_then(|slot| self.receivers.get_mut(slot))
        {
            *slot = Some(events);
        }
    }

    fn remove(&mut self, index: u8) {
        if let Some(slot) = usize::from(index)
            .checked_sub(1)
            .and_then(|slot| self.receivers.get_mut(slot))
        {
            *slot = None;
        }
    }

    /// The next event of any connection, with its index. `None` as the item
    /// means that room's queue closed. With no connection it never ends.
    async fn next(&mut self) -> (u8, Option<RoomEvent>) {
        let count = self.receivers.len();
        let start = self.next_start % count;
        let found = std::future::poll_fn(|cx| {
            for step in 0..count {
                let slot = (start + step) % count;
                if let Some(receiver) = self.receivers[slot].as_mut()
                    && let Poll::Ready(item) = receiver.poll_recv(cx)
                {
                    return Poll::Ready((slot, item));
                }
            }
            Poll::Pending
        })
        .await;
        self.next_start = found.0 + 1;
        (found.0 as u8 + 1, found.1)
    }
}

/// What the event loop waits for about the connections to other SFUs.
struct OtherInbox {
    events: OtherEvents,
    connected: Receiver<Connected>,
}

/// What the event loop waits for about the screen share.
struct ScreenInbox {
    /// A capture thread reports its generation and why it ended.
    ended: Arc<ScreenEnd>,
    opened: Receiver<Opened<OpenedScreen, ScreenState>>,
}

/// The end of a screen capture: one slot that keeps the newest generation.
/// A stale end never takes the place of the current capture's end, and the
/// slot never grows. Only the newest generation can still matter: a new
/// open starts only when no capture runs.
#[derive(Default)]
struct ScreenEnd {
    latest: std::sync::Mutex<Option<(u64, screen::Ended)>>,
    ready: tokio::sync::Notify,
}

impl ScreenEnd {
    /// The capture of `generation` ended. Called on the capture thread.
    fn report(&self, generation: u64, ended: screen::Ended) {
        {
            let mut latest = self
                .latest
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if latest.is_none_or(|(kept, _)| kept < generation) {
                *latest = Some((generation, ended));
            }
        }
        self.ready.notify_one();
    }

    /// The end in the slot, if any; the slot is then empty.
    fn take(&self) -> Option<(u64, screen::Ended)> {
        self.latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Waits for the next end. The slot is read before each wait, so an end
    /// reported while no one waited is not lost.
    async fn next(&self) -> (u64, screen::Ended) {
        loop {
            if let Some(end) = self.take() {
                return end;
            }
            self.ready.notified().await;
        }
    }
}

/// A screen start that waits for an open that a stop cancelled. It keeps
/// its own target, grant and share number: it never takes the stopped
/// open's, and the stopped open never takes its grant.
struct NextStart<G> {
    waiting: Option<(ScreenTarget, G, u32)>,
}

impl<G> Default for NextStart<G> {
    fn default() -> Self {
        Self { waiting: None }
    }
}

impl<G> NextStart<G> {
    /// The start to run now, or `None` when an open that a stop cancelled
    /// is still in flight: the start then waits here until that open ends.
    /// A start that waited already is replaced, and its grant drops.
    fn admit(
        &mut self,
        gate: &CameraGate,
        target: ScreenTarget,
        grant: G,
        share: u32,
    ) -> Option<(ScreenTarget, G, u32)> {
        if gate.opening() && !gate.wanted() {
            self.waiting = Some((target, grant, share));
            return None;
        }
        Some((target, grant, share))
    }

    /// The start that waited, once the stopped open has ended.
    fn take(&mut self) -> Option<(ScreenTarget, G, u32)> {
        self.waiting.take()
    }

    /// A stop drops the start that waited, with its grant.
    fn clear(&mut self) {
        self.waiting = None;
    }
}

/// The own screen share while it is on.
struct LocalScreen {
    generation: u64,
    capture: camera::Capture,
    sid: TrackSid,
    live: Arc<FrameAdmission>,
}

/// An open screen capture that is not yet published.
struct OpenedScreen {
    capture: camera::Capture,
    source: NativeVideoSource,
    live: Arc<FrameAdmission>,
}

/// Gives one remote audio track the call's playout. The gain is set first,
/// so a track that is enabled here never plays at the old volume.
fn play_remote_audio(track: &RemoteAudioTrack, playout: Playout) {
    track.rtc_track().set_volume(playout.gain());
    if playout.enabled() {
        track.enable();
    } else {
        track.disable();
    }
}

/// The own camera while it is on.
struct LocalCamera {
    generation: u64,
    capture: camera::Capture,
    sid: TrackSid,
    /// The feed's admission. Closed before a stop, so no picture leaves
    /// after a blocked read returns.
    live: Arc<FrameAdmission>,
}

/// One remote video track: the participant's identity and the track's SID.
type TrackKey = (String, String, u8);

/// Remote video: which tracks have a tile, which wait, and the tasks that read
/// the pictures of each tile.
#[derive(Default)]
struct RemoteVideo {
    tiles: TileAssigner<TrackKey>,
    /// The screen shares: one is decoded on the screen tile, the rest wait.
    screens: ScreenQueue<TrackKey>,
    /// Tracks that wait for a tile.
    waiting: HashMap<TrackKey, RtcVideoTrack>,
    /// The picture reader of each tile (index 0 is never used).
    readers: [Option<JoinHandle<()>>; VIDEO_TILES as usize],
}

/// Whether camera frames may go out. A frame holds the read side for its
/// whole callback, so [`FrameAdmission::close`] returns only when no frame is
/// in progress, and no frame starts after it: a callback that passed the
/// check cannot submit a picture after the stop.
#[derive(Default)]
struct FrameAdmission(RwLock<bool>);

impl FrameAdmission {
    /// Frames may go out from now on.
    fn open(&self) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = true;
    }

    /// No frame goes out from now on. Waits for a frame in progress.
    fn close(&self) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = false;
    }

    /// Admits one frame; the frame may go out while the guard lives.
    fn admit(&self) -> Option<RwLockReadGuard<'_, bool>> {
        let open = self.0.read().unwrap_or_else(|e| e.into_inner());
        if *open { Some(open) } else { None }
    }
}

/// The camera's frame callback: converts, scales and hands the picture to the
/// SFU source and to the own preview.
struct CameraFeed {
    source: NativeVideoSource,
    /// Closed until the track is published and its key index is set, so no
    /// picture goes out under a wrong key index, and closed again before a
    /// stop.
    live: Arc<FrameAdmission>,
    pictures: PictureSink,
    limiter: RateLimiter,
}

impl CameraFeed {
    fn frame(&mut self, frame: CapturedFrame<'_>) {
        // Held until the picture is handed on, so a stop waits for it.
        let Some(_admitted) = self.live.admit() else {
            return;
        };
        if !self.limiter.admit(std::time::Instant::now()) {
            return;
        }
        let Some(picture) = fitted_i420(&frame) else {
            return;
        };
        if let Some((width, height, rgba)) = to_rgba(OWN_TILE, &picture) {
            self.pictures.put(OWN_TILE, 0, width, height, rgba);
        }
        self.source
            .capture_frame(&VideoFrame::new(VideoRotation::VideoRotation0, picture));
    }
}

/// The screen capture's frame callback: converts, scales and hands the
/// picture to the SFU source. There is no own preview.
struct ScreenFeed {
    source: NativeVideoSource,
    live: Arc<FrameAdmission>,
    limiter: RateLimiter,
}

impl ScreenFeed {
    fn frame(&mut self, frame: screen::CapturedFrame<'_>) {
        // Held until the picture is handed on, so a stop waits for it.
        let Some(_admitted) = self.live.admit() else {
            return;
        };
        if !self.limiter.admit(std::time::Instant::now()) {
            return;
        }
        let Some(picture) = screen_i420(&frame) else {
            return;
        };
        self.source
            .capture_frame(&VideoFrame::new(VideoRotation::VideoRotation0, picture));
    }
}

impl Call<'_> {
    /// Starts turning the camera on. The open runs on its own thread; the
    /// result arrives in the event loop (`camera_opened`). Returns a state to
    /// report now: `On` when the camera already runs, `Unavailable` when the
    /// open could not start, nothing while an open is in flight.
    ///
    /// `grant` is what the camera-on brought. An open takes it; a camera-on
    /// that opens nothing drops it, which closes its portal remote.
    fn begin_camera(&mut self, grant: camera::Grant) -> Option<CameraState> {
        // A capture that did not stop still holds the camera: open no second
        // one. With no open in flight, any capture thread that still runs (a
        // stalled stop, or an open that timed out) holds a camera too.
        if self.camera.is_none()
            && (self.stalled_running() || (!self.gate.opening() && camera::captures_running()))
        {
            return Some(CameraState::Unavailable);
        }
        let generation = match self.gate.request_on(self.camera.is_some()) {
            OnRequest::AlreadyOn => return Some(CameraState::On),
            OnRequest::Pending => return None,
            OnRequest::Start(generation) => generation,
        };
        let source = NativeVideoSource::new(
            VideoResolution {
                width: MAX_VIDEO_SIDE,
                height: MAX_VIDEO_PIXELS / MAX_VIDEO_SIDE,
            },
            false,
        );
        let live = Arc::new(FrameAdmission::default());
        let mut feed = CameraFeed {
            source: source.clone(),
            live: live.clone(),
            pictures: self.output.pictures(),
            limiter: RateLimiter::default(),
        };
        let failed = self.camera_failed.clone();
        let choice = self.camera_device.clone();
        // Opening blocks: the platform calls and the wait for the first frame
        // (up to `OPEN_TIMEOUT`, which includes a permission dialog on macOS).
        let started = spawn_open(
            self.open_tx.clone(),
            generation,
            CameraState::Unavailable,
            move || {
                let capture = camera::open(
                    choice.as_deref(),
                    grant,
                    move |frame| feed.frame(frame),
                    move || {
                        let _ = failed.try_send(generation);
                    },
                )?;
                Ok(OpenedCamera {
                    capture,
                    source,
                    live,
                })
            },
        );
        if started.is_err() {
            self.gate.complete(generation, false);
            return Some(CameraState::Unavailable);
        }
        None
    }

    /// The open ended. A current result is published. One the camera was
    /// turned off for meanwhile is released, and the deferred off is reported
    /// now. A stale one is released and not reported.
    async fn camera_opened(
        &mut self,
        done: Opened<OpenedCamera, CameraState>,
    ) -> Result<(), Closed> {
        let generation = done.generation;
        let decision = self.gate.complete(generation, done.result.is_ok());
        match (decision, done.result) {
            (Completion::Release, result) => {
                if let Ok(opened) = result {
                    self.release_capture(opened.capture).await;
                }
                Ok(())
            }
            (Completion::Cancelled, result) => {
                let released = match result {
                    Ok(opened) => self.release_capture(opened.capture).await,
                    Err(_) => true,
                };
                // Off only when no capture still holds the camera: neither a
                // stalled one nor the thread of an open that timed out.
                let state = if released && !self.stalled_running() && !camera::captures_running() {
                    CameraState::Off
                } else {
                    CameraState::Unavailable
                };
                self.output.send(FromWorker::Camera { state }).await
            }
            // The capture ended before this completion: it is dead, not on.
            (Completion::Ended, result) => {
                if let Ok(opened) = result {
                    self.release_capture(opened.capture).await;
                }
                self.output
                    .send(FromWorker::Camera {
                        state: CameraState::Unavailable,
                    })
                    .await
            }
            (Completion::Report, Err(state)) => {
                self.output.send(FromWorker::Camera { state }).await
            }
            (Completion::Publish, Ok(opened)) => {
                let state = self.publish_camera(generation, opened).await;
                self.output.send(FromWorker::Camera { state }).await
            }
            // `complete` pairs `Publish` with `Ok` and `Report` with `Err`.
            _ => Ok(()),
        }
    }

    /// Publishes the opened camera and sets its key index.
    async fn publish_camera(&mut self, generation: u64, opened: OpenedCamera) -> CameraState {
        let OpenedCamera {
            capture,
            source,
            live,
        } = opened;
        let track = LocalVideoTrack::create_video_track("camera", RtcVideoSource::Native(source));
        let options = TrackPublishOptions {
            source: TrackSource::Camera,
            video_codec: VideoCodec::VP8,
            simulcast: false,
            video_encoding: Some(VideoEncoding {
                max_bitrate: 600_000,
                max_framerate: f64::from(MAX_VIDEO_FPS),
            }),
            ..TrackPublishOptions::default()
        };
        let published = self
            .room
            .local_participant()
            .publish_track(LocalTrack::Video(track), options)
            .await;
        let Ok(publication) = published else {
            self.gate.failed_to_publish();
            self.release_capture(capture).await;
            return CameraState::Unavailable;
        };
        let sid = publication.sid();
        self.own_sids.push(sid.clone());
        // The sender's frame cryptor exists once `publish_track` returns. The
        // loop handled every `SetOwnKey` up to now (it waits during the
        // publish), so this is the latest index, not the one at start.
        use_own_key_index(
            self.room.e2ee_manager(),
            &self.own_sids,
            self.gate.own_index(),
        );
        live.open();
        self.camera = Some(LocalCamera {
            generation,
            capture,
            sid,
            live,
        });
        CameraState::On
    }

    /// Turns the camera off: revokes frame admission, ends and joins the
    /// capture thread (the camera light goes off), drops the waiting preview
    /// picture and unpublishes the track. A capture that does not end in time
    /// is kept in `stalled`; no picture can leave it.
    async fn stop_camera(&mut self, unpublish: bool) -> CameraStop {
        let Some(camera) = self.camera.take() else {
            return CameraStop::WasOff;
        };
        // Off the runtime threads: the close waits for a frame in progress.
        let live = camera.live.clone();
        let _ = tokio::task::spawn_blocking(move || live.close()).await;
        let released = self.release_capture(camera.capture).await;
        self.output.clear_picture(OWN_TILE);
        self.own_sids.retain(|sid| *sid != camera.sid);
        if unpublish {
            let _ = self
                .room
                .local_participant()
                .unpublish_track(&camera.sid)
                .await;
        }
        if released {
            CameraStop::Released
        } else {
            CameraStop::Stalled
        }
    }

    /// Stops a capture. One that does not end in time goes to `stalled`.
    /// Returns whether the capture ended.
    async fn release_capture(&mut self, capture: camera::Capture) -> bool {
        let Some(stalled) = stop_capture(capture).await else {
            return true;
        };
        if let Some(older) = self.stalled.replace(stalled) {
            // Not expected: no camera opens while one is stalled. Drop the older
            // one off the runtime threads, because its drop waits.
            let _ = tokio::task::spawn_blocking(move || drop(older)).await;
        }
        false
    }

    /// Whether a stalled capture still runs. Joins one that has ended.
    fn stalled_running(&mut self) -> bool {
        if let Some(stalled) = self.stalled.as_mut()
            && stalled.try_finish()
        {
            self.stalled = None;
        }
        self.stalled.is_some()
    }

    /// Ends every video task before the room closes. A finished open that
    /// nobody took is released. An open still running ends on its own: its
    /// result goes nowhere and is dropped, which releases the camera.
    async fn shutdown(
        &mut self,
        open_done: &mut Receiver<Opened<OpenedCamera, CameraState>>,
        screens: &mut ScreenInbox,
    ) {
        if let Some(task) = self.sound.task.take() {
            task.abort();
        }
        open_done.close();
        while let Ok(done) = open_done.try_recv() {
            if let Ok(opened) = done.result {
                self.release_capture(opened.capture).await;
            }
        }
        screens.opened.close();
        while let Ok(done) = screens.opened.try_recv() {
            if let Ok(opened) = done.result {
                self.release_screen_capture(opened.capture).await;
            }
        }
        self.stop_camera(false).await;
        self.stop_screen(false).await;
        // The drop of a stalled capture waits up to the stop timeout.
        if let Some(stalled) = self.stalled.take() {
            let _ = tokio::task::spawn_blocking(move || drop(stalled)).await;
        }
        if let Some(stalled) = self.screen_stalled.take() {
            let _ = tokio::task::spawn_blocking(move || drop(stalled)).await;
        }
        for reader in self.remote.readers.iter_mut().filter_map(Option::take) {
            reader.abort();
            let _ = reader.await;
        }
    }

    /// The capture thread ended by itself.
    async fn camera_ended(&mut self, generation: u64) -> Result<(), Closed> {
        if self
            .camera
            .as_ref()
            .is_none_or(|camera| camera.generation != generation)
        {
            // The open for this generation may not have completed yet. The gate
            // remembers it, so the completion does not publish a dead capture.
            self.gate.capture_ended(generation);
            return Ok(());
        }
        self.stop_camera(true).await;
        self.output
            .send(FromWorker::VideoStopped { tile: OWN_TILE })
            .await?;
        self.output
            .send(FromWorker::Camera {
                state: CameraState::Unavailable,
            })
            .await
    }

    /// Starts turning the screen share on. The open runs on its own thread;
    /// the result arrives in the event loop (`screen_opened`). Returns a
    /// state to report now: `On` when the share already runs, `Unavailable`
    /// when the open could not start, nothing while an open is in flight.
    fn begin_screen(&mut self, target: ScreenTarget, grant: screen::Grant) -> Option<ScreenState> {
        // A capture that did not stop still holds the screen: start no
        // second one.
        if self.screen.is_none()
            && (self.screen_stalled_running()
                || (!self.screen_gate.opening() && screen::captures_running()))
        {
            return Some(ScreenState::Unavailable);
        }
        let generation = match self.screen_gate.request_on(self.screen.is_some()) {
            OnRequest::AlreadyOn => return Some(ScreenState::On),
            OnRequest::Pending => return None,
            OnRequest::Start(generation) => generation,
        };
        let source = NativeVideoSource::new(
            VideoResolution {
                width: 1920,
                height: 1080,
            },
            // The screencast flag: the encoder keeps text sharp.
            true,
        );
        let live = Arc::new(FrameAdmission::default());
        let mut feed = ScreenFeed {
            source: source.clone(),
            live: live.clone(),
            limiter: RateLimiter::for_tile(SCREEN_TILE),
        };
        let ends = self.screen_ends.clone();
        let started = spawn_open(
            self.screen_open_tx.clone(),
            generation,
            ScreenState::Unavailable,
            move || {
                let capture = screen::open(
                    target,
                    grant,
                    move |frame| feed.frame(frame),
                    move |ended| ends.report(generation, ended),
                )?;
                Ok(OpenedScreen {
                    capture,
                    source,
                    live,
                })
            },
        );
        if started.is_err() {
            self.screen_gate.complete(generation, false);
            return Some(ScreenState::Unavailable);
        }
        None
    }

    /// The screen open ended. A current result is published; a stale one is
    /// released.
    async fn screen_opened(
        &mut self,
        done: Opened<OpenedScreen, ScreenState>,
    ) -> Result<(), Closed> {
        let generation = done.generation;
        let share = self.screen_share;
        let decision = self.screen_gate.complete(generation, done.result.is_ok());
        match (decision, done.result) {
            (Completion::Release, result) => {
                if let Ok(opened) = result {
                    self.release_screen_capture(opened.capture).await;
                }
                Ok(())
            }
            (Completion::Cancelled, result) => {
                let released = match result {
                    Ok(opened) => self.release_screen_capture(opened.capture).await,
                    Err(_) => true,
                };
                let state =
                    if released && !self.screen_stalled_running() && !screen::captures_running() {
                        ScreenState::Off
                    } else {
                        ScreenState::Unavailable
                    };
                self.output
                    .send(FromWorker::Screen { state, share })
                    .await?;
                // A start that waited for this stopped open runs now, after
                // the stop's state.
                if let Some((target, grant, next)) = self.screen_next.take() {
                    for message in self.screen_start(target, grant, next) {
                        self.output.send(message).await?;
                    }
                }
                Ok(())
            }
            // The capture ended before this completion: it is dead, not on.
            // The state says why, as it would after a completion.
            (Completion::Ended, result) => {
                if let Ok(opened) = result {
                    self.release_screen_capture(opened.capture).await;
                }
                let state = match self.screen_end_reason.take() {
                    Some((ended, screen::Ended::Failed)) if ended == generation => {
                        ScreenState::Unavailable
                    }
                    _ => ScreenState::Off,
                };
                self.output.send(FromWorker::Screen { state, share }).await
            }
            (Completion::Report, Err(state)) => {
                self.output.send(FromWorker::Screen { state, share }).await
            }
            (Completion::Publish, Ok(opened)) => {
                let state = self.publish_screen(generation, opened).await;
                self.output.send(FromWorker::Screen { state, share }).await
            }
            // `complete` pairs `Publish` with `Ok` and `Report` with `Err`.
            _ => Ok(()),
        }
    }

    /// Publishes the opened screen share and sets its key index.
    async fn publish_screen(&mut self, generation: u64, opened: OpenedScreen) -> ScreenState {
        let OpenedScreen {
            capture,
            source,
            live,
        } = opened;
        let track = LocalVideoTrack::create_video_track("screen", RtcVideoSource::Native(source));
        let options = TrackPublishOptions {
            source: TrackSource::Screenshare,
            video_codec: VideoCodec::VP8,
            // One layer only.
            simulcast: false,
            video_encoding: Some(VideoEncoding {
                max_bitrate: SCREEN_MAX_BITRATE,
                max_framerate: f64::from(MAX_SCREEN_FPS),
            }),
            ..TrackPublishOptions::default()
        };
        let published = self
            .room
            .local_participant()
            .publish_track(LocalTrack::Video(track), options)
            .await;
        let Ok(publication) = published else {
            self.screen_gate.failed_to_publish();
            self.release_screen_capture(capture).await;
            return ScreenState::Unavailable;
        };
        let sid = publication.sid();
        self.own_sids.push(sid.clone());
        // The sender's frame cryptor exists once `publish_track` returns.
        // Every key command up to now was handled, so this is the latest
        // index. No frame goes out before it is set.
        use_own_key_index(
            self.room.e2ee_manager(),
            &self.own_sids,
            self.screen_gate.own_index(),
        );
        live.open();
        self.screen = Some(LocalScreen {
            generation,
            capture,
            sid,
            live,
        });
        ScreenState::On
    }

    /// Turns the screen share off: revokes frame admission, ends and joins
    /// the capture thread, and unpublishes the track.
    async fn stop_screen(&mut self, unpublish: bool) -> CameraStop {
        let Some(screen) = self.screen.take() else {
            return CameraStop::WasOff;
        };
        let live = screen.live.clone();
        let _ = tokio::task::spawn_blocking(move || live.close()).await;
        let released = self.release_screen_capture(screen.capture).await;
        self.own_sids.retain(|sid| *sid != screen.sid);
        if unpublish {
            let _ = self
                .room
                .local_participant()
                .unpublish_track(&screen.sid)
                .await;
        }
        if released {
            CameraStop::Released
        } else {
            CameraStop::Stalled
        }
    }

    /// Stops a screen capture. One that does not end in time goes to
    /// `screen_stalled`. Returns whether the capture ended.
    async fn release_screen_capture(&mut self, capture: camera::Capture) -> bool {
        let Some(stalled) = stop_capture(capture).await else {
            return true;
        };
        if let Some(older) = self.screen_stalled.replace(stalled) {
            let _ = tokio::task::spawn_blocking(move || drop(older)).await;
        }
        false
    }

    /// Whether a stalled screen capture still runs. Joins one that ended.
    fn screen_stalled_running(&mut self) -> bool {
        if let Some(stalled) = self.screen_stalled.as_mut()
            && stalled.try_finish()
        {
            self.screen_stalled = None;
        }
        self.screen_stalled.is_some()
    }

    /// The screen capture thread ended by itself: the source went (`Off`)
    /// or capture failed (`Unavailable`). The share never ends the call.
    async fn screen_ended(&mut self, generation: u64, ended: screen::Ended) -> Result<(), Closed> {
        if self
            .screen
            .as_ref()
            .is_none_or(|screen| screen.generation != generation)
        {
            self.screen_gate.capture_ended(generation);
            self.screen_end_reason = Some((generation, ended));
            return Ok(());
        }
        self.stop_screen(true).await;
        let state = match ended {
            screen::Ended::Source => ScreenState::Off,
            screen::Ended::Failed => ScreenState::Unavailable,
        };
        self.output
            .send(FromWorker::Screen {
                state,
                share: self.screen_share,
            })
            .await
    }

    /// A screen start. While an open that a stop cancelled is still in
    /// flight, the start waits for it (`screen_next`): it never makes that
    /// open, of another source, current again, and its `Starting` comes
    /// after the stop's state.
    fn screen_on(&mut self, target: ScreenTarget, share: u32) -> Vec<FromWorker> {
        // The remote that came with this command, taken now so it cannot go
        // to another one. A Windows monitor has none.
        let grant = match target {
            ScreenTarget::Portal { .. } => screen::Grant::take(),
            ScreenTarget::Monitor { .. } => screen::Grant::default(),
        };
        match self
            .screen_next
            .admit(&self.screen_gate, target, grant, share)
        {
            Some((target, grant, share)) => self.screen_start(target, grant, share),
            None => Vec::new(),
        }
    }

    /// The commands of a screen start: `Starting` first, then what the open
    /// says at once (an `On` that already ran, or a refusal). From here on
    /// the states of the share that runs or opens carry `share`: a repeated
    /// start of the same share takes the new number.
    fn screen_start(
        &mut self,
        target: ScreenTarget,
        grant: screen::Grant,
        share: u32,
    ) -> Vec<FromWorker> {
        self.screen_share = share;
        let mut reply = vec![FromWorker::Screen {
            state: ScreenState::Starting,
            share,
        }];
        // Never waits for the open: the loop keeps serving keys and Leave.
        reply.extend(
            self.begin_screen(target, grant)
                .map(|state| FromWorker::Screen { state, share }),
        );
        reply
    }

    /// A stop of the share (`SetScreen { target: None }`) numbered `share`.
    async fn screen_off(&mut self, share: u32) -> Vec<FromWorker> {
        // A pending open is not wanted any more. Its completion releases the
        // capture and reports the state then. A start that waited for it is
        // dropped with its grant.
        self.screen_next.clear();
        self.screen_gate.request_off();
        if self.screen_gate.opening() {
            return Vec::new();
        }
        self.stop_screen(true).await;
        let held = self.screen_stalled_running() || screen::captures_running();
        vec![FromWorker::Screen {
            state: if held {
                ScreenState::Unavailable
            } else {
                ScreenState::Off
            },
            share,
        }]
    }

    /// Starts reading the pictures of a track into a tile and says so.
    async fn start_reader(
        &mut self,
        tile: u8,
        connection: u8,
        identity: &str,
        track: RtcVideoTrack,
    ) -> Result<(), Closed> {
        // `VideoStarted` is queued before the reader exists, so the core
        // reads it before any picture of this stream.
        self.output
            .send(FromWorker::VideoStarted {
                tile,
                connection,
                identity: identity.to_owned(),
            })
            .await?;
        let stream = NativeVideoStream::new(track);
        let pictures = self.output.pictures();
        self.remote.readers[usize::from(tile)] =
            Some(tokio::spawn(read_pictures(stream, tile, pictures)));
        Ok(())
    }

    /// Frees the tiles of every track that `matches` and gives them on.
    async fn remove_tracks(&mut self, matches: impl Fn(&TrackKey) -> bool) -> Result<(), Closed> {
        self.remote.waiting.retain(|key, _| !matches(key));
        let screens = self.remote.screens.remove_where(&matches);
        for removed in &screens.removed {
            // A member that still shares another track stays marked.
            let identity = &removed.0;
            if !self
                .remote
                .screens
                .any(|key| key.0 == *identity && key.2 == removed.2)
            {
                self.output
                    .send(FromWorker::ScreenSharer {
                        connection: removed.2,
                        identity: identity.clone(),
                        sharing: false,
                    })
                    .await?;
            }
        }
        if let Some(freed) = screens.freed {
            if let Some(reader) = self.remote.readers[usize::from(freed.tile)].take() {
                reader.abort();
                let _ = reader.await;
            }
            self.output.clear_picture(freed.tile);
            self.output
                .send(FromWorker::VideoStopped { tile: freed.tile })
                .await?;
            if let Some(key) = freed.promoted
                && let Some(track) = self.remote.waiting.remove(&key)
            {
                self.start_reader(freed.tile, key.2, &key.0, track).await?;
            }
        }
        for freed in self.remote.tiles.remove_where(&matches) {
            if let Some(reader) = self.remote.readers[usize::from(freed.tile)].take() {
                reader.abort();
                let _ = reader.await;
            }
            // The reader has ended, so no newer picture can arrive.
            self.output.clear_picture(freed.tile);
            self.output
                .send(FromWorker::VideoStopped { tile: freed.tile })
                .await?;
            if let Some(key) = freed.promoted
                && let Some(track) = self.remote.waiting.remove(&key)
            {
                self.start_reader(freed.tile, key.2, &key.0, track).await?;
            }
        }
        Ok(())
    }

    /// Starts the connection to another SFU (ADR-0054). The connect runs on
    /// its own task, so the loop keeps serving the core; its result comes back
    /// through a bounded channel. An index that is not 1 to 3, or one used
    /// before, is a protocol failure of the core.
    fn begin_other(
        &mut self,
        index: u8,
        url: String,
        jwt: Zeroizing<String>,
    ) -> Result<Vec<FromWorker>, FailureCode> {
        let options = self.other_options.clone();
        let done = self.connect_tx.clone();
        let task = tokio::spawn(async move {
            let result = Room::connect(&url, &jwt, options).await.map_err(|_| ());
            // The token is not needed after the connection attempt.
            drop(jwt);
            let _ = done.send(Connected { index, result }).await;
        });
        match self.others.begin(index, task) {
            Ok(()) => Ok(Vec::new()),
            Err(task) => {
                task.abort();
                Err(FailureCode::Protocol)
            }
        }
    }

    /// A connect task ended. The room only subscribes: nothing is published
    /// on it. A failure is reported and never ends the call.
    async fn other_connected(
        &mut self,
        done: Connected,
        events: &mut OtherEvents,
    ) -> Result<(), Closed> {
        let Connected { index, result } = done;
        if !self.others.is_connecting(index) {
            // Not a connect this call waits for: close what came.
            if let Ok((room, _)) = result {
                let _ = room.close().await;
            }
            return Ok(());
        }
        match result {
            Ok((room, receiver)) => {
                self.output
                    .send(FromWorker::Connection {
                        connection: index,
                        up: true,
                    })
                    .await?;
                for (identity, participant) in room
                    .remote_participants()
                    .into_iter()
                    .take(MAX_PARTICIPANTS)
                    .map(|(identity, participant)| (identity.to_string(), participant))
                    .filter(|(identity, _)| valid_identity(identity))
                {
                    self.output
                        .send(FromWorker::ParticipantJoined {
                            connection: index,
                            identity: identity.clone(),
                        })
                        .await?;
                    // No microphone counts as muted (ADR-0059).
                    self.send_muted(index, identity, !microphone_on(&participant))
                        .await?;
                }
                events.insert(index, receiver);
                self.others.up(index, room);
                // Tracks subscribed during the connect get the playout now;
                // later ones when their subscription is handled.
                self.apply_playout();
                Ok(())
            }
            Err(()) => {
                self.others.end(index);
                self.output
                    .send(FromWorker::Connection {
                        connection: index,
                        up: false,
                    })
                    .await
            }
        }
    }

    /// Every subscribed remote audio track, on every connection.
    fn remote_audio(&self) -> Vec<RemoteAudioTrack> {
        let rooms = std::iter::once(self.room).chain(self.others.open());
        let mut tracks = Vec::new();
        for participant in rooms.flat_map(|room| room.remote_participants().into_values()) {
            for publication in participant.track_publications().values() {
                if let Some(RemoteTrack::Audio(track)) = publication.track() {
                    tracks.push(track);
                }
            }
        }
        tracks
    }

    /// Every subscribed remote audio track with its connection and its
    /// participant's identity, at most [`MAX_MEASURED_TRACKS`].
    fn remote_audio_by_participant(&self) -> Vec<(u8, String, RemoteAudioTrack)> {
        let rooms = std::iter::once((0, self.room)).chain(self.others.open_indexed());
        let mut tracks = Vec::new();
        for (connection, room) in rooms {
            for (identity, participant) in room.remote_participants() {
                for publication in participant.track_publications().values() {
                    if tracks.len() >= MAX_MEASURED_TRACKS {
                        return tracks;
                    }
                    if let Some(RemoteTrack::Audio(track)) = publication.track() {
                        tracks.push((connection, identity.to_string(), track));
                    }
                }
            }
        }
        tracks
    }

    /// Starts a measurement of the own and the received sound, unless one
    /// runs.
    fn measure_sound(&mut self) {
        if self.sound.is_busy() {
            return;
        }
        let own = match self.publication.track() {
            Some(LocalTrack::Audio(track)) => Some(track),
            _ => None,
        };
        let remote = self.remote_audio_by_participant();
        let deadline = tokio::time::Instant::now() + SOUND_READ_BUDGET;
        self.sound
            .start(measure_levels(own, remote, deadline), SOUND_MEASURE_TIMEOUT);
    }

    /// Tells the core whether a remote participant's microphone is muted.
    async fn send_muted(
        &self,
        connection: u8,
        identity: String,
        muted: bool,
    ) -> Result<(), Closed> {
        if !valid_identity(&identity) {
            return Ok(());
        }
        self.output
            .send(FromWorker::ParticipantMuted {
                connection,
                identity,
                muted,
            })
            .await
    }

    /// Gives every subscribed remote audio track the call's playout.
    fn apply_playout(&self) {
        for track in self.remote_audio() {
            play_remote_audio(&track, self.playout);
        }
    }

    /// The room of another SFU ended or its queue closed: its tracks go and
    /// its room closes. The connection is not reopened (ADR-0054).
    async fn other_lost(&mut self, index: u8, events: &mut OtherEvents) -> Result<(), Closed> {
        events.remove(index);
        self.remove_tracks(|key| key.2 == index).await?;
        if let Some(Slot::Up(room)) = self.others.end(index) {
            let _ = room.close().await;
        }
        Ok(())
    }

    /// Ends every connection to another SFU: a connect still running stops, a
    /// finished one that nobody took closes, and each open room closes.
    async fn close_others(&mut self, inbox: &mut OtherInbox) {
        inbox.connected.close();
        while let Ok(done) = inbox.connected.try_recv() {
            if let Ok((room, _)) = done.result {
                let _ = room.close().await;
            }
        }
        for slot in self.others.take_all() {
            match slot {
                Slot::Connecting(task) => task.abort(),
                Slot::Up(room) => {
                    let _ = room.close().await;
                }
                Slot::Ended => {}
            }
        }
    }

    /// Follows the remote tracks of any connection: audio gets the call's
    /// playout, video a tile.
    async fn on_event(&mut self, connection: u8, event: &RoomEvent) -> Result<(), Closed> {
        match event {
            RoomEvent::TrackSubscribed {
                track: RemoteTrack::Audio(track),
                publication,
                participant,
            } => {
                play_remote_audio(track, self.playout);
                if publication.source() != TrackSource::Microphone {
                    return Ok(());
                }
                self.send_muted(
                    connection,
                    participant.identity().to_string(),
                    publication.is_muted(),
                )
                .await
            }
            RoomEvent::TrackUnsubscribed {
                track: RemoteTrack::Audio(_),
                publication,
                participant,
            } if publication.source() == TrackSource::Microphone => {
                // No microphone counts as muted.
                self.send_muted(connection, participant.identity().to_string(), true)
                    .await
            }
            RoomEvent::TrackMuted {
                participant: Participant::Remote(participant),
                publication,
            } if publication.kind() == TrackKind::Audio
                && publication.source() == TrackSource::Microphone =>
            {
                self.send_muted(connection, participant.identity().to_string(), true)
                    .await
            }
            RoomEvent::TrackUnmuted {
                participant: Participant::Remote(participant),
                publication,
            } if publication.kind() == TrackKind::Audio
                && publication.source() == TrackSource::Microphone =>
            {
                self.send_muted(connection, participant.identity().to_string(), false)
                    .await
            }
            RoomEvent::TrackSubscribed {
                track: RemoteTrack::Video(track),
                publication,
                participant,
            } if publication.source() == TrackSource::Screenshare => {
                // Only the sender's source says it is a screen share, and
                // that is only a display hint.
                let identity = participant.identity().to_string();
                if !valid_identity(&identity) {
                    return Ok(());
                }
                let key = (identity, publication.sid().to_string(), connection);
                match self.remote.screens.add(key.clone()) {
                    Added::Tile(tile) => {
                        self.output
                            .send(FromWorker::ScreenSharer {
                                connection,
                                identity: key.0.clone(),
                                sharing: true,
                            })
                            .await?;
                        self.start_reader(tile, connection, &key.0, track.rtc_track())
                            .await
                    }
                    Added::Queued => {
                        self.output
                            .send(FromWorker::ScreenSharer {
                                connection,
                                identity: key.0.clone(),
                                sharing: true,
                            })
                            .await?;
                        self.remote.waiting.insert(key, track.rtc_track());
                        Ok(())
                    }
                    Added::Refused => Ok(()),
                }
            }
            RoomEvent::TrackSubscribed {
                track: RemoteTrack::Video(track),
                publication,
                participant,
            } => {
                let identity = participant.identity().to_string();
                if !valid_identity(&identity) {
                    return Ok(());
                }
                let key = (identity, publication.sid().to_string(), connection);
                match self.remote.tiles.add(key.clone()) {
                    Added::Tile(tile) => {
                        self.start_reader(tile, connection, &key.0, track.rtc_track())
                            .await
                    }
                    Added::Queued => {
                        self.remote.waiting.insert(key, track.rtc_track());
                        Ok(())
                    }
                    Added::Refused => Ok(()),
                }
            }
            RoomEvent::TrackUnsubscribed {
                track: RemoteTrack::Video(_),
                publication,
                participant,
            } => {
                let key = (
                    participant.identity().to_string(),
                    publication.sid().to_string(),
                    connection,
                );
                self.remove_tracks(|known| *known == key).await
            }
            RoomEvent::ParticipantDisconnected(participant) => {
                let identity = participant.identity().to_string();
                self.remove_tracks(|known| known.0 == identity && known.2 == connection)
                    .await
            }
            _ => Ok(()),
        }
    }

    /// Handles one command after `Join`. A second `Join` or a `ListDevices` is
    /// a protocol failure.
    async fn handle_command(&mut self, message: ToWorker) -> Result<Vec<FromWorker>, FailureCode> {
        match message {
            ToWorker::SetKey { identity, mut key } => {
                // A refused key (index out of the ring) leaves the old key; the
                // sender's audio then stays silent. It does not end the call.
                if valid_identity(&identity) {
                    set_key(
                        self.provider,
                        &ParticipantIdentity::from(identity),
                        &mut key,
                    );
                }
                Ok(Vec::new())
            }
            ToWorker::SetOwnKey { mut key } => {
                let index = i32::from(key.index);
                // Switch only after the key is stored, so no frame goes out
                // under an index whose ring slot still holds an older key.
                if set_key(self.provider, self.identity, &mut key) {
                    self.gate.set_own_index(index);
                    self.screen_gate.set_own_index(index);
                    use_own_key_index(self.room.e2ee_manager(), &self.own_sids, index);
                }
                Ok(Vec::new())
            }
            // The key provider has no way to remove one participant's keys.
            ToWorker::RemoveParticipant { .. } => Ok(Vec::new()),
            ToWorker::SetMuted { muted } => {
                if muted {
                    self.publication.mute();
                } else {
                    self.publication.unmute();
                }
                Ok(vec![FromWorker::Muted { muted }])
            }
            ToWorker::SetDeafened { deafened } => {
                self.playout.deafened = deafened;
                self.apply_playout();
                Ok(vec![FromWorker::Deafened { deafened }])
            }
            ToWorker::SetVolume { percent } => {
                self.playout = Playout {
                    volume: percent,
                    ..self.playout
                };
                self.apply_playout();
                Ok(Vec::new())
            }
            ToWorker::SetDiagnostics { on } => {
                crate::diagnostics::global().set_on(on);
                Ok(Vec::new())
            }
            ToWorker::SetCamera { on: true, remote } => {
                // The remote that came with this command, taken now so it
                // cannot go to another one: the camera channel gives them
                // out in the order the core sent them.
                let grant = if remote {
                    camera::Grant::take()
                } else {
                    camera::Grant::default()
                };
                // Never waits for the open: the loop keeps serving keys and Leave.
                Ok(self
                    .begin_camera(grant)
                    .map(|state| FromWorker::Camera { state })
                    .into_iter()
                    .collect())
            }
            ToWorker::SetCamera { on: false, .. } => {
                // A pending open is not wanted any more. Its completion
                // releases the capture and reports the state then, so `Off`
                // never comes before the open ended.
                self.gate.request_off();
                if self.gate.opening() {
                    return Ok(Vec::new());
                }
                let mut reply = Vec::new();
                let stop = self.stop_camera(true).await;
                if stop != CameraStop::WasOff {
                    reply.push(FromWorker::VideoStopped { tile: OWN_TILE });
                }
                // Off only when no capture still holds the camera: neither a
                // stalled one nor the thread of an open that timed out.
                let held = stop == CameraStop::Stalled
                    || self.stalled_running()
                    || camera::captures_running();
                reply.push(FromWorker::Camera {
                    state: if held {
                        CameraState::Unavailable
                    } else {
                        CameraState::Off
                    },
                });
                Ok(reply)
            }
            ToWorker::SetScreen {
                target: Some(target),
                share,
            } => Ok(self.screen_on(target, share)),
            ToWorker::SetScreen {
                target: None,
                share,
            } => Ok(self.screen_off(share).await),
            ToWorker::Subscribe { index, url, jwt } => self.begin_other(index, url, jwt),
            // A camera choice applies from the next camera start. A sound
            // change that fails keeps the call on the devices it can.
            ToWorker::SetDevices {
                recording,
                playout,
                camera,
            } => {
                self.camera_device = camera;
                let failure = if sound_unchanged_here(&self.joined_sound, &recording, &playout) {
                    None
                } else {
                    switch_devices(
                        self.audio,
                        &self.media_defaults,
                        &mut self.sound_devices,
                        recording.as_deref(),
                        playout.as_deref(),
                    )
                    .err()
                };
                Ok(vec![FromWorker::DevicesSet { failure }])
            }
            ToWorker::Join { .. }
            | ToWorker::ListDevices
            | ToWorker::TestDevice { .. }
            | ToWorker::Leave => Err(FailureCode::Protocol),
        }
    }
}

/// The result of one camera or screen open.
struct Opened<T, E> {
    generation: u64,
    result: Result<T, E>,
}

/// An open camera that is not yet published.
struct OpenedCamera {
    capture: camera::Capture,
    source: NativeVideoSource,
    live: Arc<FrameAdmission>,
}

/// Runs `open` on its own thread and sends the result to `tx`. The thread is
/// not a runtime blocking task, so a long open (a permission dialog) never
/// delays the end of the process. When the call is over, `tx` has no receiver
/// and the result is dropped on that thread, which releases the camera. The
/// caller keeps at most one open in flight, so the send never waits.
fn spawn_open<T: Send + 'static, E: Send + 'static>(
    tx: mpsc::Sender<Opened<T, E>>,
    generation: u64,
    on_panic: E,
    open: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("capture-open".to_owned())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(open))
                .unwrap_or(Err(on_panic));
            let _ = tx.blocking_send(Opened { generation, result });
        })
        .map(drop)
}

/// The result of turning the own camera off.
#[derive(Debug, PartialEq, Eq)]
enum CameraStop {
    /// The camera was not on.
    WasOff,
    /// The capture ended and the camera is released.
    Released,
    /// The capture did not end in time. It is kept and supervised.
    Stalled,
}

/// What a `SetCamera { on: true }` does.
#[derive(Debug, PartialEq, Eq)]
enum OnRequest {
    /// The camera runs: report `On`.
    AlreadyOn,
    /// An open is in flight: report nothing now; it reports when it ends.
    Pending,
    /// Start an open for this generation.
    Start(u64),
}

/// What a finished open does.
#[derive(Debug, PartialEq, Eq)]
enum Completion {
    /// Publish the track and report `On` (or `Unavailable` if that fails).
    Publish,
    /// Report the open's error state.
    Report,
    /// A stale open: release a camera that opened, report nothing.
    Release,
    /// The camera was turned off while this open was in flight: release a
    /// camera that opened, then report the off that waited for it.
    Cancelled,
    /// The capture ended before the completion: release it and report
    /// `Unavailable`.
    Ended,
}

/// The I/O-free rules of one camera open: one open at a time, a stale result
/// is dropped, and the own key index is the latest at completion.
///
/// The camera is "wanted" from `request_on` until `request_off`. A second
/// `request_on` during an open does not start a second open of the same
/// device. If the camera was turned off meanwhile (a stale open), it makes
/// that one open current again.
struct CameraGate {
    /// Generation of the last open started.
    generation: u64,
    /// Generation of the open in flight, if any.
    in_flight: Option<u64>,
    wanted: bool,
    /// The open in flight whose capture already ended by itself.
    ended_early: Option<u64>,
    own_index: i32,
}

impl CameraGate {
    fn new(own_index: i32) -> Self {
        Self {
            generation: 0,
            in_flight: None,
            wanted: false,
            ended_early: None,
            own_index,
        }
    }

    fn request_on(&mut self, camera_running: bool) -> OnRequest {
        if camera_running {
            return OnRequest::AlreadyOn;
        }
        self.wanted = true;
        if self.in_flight.is_some() {
            return OnRequest::Pending;
        }
        self.generation += 1;
        self.in_flight = Some(self.generation);
        self.ended_early = None;
        OnRequest::Start(self.generation)
    }

    /// Whether an open is in flight.
    fn opening(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Whether the device is wanted: from `request_on` until `request_off`.
    fn wanted(&self) -> bool {
        self.wanted
    }

    /// The camera is not wanted. An open in flight stays and ends cancelled.
    fn request_off(&mut self) {
        self.wanted = false;
    }

    /// An open for `generation` ended; `opened` tells whether it succeeded.
    fn complete(&mut self, generation: u64, opened: bool) -> Completion {
        if self.in_flight != Some(generation) {
            return Completion::Release;
        }
        self.in_flight = None;
        let ended = self.ended_early.take() == Some(generation);
        if !self.wanted {
            Completion::Cancelled
        } else if opened && ended {
            self.wanted = false;
            Completion::Ended
        } else if opened {
            Completion::Publish
        } else {
            self.wanted = false;
            Completion::Report
        }
    }

    /// The capture of `generation` ended by itself. Only the open in flight
    /// is remembered; any other generation is not affected.
    fn capture_ended(&mut self, generation: u64) {
        if self.in_flight == Some(generation) {
            self.ended_early = Some(generation);
        }
    }

    /// The publish of a current open failed.
    fn failed_to_publish(&mut self) {
        self.wanted = false;
    }

    fn set_own_index(&mut self, index: i32) {
        self.own_index = index;
    }

    /// The key index every own track encrypts with.
    fn own_index(&self) -> i32 {
        self.own_index
    }
}

/// Stops a capture off the runtime threads: the join blocks.
/// Returns the capture when it did not end in time.
async fn stop_capture(capture: camera::Capture) -> Option<camera::Capture> {
    tokio::task::spawn_blocking(move || capture.stop())
        .await
        .unwrap_or(None)
}

#[allow(clippy::too_many_arguments)] // the loop waits on every source of events
async fn event_loop(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<RoomEvent>,
    input: &mut Receiver<Input>,
    output: &Output,
    call: &mut Call<'_>,
    camera_failed: &mut Receiver<u64>,
    open_done: &mut Receiver<Opened<OpenedCamera, CameraState>>,
    screens: &mut ScreenInbox,
    others: &mut OtherInbox,
    tracker: &mut SpeakingTracker,
    sound_done: &mut Receiver<Option<Levels>>,
) -> End {
    let mut tick = interval(SPEAKING_TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let mut outgoing = Vec::new();
        tokio::select! {
            event = events.recv() => match event {
                // A decryption failure of one participant does not end the call.
                Some(event) => {
                    note_room_event(0, &event);
                    if call.on_event(0, &event).await.is_err() {
                        return End::Failed(FailureCode::Internal);
                    }
                    if let Some(change) = room_change(0, &event) {
                        outgoing = apply(change, tracker);
                    }
                }
                None => return End::Failed(FailureCode::Network),
            },
            (index, item) = others.events.next() => {
                // A failure of another SFU never ends the call.
                match item {
                    Some(event) => {
                        note_room_event(index, &event);
                        if call.on_event(index, &event).await.is_err() {
                            return End::Failed(FailureCode::Internal);
                        }
                        if let Some(change) = room_change(index, &event) {
                            outgoing = apply(change, tracker);
                        }
                        if matches!(event, RoomEvent::Disconnected { .. })
                            && call.other_lost(index, &mut others.events).await.is_err()
                        {
                            return End::Failed(FailureCode::Internal);
                        }
                    }
                    None => {
                        outgoing = apply(RoomChange::Disconnected(index), tracker);
                        if call.other_lost(index, &mut others.events).await.is_err() {
                            return End::Failed(FailureCode::Internal);
                        }
                    }
                }
            }
            Some(done) = others.connected.recv() => {
                if call.other_connected(done, &mut others.events).await.is_err() {
                    return End::Failed(FailureCode::Internal);
                }
            }
            message = input.recv() => match message {
                Some(Input::Message(ToWorker::Leave)) | Some(Input::Eof) | None => {
                    return End::Left;
                }
                Some(Input::Message(message)) => {
                    match call.handle_command(message).await {
                        Ok(reply) => outgoing.extend(reply),
                        Err(code) => return End::Failed(code),
                    }
                }
                Some(Input::Bad) => return End::Failed(FailureCode::Protocol),
            },
            Some(generation) = camera_failed.recv() => {
                if call.camera_ended(generation).await.is_err() {
                    return End::Failed(FailureCode::Internal);
                }
            }
            Some(done) = open_done.recv() => {
                if call.camera_opened(done).await.is_err() {
                    return End::Failed(FailureCode::Internal);
                }
            }
            (generation, ended) = screens.ended.next() => {
                if call.screen_ended(generation, ended).await.is_err() {
                    return End::Failed(FailureCode::Internal);
                }
            }
            Some(done) = screens.opened.recv() => {
                if call.screen_opened(done).await.is_err() {
                    return End::Failed(FailureCode::Internal);
                }
            }
            Some(levels) = sound_done.recv() => {
                outgoing.extend(call.sound.end(levels));
            }
            _ = tick.tick() => call.measure_sound(),
        }
        outgoing.extend(tracker.poll(Instant::now().into_std()));
        let disconnected = outgoing.iter().any(|message| {
            matches!(
                message,
                FromWorker::Failed {
                    code: FailureCode::Network
                }
            )
        });
        for message in outgoing {
            if matches!(message, FromWorker::Failed { .. }) {
                continue;
            }
            if output.send(message).await.is_err() {
                return End::Failed(FailureCode::Internal);
            }
        }
        if disconnected {
            return End::Failed(FailureCode::Network);
        }
    }
}

/// Hands the key to LiveKit. The bytes are moved out of the zeroizing
/// wrapper, so no second copy is made here.
fn set_key(provider: &KeyProvider, identity: &ParticipantIdentity, key: &mut FrameKeyMsg) -> bool {
    let bytes = std::mem::take(&mut *key.bytes);
    provider.set_key(identity, i32::from(key.index), bytes)
}

/// Makes the frame cryptor of each own track encrypt with the key at `index`.
///
/// Storing a key in the provider does not change the index that a sender's
/// frame cryptor encrypts with: native libwebrtc keeps that index apart, at
/// 0 until it is set. Without this call every frame goes out under the first
/// own key, a member that joins later (and holds only a newer key) hears
/// silence, and a rotation on leave does not take effect. Every own track
/// (microphone and camera) needs it.
fn use_own_key_index(manager: &E2eeManager, own_sids: &[TrackSid], index: i32) {
    for ((_, track), cryptor) in manager.frame_cryptors() {
        if own_sids.contains(&track) {
            cryptor.set_key_index(index);
        }
    }
}

/// A camera picture as I420, scaled to fit 640x360 (no enlarging).
fn fitted_i420(frame: &CapturedFrame<'_>) -> Option<I420Buffer> {
    let (width, height, stride) = (frame.width, frame.height, frame.stride);
    let (fit_w, fit_h) = fit_within_box(width, height)?;
    let scaled = (fit_w, fit_h) != (width, height);
    let picture = match frame.format {
        Format::Nv12 => {
            // NV12 needs even sides.
            if width % 2 != 0 || height % 2 != 0 {
                return None;
            }
            let mut buffer = NV12Buffer::with_strides(width, height, stride, stride);
            let (y_dst, uv_dst) = buffer.data_mut();
            let y_len = y_dst.len();
            if frame.data.len() < y_len + uv_dst.len() {
                return None;
            }
            y_dst.copy_from_slice(&frame.data[..y_len]);
            let uv_len = uv_dst.len();
            uv_dst.copy_from_slice(&frame.data[y_len..y_len + uv_len]);
            let buffer = if scaled {
                buffer.scale(fit_w as i32, fit_h as i32)
            } else {
                buffer
            };
            VideoFrameBufferExt::to_i420(&buffer)
        }
        Format::I420 => {
            let chroma_stride = stride.div_ceil(2);
            let mut buffer =
                I420Buffer::with_strides(width, height, stride, chroma_stride, chroma_stride);
            let (y_dst, u_dst, v_dst) = buffer.data_mut();
            let (y_len, c_len) = (y_dst.len(), u_dst.len());
            if v_dst.len() != c_len || frame.data.len() < y_len + 2 * c_len {
                return None;
            }
            y_dst.copy_from_slice(&frame.data[..y_len]);
            u_dst.copy_from_slice(&frame.data[y_len..y_len + c_len]);
            v_dst.copy_from_slice(&frame.data[y_len + c_len..y_len + 2 * c_len]);
            if scaled {
                buffer.scale(fit_w as i32, fit_h as i32)
            } else {
                buffer
            }
        }
    };
    Some(picture)
}

/// A screen picture as I420, scaled to fit 1920x1080 (no enlarging). The
/// alpha or padding byte is ignored. libyuv names its formats by the order of
/// a little-endian 32-bit word: B, G, R, A in memory is its "ARGB", and R, G,
/// B, A in memory is its "ABGR".
fn screen_i420(frame: &screen::CapturedFrame<'_>) -> Option<I420Buffer> {
    let (width, height) = (frame.width, frame.height);
    // The size is checked again here, before any buffer is made.
    if !screen::capture_size_ok(width, height) {
        return None;
    }
    let needed = screen::CapturedFrame::expected_len(width, height, frame.stride)?;
    if frame.data.len() < needed {
        return None;
    }
    let (fit_w, fit_h) = fit_publish_box(width, height)?;
    let mut buffer = I420Buffer::new(width, height);
    let (stride_y, stride_u, stride_v) = buffer.strides();
    let (y, u, v) = buffer.data_mut();
    let convert = match frame.format {
        screen::Format::Bgra => yuv_helper::argb_to_i420,
        screen::Format::Rgba => yuv_helper::abgr_to_i420,
    };
    convert(
        frame.data,
        frame.stride,
        y,
        stride_y,
        u,
        stride_u,
        v,
        stride_v,
        width as i32,
        height as i32,
    );
    Some(if (fit_w, fit_h) == (width, height) {
        buffer
    } else {
        buffer.scale(fit_w as i32, fit_h as i32)
    })
}

/// An I420 picture as RGBA in memory order R, G, B, A.
///
/// libyuv names its formats by the order of a little-endian 32-bit word, so
/// libyuv's "ABGR" is the byte order R, G, B, A in memory. That is
/// `VideoFormatType::ABGR` here (see the test below). `VideoFormatType::RGBA`
/// would give A, B, G, R in memory.
fn to_rgba(tile: u8, picture: &I420Buffer) -> Option<(u32, u32, Vec<u8>)> {
    let (width, height) = (picture.width(), picture.height());
    // The size is checked against the tile's limits before the buffer exists.
    let mut rgba = vec![0_u8; rgba_len(tile, width, height)?];
    VideoFrameBufferExt::to_argb(
        picture,
        VideoFormatType::ABGR,
        &mut rgba,
        width * 4,
        width as i32,
        height as i32,
    );
    Some((width, height, rgba))
}

/// Any buffer a decoder may give, as a new I420 picture.
fn buffer_to_i420(buffer: &dyn VideoBuffer) -> Option<I420Buffer> {
    if let Some(buffer) = buffer.as_i420() {
        return Some(VideoFrameBufferExt::to_i420(buffer));
    }
    if let Some(buffer) = buffer.as_nv12() {
        return Some(VideoFrameBufferExt::to_i420(buffer));
    }
    if let Some(buffer) = buffer.as_i422() {
        return Some(VideoFrameBufferExt::to_i420(buffer));
    }
    if let Some(buffer) = buffer.as_i444() {
        return Some(VideoFrameBufferExt::to_i420(buffer));
    }
    if let Some(buffer) = buffer.as_i010() {
        return Some(VideoFrameBufferExt::to_i420(buffer));
    }
    if let Some(buffer) = buffer.as_i420a() {
        return Some(VideoFrameBufferExt::to_i420(buffer));
    }
    buffer.as_native().map(VideoFrameBufferExt::to_i420)
}

/// Reads the pictures of one remote track, at most 15 per second (5 for the
/// screen tile), and puts the newest into the tile's slot. It ends when the
/// track ends or when it is aborted; it holds no state that needs a clean-up.
async fn read_pictures(mut stream: NativeVideoStream, tile: u8, pictures: PictureSink) {
    let mut limiter = RateLimiter::for_tile(tile);
    while let Some(frame) = stream.next().await {
        if !limiter.admit(std::time::Instant::now()) {
            continue;
        }
        let Some(mut picture) = buffer_to_i420(&*frame.buffer) else {
            continue;
        };
        let Some((fit_w, fit_h)) = fit_for_tile(tile, picture.width(), picture.height()) else {
            continue;
        };
        if (fit_w, fit_h) != (picture.width(), picture.height()) {
            picture = picture.scale(fit_w as i32, fit_h as i32);
        }
        let degrees = match frame.rotation {
            VideoRotation::VideoRotation0 => 0,
            VideoRotation::VideoRotation90 => 90,
            VideoRotation::VideoRotation180 => 180,
            VideoRotation::VideoRotation270 => 270,
        };
        let rotation = quarter_turns(degrees).unwrap_or(0);
        if let Some((width, height, rgba)) = to_rgba(tile, &picture) {
            pictures.put(tile, rotation, width, height, rgba);
        }
    }
}

pub(crate) fn start_audio() -> Result<PlatformAudio, FailureCode> {
    let audio = PlatformAudio::new().map_err(|_| FailureCode::AudioDevice)?;
    audio
        .configure_audio_processing(AudioProcessingOptions {
            echo_cancellation: true,
            noise_suppression: true,
            auto_gain_control: true,
            prefer_hardware_processing: false,
        })
        .map_err(|_| FailureCode::AudioDevice)?;
    Ok(audio)
}

async fn publish_microphone(
    room: &Room,
    audio: &PlatformAudio,
) -> Result<LocalTrackPublication, FailureCode> {
    let track = LocalAudioTrack::create_audio_track("microphone", audio.rtc_source());
    let options = TrackPublishOptions {
        source: TrackSource::Microphone,
        ..TrackPublishOptions::default()
    };
    room.local_participant()
        .publish_track(LocalTrack::Audio(track), options)
        .await
        .map_err(|error| match error {
            RoomError::Rtc(_) => FailureCode::AudioDevice,
            other => classify(&other),
        })
}

/// One diagnostic record for a room event that matters for media, while the
/// log is on (ADR-0058): identities, track IDs, kinds and states. Never a
/// payload, a token, a URL or a name. Only an event that is noted takes from
/// the record budget, and its text is built only once it is admitted, so a
/// flood of room events costs a count, not formatting.
fn note_room_event(connection: u8, event: &RoomEvent) {
    let diagnostics = crate::diagnostics::global();
    if !is_noted(event) || !diagnostics.admit(crate::diagnostics::Source::Note) {
        return;
    }
    if let Some((level, text)) = describe_room_event(connection, event) {
        diagnostics.push_admitted(
            crate::diagnostics::Source::Note,
            level,
            "persephone::call_worker::room",
            &text,
        );
    }
}

/// Whether [`describe_room_event`] makes a record for `event`, without
/// building it.
fn is_noted(event: &RoomEvent) -> bool {
    matches!(
        event,
        RoomEvent::Connected { .. }
            | RoomEvent::ConnectionStateChanged(_)
            | RoomEvent::Reconnecting
            | RoomEvent::Reconnected
            | RoomEvent::Disconnected { .. }
            | RoomEvent::ParticipantConnected(_)
            | RoomEvent::ParticipantDisconnected(_)
            | RoomEvent::TrackPublished { .. }
            | RoomEvent::TrackUnpublished { .. }
            | RoomEvent::TrackSubscribed { .. }
            | RoomEvent::TrackUnsubscribed { .. }
            | RoomEvent::TrackSubscriptionFailed { .. }
            | RoomEvent::TrackMuted { .. }
            | RoomEvent::TrackUnmuted { .. }
            | RoomEvent::LocalTrackPublished { .. }
            | RoomEvent::LocalTrackUnpublished { .. }
            | RoomEvent::E2eeStateChanged { .. }
            | RoomEvent::ParticipantEncryptionStatusChanged { .. }
            | RoomEvent::ConnectionQualityChanged { .. }
            | RoomEvent::TokenRefreshed { .. }
    )
}

/// The level and text of the record for `event` on `connection`, or `None`
/// for an event that does not matter for media. The connection comes
/// first, so the own room's tracks and another SFU's stay apart.
fn describe_room_event(connection: u8, event: &RoomEvent) -> Option<(u8, String)> {
    describe_room_event_text(event).map(|(level, text)| {
        (
            level,
            bounded_text(format_args!("connection {connection}: {text}")),
        )
    })
}

/// Text formatted within the protocol's message limit: identities and IDs
/// come from the SFU, so their text is never built whole.
fn bounded_text(args: std::fmt::Arguments<'_>) -> String {
    crate::diagnostics::format_bounded(args, persephone_call_protocol::MAX_DIAGNOSTIC_MESSAGE_BYTES)
}

fn describe_room_event_text(event: &RoomEvent) -> Option<(u8, String)> {
    use crate::diagnostics::{DEBUG, INFO, WARN};
    macro_rules! text {
        ($($arg:tt)*) => {
            bounded_text(format_args!($($arg)*))
        };
    }
    let track = |publication: &RemoteTrackPublication| {
        text!(
            "{} {:?} {:?}",
            publication.sid(),
            publication.kind(),
            publication.source()
        )
    };
    Some(match event {
        RoomEvent::Connected {
            participants_with_tracks,
        } => (
            INFO,
            text!("connected, {} participants", participants_with_tracks.len()),
        ),
        RoomEvent::ConnectionStateChanged(state) => (INFO, text!("connection state {state:?}")),
        RoomEvent::Reconnecting => (WARN, "reconnecting".to_owned()),
        RoomEvent::Reconnected => (INFO, "reconnected".to_owned()),
        RoomEvent::Disconnected { reason } => (WARN, text!("disconnected: {reason:?}")),
        RoomEvent::ParticipantConnected(participant) => (
            INFO,
            text!("participant connected {}", participant.identity()),
        ),
        RoomEvent::ParticipantDisconnected(participant) => (
            INFO,
            text!("participant disconnected {}", participant.identity()),
        ),
        RoomEvent::TrackPublished {
            publication,
            participant,
        } => (
            INFO,
            text!(
                "track published {} by {}",
                track(publication),
                participant.identity()
            ),
        ),
        RoomEvent::TrackUnpublished {
            publication,
            participant,
        } => (
            INFO,
            text!(
                "track unpublished {} by {}",
                track(publication),
                participant.identity()
            ),
        ),
        RoomEvent::TrackSubscribed {
            publication,
            participant,
            ..
        } => (
            INFO,
            text!(
                "track subscribed {} from {}",
                track(publication),
                participant.identity()
            ),
        ),
        RoomEvent::TrackUnsubscribed {
            publication,
            participant,
            ..
        } => (
            INFO,
            text!(
                "track unsubscribed {} from {}",
                track(publication),
                participant.identity()
            ),
        ),
        RoomEvent::TrackSubscriptionFailed {
            participant,
            error,
            track_sid,
        } => (
            WARN,
            text!(
                "track subscription failed {track_sid} from {}: {error:?}",
                participant.identity()
            ),
        ),
        RoomEvent::TrackMuted {
            participant,
            publication,
        } => (
            DEBUG,
            text!(
                "track muted {} by {}",
                publication.sid(),
                participant.identity()
            ),
        ),
        RoomEvent::TrackUnmuted {
            participant,
            publication,
        } => (
            DEBUG,
            text!(
                "track unmuted {} by {}",
                publication.sid(),
                participant.identity()
            ),
        ),
        RoomEvent::LocalTrackPublished { publication, .. } => (
            INFO,
            text!(
                "own track published {} {:?} {:?}",
                publication.sid(),
                publication.kind(),
                publication.source()
            ),
        ),
        RoomEvent::LocalTrackUnpublished { publication, .. } => {
            (INFO, text!("own track unpublished {}", publication.sid()))
        }
        RoomEvent::E2eeStateChanged { participant, state } => (
            INFO,
            text!("frame encryption {state:?} for {}", participant.identity()),
        ),
        RoomEvent::ParticipantEncryptionStatusChanged {
            participant,
            is_encrypted,
        } => (
            INFO,
            text!(
                "participant {} encrypted: {is_encrypted}",
                participant.identity()
            ),
        ),
        RoomEvent::ConnectionQualityChanged {
            quality,
            participant,
        } => (
            DEBUG,
            text!(
                "connection quality {quality:?} for {}",
                participant.identity()
            ),
        ),
        // The new token itself is never written.
        RoomEvent::TokenRefreshed { .. } => (DEBUG, "token refreshed".to_owned()),
        _ => return None,
    })
}

/// Whether a remote participant has a microphone that is not muted. A
/// participant that publishes none (listening only, or sharing a screen
/// only) does not.
fn microphone_on(participant: &RemoteParticipant) -> bool {
    participant
        .track_publications()
        .values()
        .any(|publication| {
            publication.source() == TrackSource::Microphone && !publication.is_muted()
        })
}

/// Reduces a room event to what the core is told. Everything else is dropped.
fn room_change(connection: u8, event: &RoomEvent) -> Option<RoomChange> {
    match event {
        RoomEvent::ParticipantConnected(participant) => Some(RoomChange::Joined(
            connection,
            participant.identity().to_string(),
            microphone_on(participant),
        )),
        RoomEvent::ParticipantDisconnected(participant) => Some(RoomChange::Left(
            connection,
            participant.identity().to_string(),
        )),
        RoomEvent::ActiveSpeakersChanged { speakers } => Some(RoomChange::Speakers(
            connection,
            speakers
                .iter()
                .map(|speaker| speaker.identity().to_string())
                .collect(),
        )),
        RoomEvent::Disconnected { .. } => Some(RoomChange::Disconnected(connection)),
        _ => None,
    }
}

/// Maps a LiveKit error to a closed failure code. The text is read only to
/// tell a TLS refusal from another transport error; it is never sent.
fn classify(error: &RoomError) -> FailureCode {
    let (fault, text) = match error {
        RoomError::Engine(EngineError::Signal(signal)) => match signal {
            SignalError::Client(..) | SignalError::Server(..) | SignalError::Handshake { .. } => {
                (ConnectFault::Refused, String::new())
            }
            SignalError::Connection(text) => (ConnectFault::Transport, text.clone()),
            SignalError::Timeout(_) | SignalError::Closed | SignalError::SendError => {
                (ConnectFault::Timeout, String::new())
            }
            _ => (ConnectFault::Other, String::new()),
        },
        RoomError::Engine(EngineError::Connection(text)) => {
            (ConnectFault::Transport, text.to_string())
        }
        _ => (ConnectFault::Other, String::new()),
    };
    failure_for_connect(fault, &text)
}

async fn fail(output: &Output, code: FailureCode) -> u8 {
    let _ = output.send(FromWorker::Failed { code }).await;
    EXIT_FAILED
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// GIVEN the devices a call uses and a new choice WHEN a change during
    /// the call is planned THEN an equal choice changes nothing, a new one
    /// switches, and a "System default" that names no listed device cannot
    /// move the call off a device it opened by ID.
    #[test]
    fn given_a_change_during_a_call_then_only_a_reachable_choice_switches() {
        let devices = |recording: Option<&str>, playout: Option<&str>| SoundDevices {
            recording: recording.map(str::to_owned),
            playout: playout.map(str::to_owned),
            ..SoundDevices::default()
        };
        let current = devices(Some("mic-1"), Some("spk-1"));
        assert_eq!(device_switch(&current, &current), Ok(None));
        let next = devices(Some("mic-2"), Some("spk-1"));
        assert_eq!(device_switch(&current, &next), Ok(Some(next.clone())));
        assert_eq!(
            device_switch(&current, &devices(None, Some("spk-1"))),
            Err(FailureCode::Unsupported)
        );
        assert_eq!(
            device_switch(&current, &devices(Some("mic-1"), None)),
            Err(FailureCode::Unsupported)
        );
        // A call on libwebrtc's own defaults stays there for a default choice.
        let unresolved = devices(None, None);
        assert_eq!(device_switch(&unresolved, &unresolved), Ok(None));
        assert_eq!(
            device_switch(&unresolved, &devices(Some("mic-2"), None)),
            Ok(Some(devices(Some("mic-2"), None)))
        );
    }

    /// GIVEN the join's sound choice WHEN `SetDevices` carries it again (a
    /// camera change only) or another one THEN only on Linux, where no
    /// device changes during a call, the same choice is no sound change;
    /// elsewhere every choice goes to the switch.
    #[test]
    fn given_the_joined_sound_choice_then_only_linux_skips_the_switch() {
        let joined = (Some("mic-1".to_owned()), None);
        let same = sound_unchanged_here(&joined, &Some("mic-1".to_owned()), &None);
        assert_eq!(same, cfg!(target_os = "linux"));
        assert!(!sound_unchanged_here(
            &joined,
            &Some("mic-2".to_owned()),
            &None
        ));
        assert!(!sound_unchanged_here(
            &joined,
            &Some("mic-1".to_owned()),
            &Some("spk".to_owned())
        ));
    }

    /// A sound device fake that keeps LiveKit 0.11's capture state: the
    /// IDs that switch, whether a restart starts capture, whether capture
    /// runs, and every call in order. A failed microphone switch stops
    /// capture; a switch while it is stopped only selects.
    #[derive(Default)]
    struct FakeAudio {
        mics: Vec<&'static str>,
        speakers: Vec<&'static str>,
        restarts: bool,
        capturing: bool,
        calls: Vec<String>,
    }

    impl SwitchAudio for FakeAudio {
        fn switch_recording(&mut self, id: &str) -> bool {
            self.calls.push(format!("mic {id}"));
            let opened = self.mics.contains(&id);
            self.capturing &= opened;
            opened
        }

        fn switch_playout(&mut self, id: &str) -> bool {
            self.calls.push(format!("speaker {id}"));
            self.speakers.contains(&id)
        }

        fn restart_recording(&mut self) -> bool {
            self.calls.push("restart".to_owned());
            self.capturing |= self.restarts;
            self.capturing
        }
    }

    /// GIVEN a change of both devices WHEN each switch succeeds or fails
    /// THEN `current` names the devices the call uses, a failed microphone
    /// switch starts the old microphone again, a failed speaker is lost,
    /// and a lost device is switched again by the next change, even to the
    /// same choice. A lost microphone counts as back only when capture
    /// runs; a lost speaker stays lost.
    #[test]
    fn given_a_device_switch_then_current_names_the_devices_in_use() {
        let devices = |recording: Option<&str>, playout: Option<&str>| SoundDevices {
            recording: recording.map(str::to_owned),
            playout: playout.map(str::to_owned),
            ..SoundDevices::default()
        };
        let next = devices(Some("mic-2"), Some("spk-2"));
        let from = devices(Some("mic-1"), Some("spk-1"));
        let run = |start: &SoundDevices, audio: &mut FakeAudio| {
            let mut current = start.clone();
            let result = apply_switch(&mut current, next.clone(), audio);
            (result, current)
        };
        let fake = |mics: &[&'static str], speakers: &[&'static str], restarts| FakeAudio {
            mics: mics.to_vec(),
            speakers: speakers.to_vec(),
            restarts,
            capturing: true,
            calls: Vec::new(),
        };
        let failed = Err(FailureCode::AudioDevice);

        // Both switch.
        let mut audio = fake(&["mic-2"], &["spk-2"], true);
        assert_eq!(run(&from, &mut audio), (Ok(()), next.clone()));
        assert_eq!(audio.calls, ["mic mic-2", "restart", "speaker spk-2"]);
        assert!(audio.capturing);

        // The new microphone fails: the old one is switched back and started.
        let mut audio = fake(&["mic-1"], &["spk-2"], true);
        assert_eq!(run(&from, &mut audio), (failed, from.clone()));
        assert_eq!(audio.calls, ["mic mic-2", "mic mic-1", "restart"]);

        // ... and when nothing starts again, the microphone is lost.
        let mut audio = fake(&[], &["spk-2"], false);
        let (result, current) = run(&from, &mut audio);
        assert_eq!(result, failed);
        assert!(current.recording_lost && !current.playout_lost);

        // The speaker fails: both go back to the old devices, and the
        // speaker is lost, as its playout stopped.
        let mut audio = fake(&["mic-1", "mic-2"], &["spk-1"], true);
        let mut lost_speaker = from.clone();
        lost_speaker.playout_lost = true;
        assert_eq!(run(&from, &mut audio), (failed, lost_speaker));
        assert_eq!(
            audio.calls,
            [
                "mic mic-2",
                "restart",
                "speaker spk-2",
                "speaker spk-1",
                "mic mic-1",
                "restart"
            ]
        );

        // The speaker cannot go back: it is lost; the microphone went back.
        let mut audio = fake(&["mic-1", "mic-2"], &[], true);
        let (result, current) = run(&from, &mut audio);
        assert_eq!(result, failed);
        assert!(current.playout_lost && !current.recording_lost);
        assert_eq!(current.recording.as_deref(), Some("mic-1"));

        // The microphone cannot go back: `current` names the new one, and
        // capture is started on what is selected.
        let mut audio = fake(&["mic-2"], &["spk-1"], true);
        let (result, current) = run(&from, &mut audio);
        assert_eq!(result, failed);
        assert_eq!(current.recording.as_deref(), Some("mic-2"));
        assert!(current.recording_lost);
        assert_eq!(audio.calls.last().map(String::as_str), Some("restart"));

        // No old microphone ID: the new microphone stays.
        let mut audio = fake(&["mic-2"], &["spk-1"], true);
        let (result, current) = run(&devices(None, Some("spk-1")), &mut audio);
        assert_eq!(result, failed);
        assert_eq!(current.recording.as_deref(), Some("mic-2"));

        // A lost microphone is switched again, even to the same choice.
        let mut lost = next.clone();
        lost.recording_lost = true;
        let mut audio = fake(&["mic-2"], &["spk-2"], true);
        assert_eq!(run(&lost, &mut audio), (Ok(()), next.clone()));
        assert_eq!(audio.calls, ["mic mic-2", "restart"]);

        // A lost microphone that the switch only selects is started; when
        // capture does not start, it stays lost.
        let mut audio = fake(&["mic-2"], &["spk-2"], true);
        audio.capturing = false;
        assert_eq!(run(&lost, &mut audio), (Ok(()), next.clone()));
        assert!(audio.capturing);
        let mut audio = fake(&["mic-2"], &["spk-2"], false);
        audio.capturing = false;
        let (result, current) = run(&lost, &mut audio);
        assert_eq!(result, failed);
        assert!(current.recording_lost && !audio.capturing);

        // A lost speaker is only selected: it stays lost, and the
        // microphone that moved is kept.
        let mut lost = devices(Some("mic-1"), Some("spk-2"));
        lost.playout_lost = true;
        let mut audio = fake(&["mic-2"], &["spk-2"], true);
        let (result, current) = run(&lost, &mut audio);
        assert_eq!(result, failed);
        assert_eq!(audio.calls, ["mic mic-2", "restart", "speaker spk-2"]);
        assert_eq!(
            (current.recording.as_deref(), current.playout.as_deref()),
            (Some("mic-2"), Some("spk-2"))
        );
        assert!(current.playout_lost && !current.recording_lost);
        assert_eq!(
            device_switch(&lost, &next),
            Ok(Some(next.clone())),
            "a lost device is not the same as the choice"
        );

        // A lost microphone or speaker chosen with no ID: the microphone
        // counts as back only when capture starts; the speaker stays lost.
        let mut lost_default = devices(None, Some("spk-1"));
        lost_default.recording_lost = true;
        for (restarts, back) in [(false, false), (true, true)] {
            let mut audio = fake(&[], &["spk-1"], restarts);
            audio.capturing = false;
            let mut current = lost_default.clone();
            let result = apply_switch(&mut current, devices(None, Some("spk-1")), &mut audio);
            assert_eq!(result.is_ok(), back);
            assert_eq!(current.recording_lost, !back);
            assert_eq!(audio.capturing, back);
            assert_eq!(audio.calls, ["restart"]);
        }
        let mut lost_speaker = devices(Some("mic-1"), None);
        lost_speaker.playout_lost = true;
        let mut audio = fake(&["mic-1"], &[], true);
        let mut current = lost_speaker.clone();
        let result = apply_switch(&mut current, devices(Some("mic-1"), None), &mut audio);
        assert_eq!(result, failed);
        assert!(current.playout_lost, "no ID cannot start playout");
        assert!(audio.calls.is_empty());
    }

    /// GIVEN a Windows machine with speakers or microphones WHEN the worker
    /// reads the media defaults THEN each is a device that libwebrtc lists,
    /// so "System default" opens it and not the default communications
    /// device. A machine with no device of a kind checks nothing for it.
    #[cfg(windows)]
    #[test]
    fn given_windows_devices_then_the_media_defaults_are_listed_ones() {
        let Ok(audio) = PlatformAudio::new() else {
            return;
        };
        let playout: Vec<String> = audio
            .playout_devices()
            .map(|device| device.id.as_str().to_owned())
            .collect();
        let recording: Vec<String> = audio
            .recording_devices()
            .map(|device| device.id.as_str().to_owned())
            .collect();
        drop(audio);
        let (speaker, microphone) = sound_defaults::media_defaults();
        if !playout.is_empty() {
            let speaker = speaker.expect("a default speaker");
            assert!(playout.contains(&speaker), "{speaker} in {playout:?}");
        }
        if !recording.is_empty() {
            let microphone = microphone.expect("a default microphone");
            assert!(
                recording.contains(&microphone),
                "{microphone} in {recording:?}"
            );
        }
    }

    /// GIVEN a sound measurement that finishes, one that times out and one
    /// that unwinds THEN each reports exactly once: the level, then `None`
    /// twice. So a slow or failed measurement never shows as silence and
    /// never keeps the slot.
    #[tokio::test]
    async fn given_measurement_endings_then_each_reports_once() {
        let (tx, mut rx) = mpsc::channel::<Option<Levels>>(1);
        drop(SoundReport {
            done: tx.clone(),
            levels: Some(own(0.5)),
        });
        assert_eq!(rx.recv().await, Some(Some(own(0.5))));

        let done = tx.clone();
        tokio::spawn(async move {
            let mut report = SoundReport { done, levels: None };
            report.levels =
                tokio::time::timeout(Duration::from_millis(1), std::future::pending::<Levels>())
                    .await
                    .ok();
        })
        .await
        .expect("the timed-out measurement ends");
        assert_eq!(rx.recv().await, Some(None));

        let done = tx.clone();
        let unwound = tokio::spawn(async move {
            let _report = SoundReport { done, levels: None };
            panic!("statistics read failed");
        });
        assert!(unwound.await.is_err());
        assert_eq!(rx.recv().await, Some(None));
        assert!(rx.try_recv().is_err(), "one report each");
    }

    /// GIVEN a sound probe WHEN measurements start and end THEN at most one
    /// runs; each report frees the slot; a level goes out only when it
    /// changes; and a timed-out or unwound measurement frees the slot
    /// without a message.
    #[tokio::test]
    async fn given_sound_measurements_then_one_runs_and_every_end_frees_it() {
        let (tx, mut rx) = mpsc::channel::<Option<Levels>>(1);
        let mut probe = SoundProbe::new(tx);
        let long = Duration::from_secs(5);

        assert!(probe.start(async { own(0.5) }, long));
        assert!(!probe.start(async { own(1.0) }, long), "one runs at a time");
        let report = rx.recv().await.expect("a report");
        assert_eq!(report, Some(own(0.5)));
        assert!(probe.is_busy(), "busy until the report is taken");
        let first = probe.end(report);
        assert!(matches!(first.as_slice(), [FromWorker::SoundLevel { level }] if *level > 0));
        assert!(!probe.is_busy());

        assert!(probe.start(async { own(0.5) }, long));
        let same = rx.recv().await.expect("a report");
        assert!(probe.end(same).is_empty(), "an unchanged level is not sent");

        assert!(probe.start(std::future::pending::<Levels>(), Duration::from_millis(1)));
        let timed_out = rx.recv().await.expect("a report");
        assert_eq!(timed_out, None);
        assert!(probe.end(timed_out).is_empty(), "a timeout is not silence");
        assert!(!probe.is_busy());

        assert!(probe.start(async { panic!("statistics read failed") }, long));
        let unwound = rx.recv().await.expect("a report");
        assert_eq!(unwound, None);
        assert!(probe.end(unwound).is_empty());
        assert!(
            probe.start(async { own(0.0) }, long),
            "the slot is free again"
        );
        let silent = rx.recv().await.expect("a report");
        assert_eq!(probe.end(silent), [FromWorker::SoundLevel { level: 0 }]);
    }

    fn own(linear: f64) -> Levels {
        Levels {
            own: Some(linear),
            remote: Vec::new(),
        }
    }

    /// GIVEN a statistics read and a deadline WHEN the read finishes in
    /// time, does not, or its deadline has passed THEN it gives its output,
    /// `None` at the deadline, and `None` without starting, so one slow
    /// read never holds back the own level.
    #[tokio::test]
    async fn given_a_read_deadline_then_a_late_read_counts_as_unread() {
        let soon = tokio::time::Instant::now() + Duration::from_secs(5);
        assert_eq!(read_by(soon, async { 7 }).await, Some(7));
        let short = tokio::time::Instant::now() + Duration::from_millis(5);
        assert_eq!(read_by(short, std::future::pending::<u8>()).await, None);
        let passed = tokio::time::Instant::now();
        let never = std::future::poll_fn(|_| -> std::task::Poll<u8> {
            panic!("a read after its deadline is not started")
        });
        assert_eq!(read_by(passed, never).await, None);
    }

    /// GIVEN a slow read before two quick ones WHEN they are read under one
    /// deadline THEN the quick ones are still read, in order, and only the
    /// slow one is `None`.
    #[tokio::test]
    async fn given_a_slow_read_then_the_other_reads_still_finish() {
        type Read = std::pin::Pin<Box<dyn std::future::Future<Output = u8> + Send>>;
        let reads: Vec<Read> = vec![
            Box::pin(std::future::pending::<u8>()),
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                2
            }),
            Box::pin(async { 3 }),
        ];
        let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
        assert_eq!(read_all(deadline, reads).await, [None, Some(2), Some(3)]);
    }

    /// GIVEN test input WHEN it is read THEN the diagnostic switch goes on,
    /// `Leave` and a closed input stop, and any other message is refused.
    #[test]
    fn given_device_test_input_then_only_leave_and_the_switch_are_served() {
        assert!(matches!(
            test_input(Some(Input::Message(ToWorker::SetDiagnostics { on: false }))),
            TestInput::Continue
        ));
        assert!(matches!(
            test_input(Some(Input::Message(ToWorker::Leave))),
            TestInput::Stop
        ));
        assert!(matches!(test_input(Some(Input::Eof)), TestInput::Stop));
        assert!(matches!(test_input(None), TestInput::Stop));
        assert!(matches!(
            test_input(Some(Input::Message(ToWorker::SetMuted { muted: true }))),
            TestInput::Bad
        ));
        assert!(matches!(test_input(Some(Input::Bad)), TestInput::Bad));
    }

    /// GIVEN room events that need no participant WHEN described THEN each
    /// names its connection, its state or reason and its level, and a token
    /// refresh never holds the token.
    #[test]
    fn given_room_events_when_described_then_records_name_connection_and_state() {
        use crate::diagnostics::{INFO, WARN};
        let cases = [
            (RoomEvent::Reconnecting, WARN, "connection 0: reconnecting"),
            (RoomEvent::Reconnected, INFO, "connection 0: reconnected"),
            (
                RoomEvent::ConnectionStateChanged(livekit::ConnectionState::Connected),
                INFO,
                "connection 0: connection state Connected",
            ),
            (
                RoomEvent::Disconnected {
                    reason: livekit::DisconnectReason::ServerShutdown,
                },
                WARN,
                "connection 0: disconnected: ServerShutdown",
            ),
            (
                RoomEvent::Connected {
                    participants_with_tracks: Vec::new(),
                },
                INFO,
                "connection 0: connected, 0 participants",
            ),
        ];
        for (event, level, text) in cases {
            assert!(is_noted(&event), "{text}");
            assert_eq!(
                describe_room_event(0, &event),
                Some((level, text.to_owned()))
            );
        }
        let other = describe_room_event(2, &RoomEvent::Reconnecting).unwrap().1;
        assert_eq!(other, "connection 2: reconnecting");
        let refreshed = RoomEvent::TokenRefreshed {
            token: "eyJsecret.payload.sig".into(),
        };
        assert!(is_noted(&refreshed));
        let (_, text) = describe_room_event(1, &refreshed).unwrap();
        assert_eq!(text, "connection 1: token refreshed");
    }

    /// GIVEN a room event that does not matter for media WHEN described THEN
    /// no record is made, so its metadata text never reaches the log.
    #[test]
    fn given_an_uninteresting_room_event_when_described_then_none() {
        let event = RoomEvent::RoomMetadataChanged {
            old_metadata: "old".into(),
            metadata: "new".into(),
        };
        assert!(!is_noted(&event), "it must take no budget");
        assert_eq!(describe_room_event(0, &event), None);
    }

    fn solid_i420(width: u32, height: u32, y: u8, u: u8, v: u8) -> I420Buffer {
        let mut buffer = I420Buffer::new(width, height);
        let (y_plane, u_plane, v_plane) = buffer.data_mut();
        y_plane.fill(y);
        u_plane.fill(u);
        v_plane.fill(v);
        buffer
    }

    /// A solid screen picture of 4-byte pixels `pixel`.
    fn solid_screen(width: u32, height: u32, pixel: [u8; 4]) -> Vec<u8> {
        pixel.repeat(width as usize * height as usize)
    }

    /// GIVEN a red screen picture in B, G, R, A and in R, G, B, A order
    /// WHEN it is made I420 THEN both give the same red (BT.601 Y 81) and the
    /// alpha byte does not matter.
    #[test]
    fn given_red_screen_in_both_byte_orders_when_made_i420_then_the_colour_matches() {
        for (format, pixel) in [
            (screen::Format::Bgra, [0, 0, 255, 0]),
            (screen::Format::Bgra, [0, 0, 255, 255]),
            (screen::Format::Rgba, [255, 0, 0, 0]),
        ] {
            let data = solid_screen(16, 8, pixel);
            let frame = screen::CapturedFrame::checked(format, 16, 8, 64, &data).expect("frame");
            let mut picture = screen_i420(&frame).expect("picture");
            assert_eq!((picture.width(), picture.height()), (16, 8));
            let (y, u, v) = picture.data_mut();
            assert!(
                y.iter().all(|&value| (79..=83).contains(&value)),
                "{pixel:?}"
            );
            assert!(
                u.iter().all(|&value| (88..=92).contains(&value)),
                "{pixel:?}"
            );
            assert!(
                v.iter().all(|&value| (238..=242).contains(&value)),
                "{pixel:?}"
            );
        }
    }

    /// GIVEN screens above, at and below 1920x1080 WHEN made I420 THEN the
    /// result fits 1920x1080 with the aspect, and a small one is not
    /// enlarged.
    #[test]
    fn given_screens_when_made_i420_then_they_fit_1920_by_1080() {
        for ((width, height), want) in [
            ((3840, 2160), (1920, 1080)),
            ((2560, 1440), (1920, 1080)),
            ((1920, 1080), (1920, 1080)),
            ((800, 600), (800, 600)),
            ((1080, 1920), (1080, 1920)),
        ] {
            let data = solid_screen(width, height, [10, 20, 30, 255]);
            let frame = screen::CapturedFrame::checked(
                screen::Format::Bgra,
                width,
                height,
                width * 4,
                &data,
            )
            .expect("frame");
            let picture = screen_i420(&frame).expect("picture");
            assert_eq!(
                (picture.width(), picture.height()),
                want,
                "{width}x{height}"
            );
        }
    }

    /// GIVEN a screen frame whose size is outside the capture limits or whose
    /// data is short WHEN it is made I420 THEN there is no picture.
    #[test]
    fn given_bad_screen_frames_when_made_i420_then_there_is_no_picture() {
        let data = vec![0_u8; 64];
        let oversize = screen::CapturedFrame {
            format: screen::Format::Bgra,
            width: 7681,
            height: 2,
            stride: 7681 * 4,
            data: &data,
        };
        assert!(screen_i420(&oversize).is_none());
        let short = screen::CapturedFrame {
            format: screen::Format::Bgra,
            width: 8,
            height: 8,
            stride: 32,
            data: &data,
        };
        assert!(screen_i420(&short).is_none());
    }

    /// GIVEN a screen open that succeeded WHEN it completes THEN it is
    /// published and reports `On`; a failed open reports its own state; a
    /// stop during the open reports `Off`; a source that ended during the
    /// open is not published.
    #[test]
    fn given_screen_open_outcomes_then_the_gate_decides_what_is_reported() {
        let mut gate = CameraGate::new(0);
        let OnRequest::Start(first) = gate.request_on(false) else {
            panic!("start");
        };
        assert_eq!(gate.request_on(false), OnRequest::Pending);
        assert_eq!(gate.complete(first, true), Completion::Publish);
        let mut gate = CameraGate::new(0);
        let OnRequest::Start(first) = gate.request_on(false) else {
            panic!("start");
        };
        assert_eq!(gate.complete(first, false), Completion::Report);
        let OnRequest::Start(second) = gate.request_on(false) else {
            panic!("start");
        };
        gate.request_off();
        assert_eq!(gate.complete(second, true), Completion::Cancelled);
        let OnRequest::Start(third) = gate.request_on(false) else {
            panic!("start");
        };
        gate.capture_ended(third);
        assert_eq!(gate.complete(third, true), Completion::Ended);
    }

    /// GIVEN a screen open in flight WHEN it is stopped THEN the gate still
    /// shows the open and no longer wants the device, which is when a new
    /// start must wait; WHEN started again THEN it is wanted again.
    #[test]
    fn given_a_stopped_open_then_the_gate_is_opening_but_not_wanted() {
        let mut gate = CameraGate::new(0);
        assert!(!gate.wanted());
        let OnRequest::Start(first) = gate.request_on(false) else {
            panic!("start");
        };
        assert!(gate.opening() && gate.wanted());
        gate.request_off();
        assert!(gate.opening() && !gate.wanted());
        assert_eq!(gate.request_on(false), OnRequest::Pending);
        assert!(gate.wanted());
        assert_eq!(gate.complete(first, true), Completion::Publish);
    }

    /// A grant that counts its drops.
    struct Counted(Arc<std::sync::atomic::AtomicUsize>);

    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// GIVEN share A's open in flight WHEN A is stopped and B, then C, start
    /// before A's open ends THEN neither starts beside A's open, B's grant
    /// drops when C replaces it, and once A's open ends cancelled the start
    /// that runs is C's, with C's own target, grant and share number. A stop
    /// drops a waiting start with its grant. With no open in flight a start
    /// runs at once.
    #[test]
    fn given_a_start_during_a_stopped_open_then_it_waits_with_its_own_grant() {
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let grant = || Counted(drops.clone());
        let portal = |node| ScreenTarget::Portal { node };
        let mut gate = CameraGate::new(0);
        let mut next = NextStart::default();

        let first = next.admit(&gate, portal(1), grant(), 1).expect("A runs");
        let OnRequest::Start(open) = gate.request_on(false) else {
            panic!("start");
        };
        drop(first);
        gate.request_off();
        assert!(
            next.admit(&gate, portal(2), grant(), 2).is_none(),
            "B waits"
        );
        assert!(
            next.admit(&gate, portal(3), grant(), 3).is_none(),
            "C waits"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 2, "A's and B's grants");
        assert_eq!(gate.complete(open, true), Completion::Cancelled);
        let (target, waiting, share) = next.take().expect("C runs now");
        assert_eq!((target, share), (portal(3), 3));
        assert_eq!(drops.load(Ordering::SeqCst), 2, "C keeps its own grant");
        drop(waiting);
        assert!(next.take().is_none());

        let OnRequest::Start(_) = gate.request_on(false) else {
            panic!("start");
        };
        gate.request_off();
        assert!(next.admit(&gate, portal(4), grant(), 4).is_none());
        next.clear();
        assert_eq!(drops.load(Ordering::SeqCst), 4, "the stop dropped it");
        assert!(next.take().is_none());
    }

    /// GIVEN capture ends of several generations WHEN they are reported out
    /// of order THEN the slot keeps only the newest; an older end reported
    /// after it does not replace it, and a taken slot is empty.
    #[test]
    fn given_capture_ends_out_of_order_then_the_newest_is_kept() {
        let ends = ScreenEnd::default();
        assert_eq!(ends.take(), None);
        ends.report(2, screen::Ended::Failed);
        ends.report(1, screen::Ended::Source);
        assert_eq!(ends.take(), Some((2, screen::Ended::Failed)));
        assert_eq!(ends.take(), None);
        ends.report(3, screen::Ended::Source);
        ends.report(4, screen::Ended::Failed);
        assert_eq!(ends.take(), Some((4, screen::Ended::Failed)));
    }

    /// GIVEN an end reported before anyone waits THEN the wait returns it at
    /// once; GIVEN a wait in progress WHEN a capture thread reports an end
    /// THEN the wait wakes with it. No end is lost and the slot stays one.
    #[tokio::test]
    async fn given_a_capture_end_then_the_wait_gets_it_whenever_it_came() {
        let ends = Arc::new(ScreenEnd::default());
        ends.report(1, screen::Ended::Source);
        assert_eq!(ends.next().await, (1, screen::Ended::Source));

        let reporter = ends.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            reporter.report(2, screen::Ended::Failed);
        });
        let end = tokio::time::timeout(std::time::Duration::from_secs(5), ends.next())
            .await
            .expect("the wait wakes");
        assert_eq!(end, (2, screen::Ended::Failed));
        thread.join().unwrap();
    }

    /// GIVEN a red I420 picture WHEN it is made RGBA THEN the bytes in memory
    /// are R, G, B, A (libyuv "ABGR"), with full alpha.
    #[test]
    fn given_red_picture_when_made_rgba_then_bytes_are_r_g_b_a() {
        // BT.601 limited range red: Y 81, U 90, V 240.
        let (width, height, rgba) = to_rgba(0, &solid_i420(4, 2, 81, 90, 240)).expect("size ok");
        assert_eq!((width, height), (4, 2));
        assert_eq!(rgba.len(), 4 * 2 * 4);
        for pixel in rgba.chunks(4) {
            assert!(pixel[0] > 230, "red byte first: {pixel:?}");
            assert!(pixel[1] < 25 && pixel[2] < 25, "{pixel:?}");
            assert_eq!(pixel[3], 255);
        }
        // Blue: Y 41, U 240, V 110 puts the big byte third.
        let (_, _, rgba) = to_rgba(0, &solid_i420(2, 2, 41, 240, 110)).expect("size ok");
        assert!(rgba[2] > 200 && rgba[0] < 40, "{:?}", &rgba[..4]);
    }

    /// GIVEN a picture outside the wire limits WHEN it is made RGBA THEN there
    /// is no picture.
    #[test]
    fn given_oversized_picture_when_made_rgba_then_it_is_refused() {
        assert!(to_rgba(0, &solid_i420(641, 2, 0, 128, 128)).is_none());
    }

    fn nv12_data(height: u32, stride: u32) -> Vec<u8> {
        let len = stride as usize * height as usize * 3 / 2;
        (0..len).map(|i| (i % 200) as u8).collect()
    }

    /// GIVEN camera frames of several sizes WHEN made I420 THEN they fit 640x360
    /// and a small one is not enlarged.
    #[test]
    fn given_camera_frames_when_made_i420_then_they_fit_the_box() {
        for ((width, height), want) in [
            ((1280, 720), (640, 360)),
            ((1920, 1080), (640, 360)),
            ((640, 360), (640, 360)),
            ((320, 240), (320, 240)),
            ((640, 480), (480, 360)),
        ] {
            let data = nv12_data(height, width);
            let frame = CapturedFrame::checked(Format::Nv12, width, height, width, &data)
                .expect("frame fits");
            let picture = fitted_i420(&frame).expect("converted");
            assert_eq!(
                (picture.width(), picture.height()),
                want,
                "{width}x{height}"
            );
        }
    }

    /// GIVEN a frame with a stride wider than the picture WHEN converted THEN the
    /// padding is skipped and the size is the picture's.
    #[test]
    fn given_padded_frame_when_converted_then_size_is_the_picture_size() {
        let data = nv12_data(32, 80);
        let frame = CapturedFrame::checked(Format::Nv12, 64, 32, 80, &data).expect("fits");
        let picture = fitted_i420(&frame).expect("converted");
        assert_eq!((picture.width(), picture.height()), (64, 32));
    }

    /// GIVEN an NV12 frame with an odd side WHEN converted THEN it is refused.
    #[test]
    fn given_odd_nv12_frame_when_converted_then_it_is_refused() {
        let data = nv12_data(34, 66);
        let frame = CapturedFrame::checked(Format::Nv12, 65, 34, 66, &data).expect("fits");
        assert!(fitted_i420(&frame).is_none());
    }

    /// GIVEN an I420 camera frame WHEN converted THEN the size follows the fit
    /// rule.
    #[test]
    fn given_i420_frame_when_converted_then_it_is_fitted() {
        let len = CapturedFrame::expected_len(Format::I420, 1280, 720, 1280).unwrap();
        let data = vec![90_u8; len];
        let frame = CapturedFrame::checked(Format::I420, 1280, 720, 1280, &data).expect("fits");
        let picture = fitted_i420(&frame).expect("converted");
        assert_eq!((picture.width(), picture.height()), (640, 360));
    }

    /// GIVEN a decoder buffer of type I420 WHEN converted for the wire THEN it
    /// is a new I420 picture of the same size.
    #[test]
    fn given_decoder_buffer_when_converted_then_it_keeps_its_size() {
        let source = solid_i420(8, 6, 100, 128, 128);
        let copy = buffer_to_i420(&source).expect("i420 is known");
        assert_eq!((copy.width(), copy.height()), (8, 6));
    }

    // The camera open runs beside the event loop (ADR-0048). The gate tests
    // are pure. The `spawn_open` tests use a stand-in for the camera. No test
    // here drives `Call` or a LiveKit room: that needs a real SFU.

    /// Counts drops, to show a result was released.
    struct Released(Arc<std::sync::atomic::AtomicUsize>);

    impl Drop for Released {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// GIVEN a pending open WHEN SetOwnKey arrives THEN the gate takes the new
    /// index while the open is still pending, and the open still completes
    /// current.
    #[test]
    fn given_pending_open_when_own_key_arrives_then_index_is_taken_at_once() {
        let mut gate = CameraGate::new(1);
        assert_eq!(gate.request_on(false), OnRequest::Start(1));
        gate.set_own_index(2);
        assert_eq!(gate.own_index(), 2);
        assert_eq!(gate.complete(1, true), Completion::Publish);
    }

    /// GIVEN a completion WHEN it is current THEN the index at completion is
    /// the latest, not the one at start.
    #[test]
    fn given_current_completion_when_published_then_latest_own_index_is_used() {
        let mut gate = CameraGate::new(1);
        let OnRequest::Start(generation) = gate.request_on(false) else {
            panic!("the first request starts an open");
        };
        gate.set_own_index(5);
        gate.set_own_index(7);
        assert_eq!(gate.complete(generation, true), Completion::Publish);
        assert_eq!(gate.own_index(), 7);
    }

    /// GIVEN a pending open WHEN the camera is turned off THEN the open stays
    /// in flight (the off waits), and its completion, success or error, is
    /// cancelled: released, then reported as the off.
    #[test]
    fn given_pending_open_when_turned_off_then_completion_is_cancelled() {
        let mut gate = CameraGate::new(0);
        assert_eq!(gate.request_on(false), OnRequest::Start(1));
        gate.request_off();
        assert!(gate.opening(), "the off waits for the open");
        assert_eq!(gate.complete(1, true), Completion::Cancelled);
        assert!(!gate.opening());
        assert_eq!(gate.request_on(false), OnRequest::Start(2));
        gate.request_off();
        assert_eq!(gate.complete(2, false), Completion::Cancelled);
        // A repeated completion of the same open is stale.
        assert_eq!(gate.complete(2, true), Completion::Release);
    }

    /// GIVEN a pending open WHEN another on arrives THEN no second open
    /// starts, and a completion of an unknown generation is released.
    #[test]
    fn given_pending_open_when_on_again_then_no_second_open() {
        let mut gate = CameraGate::new(0);
        assert_eq!(gate.request_on(false), OnRequest::Start(1));
        assert_eq!(gate.request_on(false), OnRequest::Pending);
        assert_eq!(gate.complete(9, true), Completion::Release);
        assert_eq!(gate.complete(1, true), Completion::Publish);
    }

    /// GIVEN an open turned off and on again before it ends WHEN it ends THEN
    /// that one result is published (the device is not opened twice).
    #[test]
    fn given_off_then_on_during_open_then_one_open_serves_both() {
        let mut gate = CameraGate::new(0);
        assert_eq!(gate.request_on(false), OnRequest::Start(1));
        gate.request_off();
        assert_eq!(gate.request_on(false), OnRequest::Pending);
        assert_eq!(gate.complete(1, true), Completion::Publish);
    }

    /// GIVEN a current open WHEN it fails THEN the error is reported, and the
    /// next request starts a new open.
    #[test]
    fn given_current_open_when_it_fails_then_error_is_reported() {
        let mut gate = CameraGate::new(0);
        assert_eq!(gate.request_on(false), OnRequest::Start(1));
        assert_eq!(gate.complete(1, false), Completion::Report);
        assert_eq!(gate.request_on(false), OnRequest::Start(2));
        assert_eq!(gate.request_on(true), OnRequest::AlreadyOn);
    }

    /// GIVEN a capture that ended before its open completed WHEN the open
    /// completes THEN it is released as ended, not published, and the next
    /// request starts a new open.
    #[test]
    fn given_capture_ended_before_completion_then_it_is_not_published() {
        let mut gate = CameraGate::new(0);
        assert_eq!(gate.request_on(false), OnRequest::Start(1));
        gate.capture_ended(1);
        assert_eq!(gate.complete(1, true), Completion::Ended);
        assert_eq!(gate.request_on(false), OnRequest::Start(2));
        assert_eq!(gate.complete(2, true), Completion::Publish);
    }

    /// GIVEN an end of another generation or without an open WHEN the open
    /// completes THEN it is unaffected.
    #[test]
    fn given_unrelated_capture_end_then_the_open_is_unaffected() {
        let mut gate = CameraGate::new(0);
        gate.capture_ended(1);
        assert_eq!(gate.request_on(false), OnRequest::Start(1));
        gate.capture_ended(7);
        assert_eq!(gate.complete(1, true), Completion::Publish);
        // After a publish the old generation is no longer in flight.
        gate.capture_ended(1);
        gate.request_off();
        assert_eq!(gate.request_on(false), OnRequest::Start(2));
        assert_eq!(gate.complete(2, true), Completion::Publish);
    }

    /// GIVEN a capture that ended early and a camera turned off WHEN the open
    /// completes THEN it is cancelled (the off is reported), and a failed open
    /// still reports its own error.
    #[test]
    fn given_ended_early_then_off_or_failed_open_keeps_its_rule() {
        let mut gate = CameraGate::new(0);
        assert_eq!(gate.request_on(false), OnRequest::Start(1));
        gate.capture_ended(1);
        gate.request_off();
        assert_eq!(gate.complete(1, true), Completion::Cancelled);
        assert_eq!(gate.request_on(false), OnRequest::Start(2));
        gate.capture_ended(2);
        assert_eq!(gate.complete(2, false), Completion::Report);
    }

    /// GIVEN a closed admission THEN no frame is admitted; once opened, frames
    /// are admitted until it is closed again.
    #[test]
    fn given_frame_admission_then_frames_pass_only_while_open() {
        let live = FrameAdmission::default();
        assert!(live.admit().is_none(), "closed until the publish");
        live.open();
        assert!(live.admit().is_some());
        live.close();
        assert!(live.admit().is_none());
    }

    /// GIVEN a frame that passed admission WHEN a stop closes the admission
    /// THEN the close waits until that frame is handed on, and no frame is
    /// admitted after it.
    #[test]
    fn given_a_frame_in_progress_when_closed_then_the_close_waits_for_it() {
        let live = Arc::new(FrameAdmission::default());
        live.open();
        let (admitted_tx, admitted) = std::sync::mpsc::channel();
        let (finish, finish_rx) = std::sync::mpsc::channel::<()>();
        let (handed_tx, handed) = std::sync::mpsc::channel();
        let frame = {
            let live = live.clone();
            std::thread::spawn(move || {
                let guard = live.admit().expect("open");
                admitted_tx.send(()).unwrap();
                finish_rx.recv().unwrap();
                handed_tx.send(()).unwrap();
                drop(guard);
            })
        };
        admitted.recv().unwrap();
        let (closed_tx, closed) = std::sync::mpsc::channel();
        let stop = {
            let live = live.clone();
            std::thread::spawn(move || {
                live.close();
                closed_tx.send(()).unwrap();
            })
        };
        assert!(
            closed.recv_timeout(Duration::from_millis(200)).is_err(),
            "the close waits for the frame in progress"
        );
        finish.send(()).unwrap();
        closed.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(handed.try_recv().is_ok(), "the frame was handed on first");
        assert!(live.admit().is_none());
        frame.join().unwrap();
        stop.join().unwrap();
    }

    /// GIVEN a slow open WHEN a command arrives THEN a loop that selects over
    /// both serves the command before the open ends, and the result then
    /// arrives with its generation.
    #[tokio::test]
    async fn given_slow_open_when_command_arrives_then_loop_serves_it_first() {
        let (tx, mut done) = mpsc::channel::<Opened<u8, CameraState>>(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        spawn_open(tx, 3, CameraState::Unavailable, move || {
            let _ = release_rx.recv();
            Ok(42)
        })
        .expect("thread starts");
        let (command_tx, mut commands) = mpsc::channel::<&str>(1);
        command_tx.send("set-own-key").await.expect("queued");
        let mut order = Vec::new();
        while order.len() < 2 {
            tokio::select! {
                Some(command) = commands.recv() => {
                    order.push(command.to_owned());
                    release_tx.send(()).expect("open waits");
                }
                Some(opened) = done.recv() => {
                    assert_eq!((opened.generation, opened.result.ok()), (3, Some(42)));
                    order.push("opened".to_owned());
                }
            }
        }
        assert_eq!(order, ["set-own-key", "opened"]);
    }

    /// GIVEN an open that ends after the call ended WHEN it sends THEN the
    /// result is dropped (released) and nothing waits.
    #[tokio::test]
    async fn given_open_ending_after_call_when_it_sends_then_result_is_released() {
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, done) = mpsc::channel::<Opened<Released, CameraState>>(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let held = Released(drops.clone());
        spawn_open(tx, 1, CameraState::Unavailable, move || {
            let _ = release_rx.recv();
            Ok(held)
        })
        .expect("thread starts");
        // The call ends: the receiver goes away while the open still runs.
        drop(done);
        release_tx.send(()).expect("open waits");
        for _ in 0..200 {
            if drops.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    /// GIVEN an open that panics WHEN it ends THEN the result is Unavailable
    /// and the gate is not stuck.
    #[tokio::test]
    async fn given_open_that_panics_when_it_ends_then_result_is_unavailable() {
        let (tx, mut done) = mpsc::channel::<Opened<u8, CameraState>>(1);
        spawn_open(tx, 1, CameraState::Unavailable, || panic!("backend fault"))
            .expect("thread starts");
        let opened = done.recv().await.expect("a result arrives");
        assert_eq!(opened.result.err(), Some(CameraState::Unavailable));
    }

    /// GIVEN rooms of two other SFUs (1 and 3) and no room at 2
    /// WHEN each sends an event and one queue closes
    /// THEN the events come with their index, the closed queue gives `None`,
    /// and with no room left the wait never ends.
    #[tokio::test]
    async fn given_other_rooms_when_they_send_then_events_carry_their_index() {
        let mut events = OtherEvents::default();
        let (first_tx, first) = mpsc::unbounded_channel();
        let (third_tx, third) = mpsc::unbounded_channel();
        events.insert(1, first);
        events.insert(3, third);
        third_tx.send(RoomEvent::Reconnecting).expect("sent");
        let (index, item) = events.next().await;
        assert_eq!(index, 3);
        assert!(matches!(item, Some(RoomEvent::Reconnecting)));
        first_tx.send(RoomEvent::Reconnected).expect("sent");
        let (index, item) = events.next().await;
        assert_eq!(index, 1);
        assert!(matches!(item, Some(RoomEvent::Reconnected)));
        drop(first_tx);
        let (index, item) = events.next().await;
        assert_eq!((index, item.is_none()), (1, true));
        events.remove(1);
        events.remove(3);
        let waited = tokio::time::timeout(Duration::from_millis(50), events.next()).await;
        assert!(waited.is_err(), "no room, nothing to wait for");
    }

    /// GIVEN two rooms that both have events queued
    /// WHEN the events are taken
    /// THEN neither room is served twice before the other.
    #[tokio::test]
    async fn given_two_busy_other_rooms_then_neither_starves() {
        let mut events = OtherEvents::default();
        let (first_tx, first) = mpsc::unbounded_channel();
        let (second_tx, second) = mpsc::unbounded_channel();
        events.insert(1, first);
        events.insert(2, second);
        for _ in 0..4 {
            first_tx.send(RoomEvent::Reconnecting).expect("sent");
            second_tx.send(RoomEvent::Reconnecting).expect("sent");
        }
        let mut order = Vec::new();
        for _ in 0..4 {
            order.push(events.next().await.0);
        }
        assert_eq!(order, [1, 2, 1, 2]);
    }
}
