//! Native lifecycle workloads for the vendored patches: the remote audio
//! volume (ADR-0055), the hidden built-in echo canceller (ADR-0043) and the
//! microphone gain and automatic gain control (ADR-0062).
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
    native::capture_processing,
    peer_connection::{AnswerOptions, OfferOptions, TrackEvent},
    peer_connection_factory::{
        PeerConnectionFactory, RtcConfiguration, native::PeerConnectionFactoryExt,
    },
    stats::RtcStats,
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

/// One round of the microphone gain workload (ADR-0062): a factory made
/// while a gain is set, a gain change while it lives, the automatic gain
/// control call on a track, and the factory dropped.
fn gain_round() {
    let factory = PeerConnectionFactory::default();
    let report = capture_processing::capture_processing().expect("a live factory reports");
    assert_eq!(
        report.gain_percent, 250,
        "a factory made after the gain was set takes it"
    );
    // The gain moves while the factory lives, and a gain over the limit
    // changes nothing.
    for (percent, accepted) in [(0, true), (5, true), (400, true), (405, false), (100, true)] {
        assert_eq!(
            capture_processing::set_capture_gain_percent(percent),
            accepted
        );
    }
    let after = capture_processing::capture_processing().expect("the factory is alive");
    assert_eq!(after.gain_percent, 100);
    // A track that no device feeds may refuse the options. The call must be
    // safe either way, also after the factory's other tracks are dropped.
    let track = local_track(&factory);
    let _ = track.set_auto_gain_control(false);
    let _ = track.set_auto_gain_control(true);
    drop(track);
    assert!(capture_processing::set_capture_gain_percent(250));
    drop(factory);
}

/// GIVEN a microphone gain set before and while peer connection factories
/// exist
/// WHEN factories are made, read, changed, used for automatic gain control
/// and dropped, many times
/// THEN each new factory takes the gain that is set, one over 400 percent
/// changes nothing, a dropped factory is never reached, and the process's
/// native resources return to their level after the warm-up rounds. This is
/// the only test that sets the gain: it is one value for the whole process.
#[test]
fn given_repeated_factories_when_the_gain_is_set_then_each_takes_it_and_resources_are_released() {
    assert!(capture_processing::set_capture_gain_percent(250));
    gain_round();
    gain_round();
    let before = native_resources();
    for _ in 0..ROUNDS {
        gain_round();
    }
    let after = native_resources();
    if let (Some(before), Some(after)) = (before, after) {
        assert!(
            after <= before + ALLOWED_GROWTH,
            "native resources grew from {before} to {after} over {ROUNDS} rounds"
        );
    }
    assert!(capture_processing::set_capture_gain_percent(100));
}

/// GIVEN a microphone track from the platform audio device module (a
/// machine with no audio device has none)
/// WHEN automatic gain control is turned off and on again, and the gain is
/// set, while the track exists and with no peer connection
/// THEN WebRTC takes each request, and the factory's report shows the gain.
#[test]
fn given_a_device_track_when_automatic_level_changes_then_webrtc_takes_it() {
    use livekit::prelude::{LocalAudioTrack, PlatformAudio};
    let Ok(audio) = crate::session::start_audio() else {
        assert!(PlatformAudio::new().is_err());
        return;
    };
    let track = LocalAudioTrack::create_audio_track("persephone-gain", audio.rtc_source());
    let rtc = track.rtc_track();
    for enabled in [false, true, false, true] {
        assert!(
            rtc.set_auto_gain_control(enabled),
            "WebRTC took automatic gain control {enabled}"
        );
    }
    drop(track);
    drop(audio);
}

