# Test call devices, show each participant's level and switch devices during a call

* Status: proposed
* Deciders: project owner
* Date: 2026-11-24

Technical Story: on 23 November 2026 the owner reported calls that work only
in part. Persephone received sound (the meter moved) but played none, and
Element did not receive Persephone's sound or video. The owner found that
"the biggest problem is actually Persephone not detecting and listing devices
properly, or even using them properly". The owner asked for microphone,
speaker and camera test buttons; for the meter to show the user's own
microphone; for a level meter and a mute mark for each participant; and for
a device change during a call to apply at once. The owner also said: "if I
select a device, I want you to use the device I've selected".

## Context and Problem Statement

The call worker opens the microphone, speaker and camera inside its
containment (ADR-0043, ADR-0049). When a call is silent, the user cannot see
whether the worker could open the device, whether the operating system
refused it, or whether the device works but the network path fails. The meter
of ADR-0055 showed the loudest remote sound, so it moved even when the own
microphone was dead. How does Persephone show that each device works, inside
the same containment as a call, and how does a device change reach a running
call?

Facts from the source on 24 November 2026:

* LiveKit 0.11.0 gives the own microphone's level as the `audio_level` of the
  `media-source` statistic of the local audio track, and a remote level as
  the `audio_level` of the inbound RTP statistic.
* `PlatformAudio` has `switch_recording_device` and `switch_playout_device`
  for a running call on Windows and macOS. `switch_playout_device` does not
  check that the device exists.
* On Windows, WASAPI in shared mode opens an endpoint by its ID. A refusal by
  the privacy settings returns `E_ACCESSDENIED`.

## Decision Drivers

* Test each device with the same worker, containment and device IDs as a call.
* Use the device the user selected. Fall back to the system default only when
  the user chose "System default".
* Keep the web view keyless and give it codes and numbers only.
* Bound every new message, list and timer.

## Considered Options

* Device tests in a contained call worker that connects to nothing
* Device tests in the web view with `getUserMedia`
* No tests; show the call's own errors only

## Decision Outcome

Chosen option: "Device tests in a contained call worker that connects to
nothing", because only the worker tests the path a call uses. The web view has
no media permissions, and a browser test would prove nothing about the worker.

### Own and participant levels

* `FromWorker::SoundLevel` is now the own microphone's level, from the local
  track's `media-source` statistic. A silent meter now means that the call
  does not hear you. The meter's label is "Your microphone". This replaces
  the meaning in ADR-0055; its timing and bounds stay.
* The worker sends `ParticipantLevel { connection, identity, level }` for each
  remote identity, the loudest of its tracks, through the same `LevelGate`
  rules as the own level. It keeps at most `MAX_PARTICIPANTS` gates, ignores
  an invalid identity and an unknown connection, and sends silence once for a
  participant that left.
* The core keeps the newest level of each device that is not silent and sends
  `CallLevels { levels }` together with `CallSoundLevel`, under the same
  100 ms gap. The host drops a list with more than `MAX_CALL_LEVELS` entries,
  an overlong ID or a level over 100. The page parses it again and drops a
  bad list whole. The list is empty when the call ends.

### Mutes

* The worker sends `ParticipantMuted { connection, identity, muted }` when a
  remote microphone track is subscribed, muted, unmuted or unsubscribed. A
  participant with no microphone track counts as muted.
* `CallParticipant` has `muted`. The call bar and the call window show a
  "Microphone off" mark.

### Device changes during a call

* `set_call_devices` saves the choice, as before. It now also sends the
  microphone and speaker to a running call (`ToWorker::SetDevices`).
* The worker checks both devices before it changes either. A selected device
  that is not connected is `DeviceNotFound`, and the call keeps the devices it
  has. It never falls back to the default device for a selected one.
* The worker answers `DevicesSet { failure }`. The core shows a failure as the
  call status's `deviceChange` (`not_found`, `unsupported` or `failed`) until
  the next change. Linux reports `unsupported`: the PulseAudio path has no
  switch yet. The camera change applies to the next camera start.

### Device tests

* `call_device_test { test, device }` starts a test of the microphone,
  speaker or camera. `device` is a platform ID, or null for the system
  default. The core starts a worker and sends `ToWorker::TestDevice` as its
  first message. That worker never connects to a server.
* The microphone test listens for 15 seconds and sends the own level as in a
  call. The speaker test plays a 440 Hz tone for 1.5 seconds. On Windows, both
  use WASAPI in shared mode on the selected endpoint. The camera test opens
  the camera as a call does and shows its pictures on video tile 0 for at
  most 30 seconds.
* The core runs one test at a time, on its own thread. A new test stops the
  old one and waits for its worker to end. A test is refused with `in_call`
  while a call runs or joins, and a join stops a running test. The core ends
  any test after 100 seconds.
* The states are `idle`, `starting`, `running`, `done`, `not_found`,
  `denied`, `unsupported`, `unavailable`, `in_call` and `failed`. The
  `call_device_test_changed` event tells the page to ask
  `call_device_test_status`.
* `FailureCode::DeviceDenied` (wire value 11) is an operating system refusal,
  shown as `denied` with a pointer to the privacy settings.
  `FailureCode::Unsupported` (10) is a test this platform does not have.
* The call protocol is version 10.

### Positive Consequences

* The user can see which device fails, and whether the system refused it,
  before a call.
* The meter now shows whether the call hears the user.
* A device change applies to a running call without a rejoin.

### Negative Consequences

* The microphone and speaker tests are Windows only. Linux and macOS show
  `unsupported`.
* The camera test on Linux reports `unavailable`, because the portal camera
  grant is not passed to a test worker.
* Linux cannot switch the microphone or speaker during a call.
* Each test starts a worker process, which takes about a second.
