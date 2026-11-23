//! Native lifecycle workloads for the vendored patches: the remote audio
//! volume (ADR-0055) and the hidden built-in echo canceller (ADR-0043).
//!
//! `third_party/webrtc-sys-0.3.54/AGENTS.md` asks every new C++-reachable
//! feature for a repeated initialize, use, disconnect or drop, and shutdown
//! workload. The vendored crates are patch sources, not workspace members,
//! so the workload runs here, through the same `livekit::webrtc` API the
//! worker uses. It needs no network: a loopback pair of peer connections
//! makes a real remote audio track as soon as the answer side sets the offer.

use livekit::webrtc::{
    audio_source::{AudioSourceOptions, native::NativeAudioSource},
    audio_track::RtcAudioTrack,
    media_stream_track::MediaStreamTrack,
    peer_connection::{AnswerOptions, OfferOptions, TrackEvent},
    peer_connection_factory::{
        PeerConnectionFactory, RtcConfiguration, native::PeerConnectionFactoryExt,
    },
};
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::Duration;

/// Volumes in and out of range, and NaN, which the binding makes silent.
const VOLUMES: [f64; 8] = [0.0, 0.25, 1.0, -1.0, 2.0, 10.0, f64::NAN, 0.5];
/// Iterations measured after the warm-up.
const ROUNDS: usize = 24;
/// The growth in native resources that the whole measured run may show.
/// A leaked factory or peer connection costs threads, sockets and handles
/// on every round, so a leak passes this within a few rounds. Other tests
/// in the same process may add a little noise.
const ALLOWED_GROWTH: u64 = 40;

fn set_volumes(track: &RtcAudioTrack) {
    for volume in VOLUMES {
        track.set_volume(volume);
    }
}

fn local_track(factory: &PeerConnectionFactory) -> RtcAudioTrack {
    let source = NativeAudioSource::new(AudioSourceOptions::default(), 48_000, 1, 100);
    factory.create_audio_track("persephone-volume", source)
}

/// One round: a factory, a local track, a loopback pair and the remote
/// track; the volume is set on both tracks while connected, after the
/// remote track is disabled and enabled, and after both connections close.
/// Everything is dropped at the end, the factory last.
async fn round() {
    let factory = PeerConnectionFactory::default();

    // Partial start: a factory with a local track and no connection. A local
    // source ignores the volume; the call must still be safe.
    let unused = local_track(&factory);
    set_volumes(&unused);
    drop(unused);

    let sender = factory
        .create_peer_connection(RtcConfiguration::default())
        .expect("sending connection");
    let receiver = factory
        .create_peer_connection(RtcConfiguration::default())
        .expect("receiving connection");
    let (tracks, remote) = sync_channel::<MediaStreamTrack>(4);
    receiver.on_track(Some(Box::new(move |event: TrackEvent| {
        let _ = tracks.try_send(event.track);
    })));

    let local = local_track(&factory);
    sender
        .add_track(MediaStreamTrack::Audio(local.clone()), &["persephone"])
        .expect("audio track added");
    let offer = sender
        .create_offer(OfferOptions::default())
        .await
        .expect("offer");
    sender
        .set_local_description(offer.clone())
        .await
        .expect("local offer");
    receiver
        .set_remote_description(offer)
        .await
        .expect("remote offer");
    let answer = receiver
        .create_answer(AnswerOptions::default())
        .await
        .expect("answer");
    receiver
        .set_local_description(answer.clone())
        .await
        .expect("local answer");
    sender
        .set_remote_description(answer)
        .await
        .expect("remote answer");

    let remote = remote_audio(&remote);
    set_volumes(&remote);
    set_volumes(&local);
    remote.set_enabled(false);
    set_volumes(&remote);
    remote.set_enabled(true);
    remote.set_volume(0.75);

    // Disconnect, then use: a call after the receiver stopped must be safe,
    // because a track can outlive its room by a moment in the worker.
    receiver.on_track(None);
    sender.close();
    receiver.close();
    set_volumes(&remote);
    set_volumes(&local);

    drop(remote);
    drop(local);
    drop(receiver);
    drop(sender);
    drop(factory);
}

/// The remote audio track that the answer side made for the offer.
fn remote_audio(tracks: &Receiver<MediaStreamTrack>) -> RtcAudioTrack {
    match tracks
        .recv_timeout(Duration::from_secs(10))
        .expect("the answer side reports the remote track")
    {
        MediaStreamTrack::Audio(track) => track,
        MediaStreamTrack::Video(_) => panic!("an audio track was sent"),
    }
}

/// Native resources of this process that a leak would grow: handles on
/// Windows (threads, events, sockets), threads and descriptors on Linux.
#[cfg(windows)]
fn native_resources() -> Option<u64> {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
    let mut count = 0_u32;
    // SAFETY: the pseudo handle of this process needs no closing, and `count`
    // is a valid place for the answer.
    let ok = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
    (ok != 0).then_some(u64::from(count))
}

#[cfg(target_os = "linux")]
fn native_resources() -> Option<u64> {
    let threads = std::fs::read_dir("/proc/self/task").ok()?.count();
    let descriptors = std::fs::read_dir("/proc/self/fd").ok()?.count();
    u64::try_from(threads + descriptors).ok()
}

/// macOS: the workload runs, but no count is taken.
#[cfg(not(any(windows, target_os = "linux")))]
fn native_resources() -> Option<u64> {
    None
}

/// GIVEN the volume binding on local and remote audio tracks
/// WHEN a factory, a loopback call and its tracks are made, used with in and
/// out of range volumes (and NaN), disabled, closed, used again and dropped,
/// many times
/// THEN nothing crashes, and the process's native resources (handles on
/// Windows, threads and descriptors on Linux) return to their level after
/// the warm-up rounds.
#[test]
fn given_repeated_calls_when_the_volume_is_used_and_dropped_then_native_resources_are_released() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        // Warm-up: one-time native state (the WebRTC runtime, the log sink,
        // lazily made threads) is made here, outside the measurement.
        round().await;
        round().await;
        let before = native_resources();
        for _ in 0..ROUNDS {
            round().await;
        }
        let after = native_resources();
        if let (Some(before), Some(after)) = (before, after) {
            assert!(
                after <= before + ALLOWED_GROWTH,
                "native resources grew from {before} to {after} over {ROUNDS} rounds"
            );
        }
    });
}

/// GIVEN the worker's audio setup, which asks for echo cancellation
/// (ADR-0043 amendment of 2026-11-23)
/// WHEN the platform audio device module is made and configured, and is
/// then asked to prefer the operating system's processing
/// THEN it never offers a built-in echo canceller, so libwebrtc keeps its
/// software one and the microphone starts without waiting for playout.
/// A machine with no audio device has no platform module to check.
#[test]
fn platform_audio_offers_no_builtin_aec() {
    use livekit::prelude::{AudioProcessingOptions, PlatformAudio};
    for _ in 0..3 {
        let audio = match crate::session::start_audio() {
            Ok(audio) => audio,
            Err(_) => {
                assert!(
                    PlatformAudio::new().is_err(),
                    "the worker's audio setup failed with a platform module present"
                );
                return;
            }
        };
        assert!(!audio.is_hardware_aec_available());
        audio
            .configure_audio_processing(AudioProcessingOptions {
                echo_cancellation: true,
                noise_suppression: true,
                auto_gain_control: true,
                prefer_hardware_processing: true,
            })
            .expect("configuring audio processing");
        assert!(!audio.is_hardware_aec_available());
        drop(audio);
    }
}
