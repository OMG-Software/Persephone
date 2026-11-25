# Set the microphone gain and the automatic level in calls

* Status: proposed
* Deciders: project owner
* Date: 2026-11-25

Technical Story: on 25 November 2026 the owner reported that call members
could not hear each other, although the own level meter moved and the device
tests passed, and asked for two settings: an automatic microphone level and a
manual microphone gain.

## Context and Problem Statement

The call worker captures the microphone through libwebrtc's platform audio
device module and sends it through the voice engine's audio processing module
(APM): echo cancellation, noise suppression and automatic gain control
(ADR-0043). The user has no control over the level of the own voice. A quiet
microphone gives a quiet call, and the own level meter (ADR-0055, ADR-0059)
shows it.

Facts from the source on 25 November 2026:

* `PlatformAudio::configure_audio_processing` of LiveKit 0.11 only turns the
  hardware (operating system) effects on or off. The software processing
  comes from the options of the track's audio source:
  `PeerConnectionFactory::create_device_audio_track` in `webrtc-sys` sets
  echo cancellation, automatic gain control and noise suppression to `true`.
* The APM is built inside the voice engine. The LiveKit crates do not expose
  it, and `AudioOptions` has no gain.
* The pinned libwebrtc (`a545de4`) has `AudioProcessing::Config::
  capture_level_adjustment` with `enabled` and `pre_gain_factor` ("scales the
  signal before any processing is done"), `CustomAudioProcessing` to hand a
  ready APM to the factory, and `AudioTrackInterface::
  SetAudioProcessingOptions`, which applies new options to a track that is
  sent.
* `AudioProcessing::GetConfig` and `PeerConnectionFactoryInterface::
  GetAudioProcessingState` report what the module does now, per component:
  what was asked and what runs.

How does the user set the automatic level and the manual gain, so that both
apply during a call and the own level meter shows the effect?

## Decision Drivers

* The page stays keyless and plays and captures no call audio (ADR-0004,
  ADR-0043).
* A change must apply at once in a running call, with no new join and no new
  publish of the microphone.
* Bounded values on every hop: the core, the host, the worker and the page
  each refuse a gain that is out of range or off the step.
* No change to the call's cryptography or Matrix events, and no new
  authority for the worker.
* A small vendored patch, with no new native thread, callback or observer.

## Considered Options

* A small patch in `webrtc-sys` and `libwebrtc` that owns the APM, sets
  `capture_level_adjustment` on it, and turns automatic gain control on or
  off through `SetAudioProcessingOptions`.
* Scale the captured samples in the worker, with a `NativeAudioSource`
  instead of the platform audio device module.
* Set the operating system's microphone volume.
* Offer the automatic level only, with no manual gain.

## Decision Outcome

Proposed option: "A small patch in `webrtc-sys` and `libwebrtc`".

### Behaviour

* **Automatic microphone level** is automatic gain control (WebRTC's
  software AGC). It is on by default. It changes at once in a running call.
* **Microphone gain** is a whole percent from 0 to 400 in steps of 5. The
  default is 100 (no change). It is a linear gain on the captured sound
  before echo cancellation, noise suppression and automatic gain control
  (`pre_gain_factor` = percent / 100). With the automatic level on, the
  automatic level evens out the result, so the gain mostly moves the
  starting point of the automatic level. Above 100 percent the sound may
  clip.
* The own level meter reads the `media-source` level of the microphone
  track. Whether that level is measured after the gain was not measured
  (see the verification below), so the settings page does not promise that
  the meter shows the gain.
* The device tests (ADR-0059, `sound_test.rs`) do not use the gain or the
  automatic level. They open the device with WASAPI, Core Audio or
  PulseAudio on their own. The settings page says so.
* A refusal never ends a call. The worker notes it (ADR-0058) and keeps the
  settings it had.

### Vendored patch

Each change is marked `PERSEPHONE PATCH (ADR-0062)` and described in each
crate's `PERSEPHONE-PATCH.md`:

* `webrtc-sys`, `PeerConnectionFactory`: the factory builds its APM with
  `BuiltinAudioProcessingBuilder` (what the voice engine did) and hands it to
  the engine with `CustomAudioProcessing`, so it keeps a reference.
  `apply_capture_gain(percent)` reads the APM configuration, sets
  `capture_level_adjustment`, and applies it on the worker thread, where the
  voice engine does the same read and write, so no change is lost between
  them.
* `webrtc-sys`: a process-wide registry of the live factories (a mutex, raw
  pointers that each factory removes before it is torn down) and the gain.
  `set_capture_gain_percent(percent)` refuses a gain over 400 percent,
  stores the gain, and applies it to every live factory. It returns false
  when a live factory has no APM of its own (its build failed and the engine
  made one the gain cannot reach), so the worker's note shows the gain as
  refused. A factory that is
  made later takes the stored gain when it joins the registry. This is
  needed because the factory that serves the call is made inside LiveKit and
  is not reachable from the worker.
* `webrtc-sys`: `capture_processing_state()` reports, for the oldest live
  factory (in the worker, LiveKit's one runtime factory, which every room
  shares), the gain the APM applies and, for echo cancellation, noise
  suppression and automatic gain control, what was asked and what runs. It
  is read only and feeds the diagnostic notes. It waits for the worker
  thread, so the worker writes those notes from a blocking task, one at a
  time per kind, never on its event loop.
* The gain is posted to the worker thread, not waited for, and the
  registry's mutex is never held across a wait. The pinned
  `CaptureLevelsAdjuster` takes any `pre_gain` from 0, so a gain under 100
  percent lowers the sound.
* `webrtc-sys`, `AudioTrack::set_auto_gain_control(bool)`: calls
  `SetAudioProcessingOptions` with the options that
  `create_device_audio_track` sets (echo cancellation and noise suppression
  on) and the automatic gain control as asked.
* `libwebrtc`: `RtcAudioTrack::set_auto_gain_control` and the module
  `native::capture_processing` (`set_capture_gain_percent`,
  `capture_processing`).

Neither change adds a native thread, callback or observer. The strong
reference graph gains one edge: the factory holds its APM, as the voice
engine already does. The registry holds no strong reference.

The lifecycle workload that `webrtc-sys`'s `AGENTS.md` asks for is in
`crates/call-worker/src/native_volume_tests.rs`: 26 rounds of a factory made
while a gain is set, a gain change during its life (also an invalid one), the
automatic gain control call, and the factory dropped, with the handle count
back at its level after the warm-up. An ignored test sends a real microphone
track through a loopback pair of connections and checks that automatic gain
control and the gain follow the requests while the track is sent.

### Order

* At the join, the worker sets the gain before the first sound is captured,
  and sets the automatic level on the track right after the publish (a new
  track starts with it on, so this changes only a choice of off).
* The worker sets both again after each device change (`SetDevices`), because
  a change may reconfigure the capture path.
* A gain that is set later changes the APM configuration only. A later
  automatic level change reads and writes the configuration again; the
  ignored loopback test checks that the gain survives it.

### Wire and storage

* Call protocol version 11. `Join` ends with `auto_level` (a flag) and
  `mic_gain` (two bytes, big endian). The new message is
  `ToWorker::SetMicrophone { auto_level, mic_gain }` (tag 16, no answer). A
  gain over 400 or off the step of 5 is refused when decoded, so the whole
  message is refused, and the worker checks it again where it uses it
  (`mapping::Microphone::new`).
* Core protocol: `CallMicrophone { auto_level, gain }`. The host command is
  `call_microphone { autoLevel, gain }`, with its own permission in the main
  window's capability only: the call window has no microphone settings.
* `CallDevices` has `autoLevel` and `micGain`. The core, the host and the
  page each check the gain.
* The settings are saved in the profile's call device record (optional
  fields `auto_level` and `mic_gain`), next to the devices and the volume. A
  setting that was never changed is not written. Saving the devices keeps
  them. The core sends the saved values in `Join`, and sends each change to a
  joining or running call through a watch channel that keeps only the newest
  value, so a slider never fills the command queue. A change made while the
  call joins is sent once the worker is connected.

