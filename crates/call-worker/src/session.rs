//! One call: joins the LiveKit room and runs until the core ends it or the
//! room fails. All LiveKit types stay in this module.

use std::time::Duration;

use livekit::{
    Room, RoomError, RoomEvent, RoomOptions,
    e2ee::{
        E2eeOptions,
        key_provider::{
            DEFAULT_RATCHET_SALT, KeyDerivationAlgorithm, KeyProvider, KeyProviderOptions,
        },
    },
    id::ParticipantIdentity,
    options::TrackPublishOptions,
    prelude::{
        AudioProcessingOptions, LocalAudioTrack, LocalTrack, LocalTrackPublication, PlatformAudio,
        TrackSource,
    },
    rtc_engine::EngineError,
    webrtc::peer_connection_factory::{IceServer, IceTransportsType, RtcConfiguration},
};
use livekit_signaling::SignalError;
use persephone_call_protocol::{FailureCode, FrameKeyMsg, FromWorker, ToWorker};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::{Instant, MissedTickBehavior, interval};

use crate::mapping::{
    ConnectFault, RoomChange, SpeakingTracker, apply, failure_for_connect, valid_identity,
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
pub async fn run(first: ToWorker, input: &mut Receiver<Input>, output: &Sender<FromWorker>) -> u8 {
    let ToWorker::Join {
        sfu_url,
        jwt,
        turn,
        relay_only,
        own_identity,
        mut own_key,
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

    let end = event_loop(
        &mut events,
        input,
        output,
        &provider,
        &own,
        &publication,
        &mut tracker,
    )
    .await;
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

#[allow(clippy::too_many_arguments)]
async fn event_loop(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<RoomEvent>,
    input: &mut Receiver<Input>,
    output: &Sender<FromWorker>,
    provider: &KeyProvider,
    own: &ParticipantIdentity,
    publication: &LocalTrackPublication,
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
                    match handle_command(message, provider, own, publication) {
                        Ok(reply) => outgoing.extend(reply),
                        Err(code) => return End::Failed(code),
                    }
                }
                Some(Input::Bad) => return End::Failed(FailureCode::Protocol),
            },
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

/// Handles one command after `Join`. A second `Join` is a protocol failure.
fn handle_command(
    message: ToWorker,
    provider: &KeyProvider,
    own: &ParticipantIdentity,
    publication: &LocalTrackPublication,
) -> Result<Vec<FromWorker>, FailureCode> {
    match message {
        ToWorker::SetKey { identity, mut key } => {
            // A refused key (index out of the ring) leaves the old key; the
            // sender's audio then stays silent. It does not end the call.
            if valid_identity(&identity) {
                set_key(provider, &ParticipantIdentity::from(identity), &mut key);
            }
            Ok(Vec::new())
        }
        ToWorker::SetOwnKey { mut key } => {
            set_key(provider, own, &mut key);
            Ok(Vec::new())
        }
        // The key provider has no way to remove one participant's keys.
        ToWorker::RemoveParticipant { .. } => Ok(Vec::new()),
        ToWorker::SetMuted { muted } => {
            if muted {
                publication.mute();
            } else {
                publication.unmute();
            }
            Ok(vec![FromWorker::Muted { muted }])
        }
        ToWorker::Join { .. } | ToWorker::Leave => Err(FailureCode::Protocol),
    }
}

/// Hands the key to LiveKit. The bytes are moved out of the zeroizing
/// wrapper, so no second copy is made here.
fn set_key(provider: &KeyProvider, identity: &ParticipantIdentity, key: &mut FrameKeyMsg) -> bool {
    let bytes = std::mem::take(&mut *key.bytes);
    provider.set_key(identity, i32::from(key.index), bytes)
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

async fn fail(output: &Sender<FromWorker>, code: FailureCode) -> u8 {
    let _ = output.send(FromWorker::Failed { code }).await;
    EXIT_FAILED
}
