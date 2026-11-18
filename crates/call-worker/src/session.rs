//! One call: joins the LiveKit room and runs until the core ends it or the
//! room fails. All LiveKit types stay in this module.

use std::collections::HashMap;
use std::sync::{Arc, RwLock, RwLockReadGuard};
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
        LocalVideoTrack, PlatformAudio, RemoteTrack, TrackSid, TrackSource,
    },
    rtc_engine::EngineError,
    webrtc::{
        peer_connection_factory::{IceServer, IceTransportsType, RtcConfiguration},
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
    CameraState, FailureCode, FrameKeyMsg, FromWorker, MAX_VIDEO_FPS, MAX_VIDEO_PIXELS,
    MAX_VIDEO_SIDE, ToWorker, VIDEO_TILES,
};
use tokio::sync::mpsc::{self, Receiver};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval};
use tokio_stream::StreamExt;

use crate::camera::{self, CapturedFrame, Format};
use crate::containment;
use crate::mapping::{
    ConnectFault, RoomChange, SpeakingTracker, apply, bounded_device_list, choose_device,
    failure_for_connect, valid_identity,
};
use crate::output::{Closed, Output, PictureSink};
use crate::video::{
    Added, OWN_TILE, RateLimiter, TileAssigner, fit_within_box, quarter_turns, rgba_len,
};
use crate::{EXIT_DONE, EXIT_FAILED, Input};

/// Element Call's key ring size (livekit-client keyringSize).
const KEY_RING_SIZE: i32 = 256;
/// Element Call's ratchet window size (livekit-client ratchetWindowSize).
const RATCHET_WINDOW_SIZE: i32 = 10;
/// No tolerance for decryption failures before a key is marked bad (-1).
const FAILURE_TOLERANCE: i32 = -1;
/// How often deferred Speaking changes are looked at.
const SPEAKING_TICK: Duration = Duration::from_millis(100);

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

    let audio = match start_audio() {
        Ok(audio) => audio,
        Err(code) => return fail(output, code).await,
    };
    if let Err(code) = select_devices(
        &audio,
        recording_device.as_deref(),
        playout_device.as_deref(),
    ) {
        return fail(output, code).await;
    }
    // Linux stage 2 (ADR-0049): after the sound server connection and before
    // anything goes to the SFU. A failure ends the call.
    if !containment::apply_stage2() && !cfg!(feature = "dev-uncontained") {
        return fail(output, FailureCode::Containment).await;
    }
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
    for identity in room.remote_participants().keys() {
        let change = RoomChange::Joined(identity.to_string());
        for message in apply(change, &mut tracker) {
            if output.send(message).await.is_err() {
                let _ = room.close().await;
                return EXIT_FAILED;
            }
        }
    }

    let (camera_failed_tx, mut camera_failed) = mpsc::channel::<u64>(1);
    // At most one camera open is in flight, so one slot is enough.
    let (open_tx, mut open_done) = mpsc::channel::<Opened<OpenedCamera>>(1);
    let mut call = Call {
        room: &room,
        provider: &provider,
        identity: &own,
        output,
        own_sids,
        publication: &publication,
        camera_device,
        camera: None,
        stalled: None,
        gate: CameraGate::new(own_index),
        camera_failed: camera_failed_tx,
        open_tx,
        remote: RemoteVideo::default(),
    };
    let mut end = None;
    if camera_on
        && let Some(state) = call.begin_camera()
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
                &mut tracker,
            )
            .await
        }
    };
    call.shutdown(&mut open_done).await;
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