/// GIVEN a microphone track from the platform audio device module, sent
/// through a loopback pair of peer connections (it records from the
/// microphone for a moment and sends the sound nowhere)
/// WHEN automatic gain control is turned off and on again, and the gain is
/// set, while the track is sent
/// THEN the audio processing module reports each change: the gain, and
/// automatic gain control as running or not (ADR-0062).
///
/// Run by choice, alone, as it records: `cargo test --locked -p
/// persephone-call-worker -- --ignored loopback_automatic_level`.
#[test]
#[ignore = "records from the microphone"]
fn loopback_automatic_level_follows_the_request_while_the_track_is_sent() {
    use livekit::webrtc::peer_connection::{AnswerOptions, OfferOptions};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let factory = PeerConnectionFactory::default();
        if !factory.acquire_platform_adm() {
            return;
        }
        factory.set_adm_recording_enabled(true);
        // The receiving side of the loopback must not play the microphone
        // through the speakers.
        factory.set_adm_playout_enabled(false);
        let sender = factory
            .create_peer_connection(RtcConfiguration::default())
            .expect("sending connection");
        let receiver = factory
            .create_peer_connection(RtcConfiguration::default())
            .expect("receiving connection");
        // The two connections trade their candidates, so the media path
        // comes up and the send stream starts, as it does in a call.
        let (to_receiver, from_sender) = sync_channel(64);
        let (to_sender, from_receiver) = sync_channel(64);
        sender.on_ice_candidate(Some(Box::new(move |candidate| {
            let _ = to_receiver.try_send(candidate);
        })));
        receiver.on_ice_candidate(Some(Box::new(move |candidate| {
            let _ = to_sender.try_send(candidate);
        })));
        let track = factory.create_device_audio_track("persephone-loopback");
        sender
            .add_track(MediaStreamTrack::Audio(track.clone()), &["persephone"])
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

        for _ in 0..30 {
            while let Ok(candidate) = from_sender.try_recv() {
                let _ = receiver.add_ice_candidate(candidate).await;
            }
            while let Ok(candidate) = from_receiver.try_recv() {
                let _ = sender.add_ice_candidate(candidate).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let report = |what: &str| {
            let state = capture_processing::capture_processing().expect("a live factory");
            eprintln!("LOOPBACK {what}: {state:?}");
            state
        };
        tokio::time::sleep(Duration::from_millis(800)).await;
        // Capture runs with no playout and no built-in echo canceller
        // (ADR-0043 amendment of 2026-11-23): the source's sample time moves
        // and packets leave.
        let stats = sender.get_stats().await.expect("sender statistics");
        let seconds: f64 = stats
            .iter()
            .filter_map(|stat| match stat {
                RtcStats::MediaSource(source) if source.source.kind == "audio" => {
                    Some(source.audio.total_samples_duration)
                }
                _ => None,
            })
            .sum();
        let packets: u64 = stats
            .iter()
            .filter_map(|stat| match stat {
                RtcStats::OutboundRtp(out) => Some(out.sent.packets_sent),
                _ => None,
            })
            .sum();
        eprintln!("LOOPBACK capture with no playout: {seconds:.2} s of samples, {packets} packets");
        assert!(
            seconds > 1.0 && packets > 0,
            "{seconds} s, {packets} packets"
        );
        let sent = report("sent, automatic level on");
        assert_eq!(sent.auto_gain_control.active, Some(true), "{sent:?}");
        assert!(
            sent.recording,
            "the device module reports capture: {sent:?}"
        );
        assert!(track.set_auto_gain_control(false));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let off = report("automatic level off");
        assert_eq!(off.auto_gain_control.active, Some(false), "{off:?}");
        assert!(track.set_auto_gain_control(true));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let on = report("automatic level on again");
        assert_eq!(on.auto_gain_control.active, Some(true), "{on:?}");
        assert!(capture_processing::set_capture_gain_percent(250));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let loud = report("gain 250");
        assert_eq!(loud.gain_percent, 250);
        assert_eq!(
            loud.auto_gain_control.active,
            Some(true),
            "the voice engine keeps the gain and the automatic level together"
        );
        // A later automatic level change makes the voice engine read and
        // write the configuration again: the gain must survive it.
        assert!(track.set_auto_gain_control(false));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let kept = report("gain 250, automatic level off");
        assert_eq!(kept.gain_percent, 250, "{kept:?}");
        assert_eq!(kept.auto_gain_control.active, Some(false), "{kept:?}");
        assert!(capture_processing::set_capture_gain_percent(100));
        sender.close();
        receiver.close();
        drop(track);
        drop(factory);
    });
}
