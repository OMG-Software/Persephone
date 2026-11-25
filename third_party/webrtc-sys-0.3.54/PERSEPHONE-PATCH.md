# Persephone patch: webrtc-sys 0.3.54 (ADR-0043)

Upstream: crates.io `webrtc-sys` 0.3.54, Apache-2.0 (see `NOTICE.md`).
`LICENSE-APACHE` is the standard Apache-2.0 text, copied from the `serde`
1.0.229 crate, because the published crate has none. Used through
`[patch.crates-io]` in the root `Cargo.toml`. Reapply this patch for each
LiveKit update.

## `src/frame_cryptor.cpp`

- `KeyProvider::KeyProvider` clamps the key ring size with
  `std::clamp(options.key_ring_size, 1, 256)` instead of `1, 255`. Without
  this, the Rust change in `libwebrtc` has no effect: a ring of 256 would be
  cut to 255 here and key index 255 would be refused.

## Remote audio volume (ADR-0055)

- `include/livekit/audio_track.h` and `src/audio_track.cpp`:
  `AudioTrack::set_volume(double) const` clamps the volume to 0.0..1.0 (NaN
  becomes 0.0) and calls `SetVolume` on the track's audio source on the
  signaling thread.
  For a remote track, WebRTC's `RemoteAudioSource` passes it to the
  receiver's `OnSetVolume`, which expects that thread. A local track's
  source ignores it.
- `src/audio_track.rs`: the cxx bridge declares `set_volume`.
- No native object, thread, callback or observer is added, so the strong
  reference graph does not change.
- The lifecycle workload this crate's `AGENTS.md` asks for is
  `crates/call-worker/src/native_volume_tests.rs`. These crates are patch
  sources, not workspace members, so their own tests do not run.

Each change is marked `PERSEPHONE PATCH (ADR-0055)`.

## No built-in echo canceller (ADR-0043)

- `src/adm_proxy.cpp`: `AdmProxy::BuiltInAECIsAvailable` returns `false`,
  and `AdmProxy::EnableBuiltInAEC(true)` returns `-1` without reaching the
  device. `EnableBuiltInAEC(false)` still reaches it.
- Why: `WebRtcVoiceEngine::ApplyOptions` turns the built-in echo canceller
  on whenever it is available and the track asks for echo cancellation,
  after LiveKit's `PlatformAudio` turned it off. On Windows that is the
  Voice Capture DMO. It gives no microphone audio unless playout runs
  first, so `StartRecording` failed for a member who joined before anybody
  else was heard, and capture stalled when playout stopped. With the patch
  the voice engine keeps WebRTC's software echo canceller (AEC3).
- No native object, thread, callback or observer is added. The lifecycle
  check is `platform_audio_offers_no_builtin_aec` in
  `crates/call-worker/src/native_volume_tests.rs`.

Each change is marked `PERSEPHONE PATCH (ADR-0043)`.

## Microphone gain and automatic gain control (ADR-0062)

- `include/livekit/peer_connection_factory.h`, `src/peer_connection_factory.cpp`
  and `.rs`: the factory builds its audio processing module (APM) with
  `BuiltinAudioProcessingBuilder().Build(env_)`, which is what the voice
  engine would have built, keeps a reference in `audio_processing_`, and
  gives it to the engine with `CustomAudioProcessing`. If the build returns
  nothing, the old builder is used. Check this against the libwebrtc
  `BuiltinAudioProcessingBuilder` for each update.
- `PeerConnectionFactory::apply_capture_gain(percent)` posts a task to the
  worker thread that reads `AudioProcessing::GetConfig()`, sets
  `capture_level_adjustment.enabled` and `pre_gain_factor` (percent / 100),
  and calls `ApplyConfig`: `WebRtcVoiceEngine::ApplyOptions` reads and writes
  the same configuration there, so neither loses the other's change. The
  task holds its own reference to the APM. A thread runs its tasks in order,
  so the newest gain wins and a later read there sees it.
- The range: the pinned `capture_levels_adjuster.h` takes any `pre_gain`
  from 0 and multiplies each sample by it (clamped to 16 bits), so 0 to 99
  percent lowers the sound. The lifecycle test reads the configuration back
  at 0, 5, 50, 400 and 100 percent; that checks what `ApplyConfig` keeps,
  not the sound.
- A file-local registry (`CaptureGainRegistry`: a mutex, the live factories
  as raw pointers, the gain, 100 at the start, and a count of reads in
  progress with a condition variable) serves the free functions
  `set_capture_gain_percent(uint16_t)` (false for over 400, otherwise stores
  the gain and posts it to every live factory) and
  `capture_processing_state()` (read only, for the oldest factory, which in
  the worker is LiveKit's one runtime factory: the gain
  the APM applies and, for echo cancellation, noise suppression and
  automatic gain control, what was requested and what runs, from
  `GetAudioProcessingState()`, and whether the audio device module has
  playout prepared, playout running and capture running). A factory joins
  the registry at the end of its constructor, takes the stored gain, and
  leaves it first in its destructor, so a gain change never reaches a
  factory that is going away. The mutex guards the registry's fields only:
  a read counts itself, releases the mutex and then queries the factory,
  and a factory that leaves waits until no read runs.
- `AudioTrack::set_auto_gain_control` needs no marshalling: the track is
  WebRTC's `AudioTrack` proxy, which runs `SetAudioProcessingOptions` on
  the signaling thread.
  The registry holds no strong reference.
- `include/livekit/audio_track.h`, `src/audio_track.cpp` and `.rs`:
  `AudioTrack::set_auto_gain_control(bool)` calls `SetAudioProcessingOptions`
  with echo cancellation and noise suppression on (the options that
  `create_device_audio_track` sets) and automatic gain control as asked. It
  returns the result code (0 applied, 1 stored).
- No native thread, callback or observer is added. The only new strong edge
  is the factory's reference to its APM, which the voice engine also holds.
- Lifecycle checks, in `crates/call-worker/src/native_volume_tests.rs`:
  `given_repeated_factories_when_the_gain_is_set_then_each_takes_it_and_resources_are_released`
  (repeated factories, gain changes and automatic gain control calls, with the
  handle count back at its level), `given_a_device_track_when_automatic_level_changes_then_webrtc_takes_it`
  and the ignored `loopback_automatic_level_follows_the_request_while_the_track_is_sent`
  (a real microphone track in a loopback pair: automatic gain control and the
  gain follow the requests, and capture runs with no playout).

Each change is marked `PERSEPHONE PATCH (ADR-0062)`.

`build.rs` is unchanged. Build with `LK_CUSTOM_WEBRTC` set to the verified
libwebrtc, so the build never downloads one.