/// Applies the chosen microphone and speaker. `None` keeps the system default.
fn select_devices(
    audio: &PlatformAudio,
    recording: Option<&str>,
    playout: Option<&str>,
) -> Result<(), FailureCode> {
    let recording_devices: Vec<_> = audio.recording_devices().collect();
    let wanted = choose_device(
        recording,
        recording_devices.iter().map(|device| device.id.as_str()),
    )?;
    if let Some(wanted) = wanted {
        let device = recording_devices
            .iter()
            .find(|device| device.id.as_str() == wanted)
            .ok_or(FailureCode::DeviceNotFound)?;
        audio
            .set_recording_device(&device.id)
            .map_err(|_| FailureCode::DeviceNotFound)?;
    }
    let playout_devices: Vec<_> = audio.playout_devices().collect();
    let wanted = choose_device(
        playout,
        playout_devices.iter().map(|device| device.id.as_str()),
    )?;
    if let Some(wanted) = wanted {
        let device = playout_devices
            .iter()
            .find(|device| device.id.as_str() == wanted)
            .ok_or(FailureCode::DeviceNotFound)?;
        audio
            .set_playout_device(&device.id)
            .map_err(|_| FailureCode::DeviceNotFound)?;
    }
    Ok(())
}

/// Everything the call changes after it is connected.
struct Call<'a> {
    room: &'a Room,
    provider: &'a KeyProvider,
    identity: &'a ParticipantIdentity,
    output: &'a Output,
    /// The microphone's publication.
    publication: &'a LocalTrackPublication,
    /// The SIDs of every own track (microphone and camera).
    own_sids: Vec<TrackSid>,
    camera_device: Option<String>,
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
    open_tx: mpsc::Sender<Opened<OpenedCamera>>,
    remote: RemoteVideo,
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
type TrackKey = (String, String);

/// Remote video: which tracks have a tile, which wait, and the tasks that read
/// the pictures of each tile.
#[derive(Default)]
struct RemoteVideo {
    tiles: TileAssigner<TrackKey>,
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
        if let Some((width, height, rgba)) = to_rgba(&picture) {
            self.pictures.put(OWN_TILE, 0, width, height, rgba);
        }
        self.source
            .capture_frame(&VideoFrame::new(VideoRotation::VideoRotation0, picture));
    }
}