### Verification on 25 November 2026

Windows 11, debug build, run from the worktree of this change:

* `cargo test -p persephone-call-worker` passed, with the lifecycle workload
  for the gain and automatic gain control.
* The ignored loopback test passed with a real microphone track: with the
  track sent, echo cancellation, noise suppression and automatic gain
  control were reported running; `set_auto_gain_control(false)` and `(true)`
  turned automatic gain control off and on, and the report followed each
  call; a gain of 250 percent was reported by the module and stayed after a
  later automatic level change; capture ran with no playout (3.84 seconds of
  samples and 192 packets in 3.84 seconds).
* Not run: a call with another client, listening by ear, the own level meter
  with a changed gain, and any run on Linux or macOS.

### Diagnostic notes

The same change adds notes to the diagnostic log (ADR-0058), written only
while the log is on. They hold counts, flags and levels, and never a device
ID or name, a key or a token (see the amendment to ADR-0058):

* when the call opens its devices: how many microphones and speakers
  libwebrtc lists, how each was chosen (by the user, the system default
  selected, or left to libwebrtc) and whether the system default was read;
* at each microphone setting: the gain asked, whether it was queued for
  every factory's audio thread (a factory with no audio processing module of
  its own does not take it), the gain the oldest factory applies (none when
  it has no module), the automatic level, and for echo cancellation, noise
  suppression and automatic gain control what was asked and what runs;