impl Call<'_> {
    /// Starts turning the camera on. The open runs on its own thread; the
    /// result arrives in the event loop (`camera_opened`). Returns a state to
    /// report now: `On` when the camera already runs, `Unavailable` when the
    /// open could not start, nothing while an open is in flight.
    fn begin_camera(&mut self) -> Option<CameraState> {
        // A capture that did not stop still holds the camera: open no second
        // one. With no open in flight, any capture thread that still runs (a
        // stalled stop, or an open that timed out) holds a camera too.
        if self.camera.is_none()
            && (self.stalled_running() || (!self.gate.opening() && camera::captures_running()))
        {
            drop_unused_handle();
            return Some(CameraState::Unavailable);
        }
        let generation = match self.gate.request_on(self.camera.is_some()) {
            OnRequest::AlreadyOn => {
                drop_unused_handle();
                return Some(CameraState::On);
            }
            OnRequest::Pending => {
                drop_unused_handle();
                return None;
            }
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
        let started = spawn_open(self.open_tx.clone(), generation, move || {
            let capture = camera::open(
                choice.as_deref(),
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
        });
        if started.is_err() {
            self.gate.complete(generation, false);
            return Some(CameraState::Unavailable);
        }
        None
    }

    /// The open ended. A current result is published. One the camera was
    /// turned off for meanwhile is released, and the deferred off is reported
    /// now. A stale one is released and not reported.
    async fn camera_opened(&mut self, done: Opened<OpenedCamera>) -> Result<(), Closed> {
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
    async fn shutdown(&mut self, open_done: &mut Receiver<Opened<OpenedCamera>>) {
        open_done.close();
        while let Ok(done) = open_done.try_recv() {
            if let Ok(opened) = done.result {
                self.release_capture(opened.capture).await;
            }
        }
        self.stop_camera(false).await;
        // The drop of a stalled capture waits up to the stop timeout.
        if let Some(stalled) = self.stalled.take() {
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

    /// Starts reading the pictures of a track into a tile and says so.
    async fn start_reader(
        &mut self,
        tile: u8,
        identity: &str,
        track: RtcVideoTrack,
    ) -> Result<(), Closed> {
        // `VideoStarted` is queued before the reader exists, so the core
        // reads it before any picture of this stream.
        self.output
            .send(FromWorker::VideoStarted {
                tile,
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
                self.start_reader(freed.tile, &key.0, track).await?;
            }
        }
        Ok(())
    }

    /// Follows the video tracks of remote participants.
    async fn on_event(&mut self, event: &RoomEvent) -> Result<(), Closed> {
        match event {
            RoomEvent::TrackSubscribed {
                track: RemoteTrack::Video(track),
                publication,
                participant,
            } if publication.source() != TrackSource::Screenshare => {
                let identity = participant.identity().to_string();
                if !valid_identity(&identity) {
                    return Ok(());
                }
                let key = (identity, publication.sid().to_string());
                match self.remote.tiles.add(key.clone()) {
                    Added::Tile(tile) => self.start_reader(tile, &key.0, track.rtc_track()).await,
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
                );
                self.remove_tracks(|known| *known == key).await
            }
            RoomEvent::ParticipantDisconnected(participant) => {
                let identity = participant.identity().to_string();
                self.remove_tracks(|known| known.0 == identity).await
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
            ToWorker::SetCamera { on: true } => {
                // Never waits for the open: the loop keeps serving keys and Leave.
                Ok(self
                    .begin_camera()
                    .map(|state| FromWorker::Camera { state })
                    .into_iter()
                    .collect())
            }
            ToWorker::SetCamera { on: false } => {
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
            ToWorker::Join { .. } | ToWorker::ListDevices | ToWorker::Leave => {
                Err(FailureCode::Protocol)
            }
        }
    }
}

/// The result of one camera open.
struct Opened<T> {
    generation: u64,
    result: Result<T, CameraState>,
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
fn spawn_open<T: Send + 'static>(
    tx: mpsc::Sender<Opened<T>>,
    generation: u64,
    open: impl FnOnce() -> Result<T, CameraState> + Send + 'static,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("camera-open".to_owned())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(open))
                .unwrap_or(Err(CameraState::Unavailable));
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

/// A camera-on that starts no open closes the portal remote the core sent
/// with it (Linux portal path), so that remote can never start a later
/// capture. On the portal path the core sends each camera-on directly after
/// its own remote and sends no other camera-on (`Call::camera_command` in
/// the core), so the remote waiting now is this command's. If a newer remote
/// came first (a race of microseconds against a desktop dialog), it is
/// closed and its open reports `Unavailable`: the failure is closed. Off the
/// portal path there is none.
fn drop_unused_handle() {
    #[cfg(target_os = "linux")]
    drop(crate::camera_channel::take_handle());
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

async fn event_loop(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<RoomEvent>,
    input: &mut Receiver<Input>,
    output: &Output,
    call: &mut Call<'_>,
    camera_failed: &mut Receiver<u64>,
    open_done: &mut Receiver<Opened<OpenedCamera>>,
    tracker: &mut SpeakingTracker,
) -> End {
    let mut tick = interval(SPEAKING_TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let mut outgoing = Vec::new();
        tokio::select! {
            event = events.recv() => match event {
                // A decryption failure of one participant does not end the call.
                Some(event) => {
                    if call.on_event(&event).await.is_err() {
                        return End::Failed(FailureCode::Internal);
                    }
                    if let Some(change) = room_change(&event) {
                        outgoing = apply(change, tracker);
                    }
                }
                None => return End::Failed(FailureCode::Network),
            },
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
            _ = tick.tick() => {}
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

/// An I420 picture as RGBA in memory order R, G, B, A.
///
/// libyuv names its formats by the order of a little-endian 32-bit word, so
/// libyuv's "ABGR" is the byte order R, G, B, A in memory. That is
/// `VideoFormatType::ABGR` here (see the test below). `VideoFormatType::RGBA`
/// would give A, B, G, R in memory.
fn to_rgba(picture: &I420Buffer) -> Option<(u32, u32, Vec<u8>)> {
    let (width, height) = (picture.width(), picture.height());
    let mut rgba = vec![0_u8; rgba_len(width, height)?];
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

/// Reads the pictures of one remote track, at most 15 per second, and puts the
/// newest into the tile's slot. It ends when the track ends or when it is
/// aborted; it holds no state that needs a clean-up.
async fn read_pictures(mut stream: NativeVideoStream, tile: u8, pictures: PictureSink) {
    let mut limiter = RateLimiter::default();
    while let Some(frame) = stream.next().await {
        if !limiter.admit(std::time::Instant::now()) {
            continue;
        }
        let Some(mut picture) = buffer_to_i420(&*frame.buffer) else {
            continue;
        };
        let Some((fit_w, fit_h)) = fit_within_box(picture.width(), picture.height()) else {
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
        if let Some((width, height, rgba)) = to_rgba(&picture) {
            pictures.put(tile, rotation, width, height, rgba);
        }
    }
}

fn start_audio() -> Result<PlatformAudio, FailureCode> {
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

/// Reduces a room event to what the core is told. Everything else is dropped.
fn room_change(event: &RoomEvent) -> Option<RoomChange> {
    match event {
        RoomEvent::ParticipantConnected(participant) => {
            Some(RoomChange::Joined(participant.identity().to_string()))
        }
        RoomEvent::ParticipantDisconnected(participant) => {
            Some(RoomChange::Left(participant.identity().to_string()))
        }
        RoomEvent::ActiveSpeakersChanged { speakers } => Some(RoomChange::Speakers(
            speakers
                .iter()
                .map(|speaker| speaker.identity().to_string())
                .collect(),
        )),
        RoomEvent::Disconnected { .. } => Some(RoomChange::Disconnected),
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

    fn solid_i420(width: u32, height: u32, y: u8, u: u8, v: u8) -> I420Buffer {
        let mut buffer = I420Buffer::new(width, height);
        let (y_plane, u_plane, v_plane) = buffer.data_mut();
        y_plane.fill(y);
        u_plane.fill(u);
        v_plane.fill(v);
        buffer
    }

    /// GIVEN a red I420 picture WHEN it is made RGBA THEN the bytes in memory
    /// are R, G, B, A (libyuv "ABGR"), with full alpha.
    #[test]
    fn given_red_picture_when_made_rgba_then_bytes_are_r_g_b_a() {
        // BT.601 limited range red: Y 81, U 90, V 240.
        let (width, height, rgba) = to_rgba(&solid_i420(4, 2, 81, 90, 240)).expect("size ok");
        assert_eq!((width, height), (4, 2));
        assert_eq!(rgba.len(), 4 * 2 * 4);
        for pixel in rgba.chunks(4) {
            assert!(pixel[0] > 230, "red byte first: {pixel:?}");
            assert!(pixel[1] < 25 && pixel[2] < 25, "{pixel:?}");
            assert_eq!(pixel[3], 255);
        }
        // Blue: Y 41, U 240, V 110 puts the big byte third.
        let (_, _, rgba) = to_rgba(&solid_i420(2, 2, 41, 240, 110)).expect("size ok");
        assert!(rgba[2] > 200 && rgba[0] < 40, "{:?}", &rgba[..4]);
    }

    /// GIVEN a picture outside the wire limits WHEN it is made RGBA THEN there
    /// is no picture.
    #[test]
    fn given_oversized_picture_when_made_rgba_then_it_is_refused() {
        assert!(to_rgba(&solid_i420(641, 2, 0, 128, 128)).is_none());
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
        let (tx, mut done) = mpsc::channel::<Opened<u8>>(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        spawn_open(tx, 3, move || {
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
        let (tx, done) = mpsc::channel::<Opened<Released>>(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let held = Released(drops.clone());
        spawn_open(tx, 1, move || {
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
        let (tx, mut done) = mpsc::channel::<Opened<u8>>(1);
        spawn_open(tx, 1, || panic!("backend fault")).expect("thread starts");
        let opened = done.recv().await.expect("a result arrives");
        assert_eq!(opened.result.err(), Some(CameraState::Unavailable));
    }
}