* every 5 seconds, while the log is on: the own level, whether the call's
  playout is on and its volume, whether the audio device module has playout
  prepared and running and capture running, and for each remote audio track
  whether it is enabled, the packets received and lost, the samples decoded
  and concealed and the level.

### Positive Consequences

* The user can raise a quiet microphone or turn the automatic level off, in
  a running call, with no new join.
* No secret moves and the page gains no authority. The worker's authority
  does not grow.
* The diagnostic notes make a live call without sound conclusive: they tell
  apart a speaker that is not chosen, a remote track that does not arrive,
  one that arrives and does not decode, and one that plays at level 0.

### Negative Consequences

* A fourth vendored change to maintain in `webrtc-sys` and `libwebrtc`, which
  must be reapplied for each LiveKit update, and a process-wide registry.
* The voice engine's own APM construction is replaced by the same builder
  called in the factory. A new libwebrtc may build the APM differently; the
  patch notes say to check it with each update.
* An older build cannot read a device record that has these fields: the
  record refuses unknown fields. After a downgrade, call device settings are
  unreadable and calls are refused until the device choice is saved again in
  the older build (as for the volume in ADR-0055).
* Version 11 of the call protocol is not compatible with an older call
  worker. The core and worker ship together, so this affects only mixed
  development builds.
* A gain above 100 percent can clip. The automatic level may then lower it
  again, and the user can choose a smaller gain.
* Not verified by ear, in a live call, or on Linux and macOS. The worker
  tests and the ignored loopback test ran on Windows only, on 25 November
  2026. Whether WebRTC's automatic gain control also changes
  the operating system's microphone volume (its analog mode) is not measured.

## Pros and Cons of the Options

### A small patch in `webrtc-sys` and `libwebrtc`

* Good, because libwebrtc applies the gain and the automatic level in its
  own capture path, so the level meter, the echo canceller and the encoder
  all see the same sound.
* Good, because the automatic level changes during a call with
  `SetAudioProcessingOptions`, which libwebrtc supports for a sent track.
* Bad, because it is one more vendored change.

### Scale the samples in the worker

* Good, because it needs no change to `webrtc-sys`.
* Bad, because the worker would capture the microphone itself and feed a
  `NativeAudioSource`, which replaces the platform audio device module and
  its device switching (ADR-0059). That is a much larger change.

### Set the operating system's microphone volume

* Good, because it needs no WebRTC change.
* Bad, because each OS has a different mixer API, the contained worker may
  not be allowed to use it, and the volume is shared with other programs.

### Automatic level only

* Good, because it needs only the track option.
* Bad, because it does not meet the owner's request for a manual gain.

## Links

* [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
* [ADR-0043](0043-make-audio-calls-through-matrixrtc-in-a-contained-call-worker.md)
* [ADR-0055](0055-add-deafen-and-a-call-volume-to-calls.md)
* [ADR-0058](0058-write-a-diagnostic-log-only-when-the-user-turns-it-on.md)
* [ADR-0059](0059-test-call-devices-show-each-participants-level-and-switch-devices-during-a-call.md)
