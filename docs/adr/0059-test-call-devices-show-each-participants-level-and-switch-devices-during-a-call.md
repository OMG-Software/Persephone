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
* The worker measures the remote tracks of an audio roster
  (`mapping::AudioRoster`) that the room events keep: a subscription adds a
  track, and an unsubscription, a participant that leaves, a `Disconnected`
  event or a lost connection removes tracks. It keeps at most
  `MAX_KEPT_AUDIO_TRACKS` (128) tracks in subscription order and measures
  the first `MAX_MEASURED_TRACKS` (64). A track that waits moves up when a
  measured one goes; a track past the limit has no meter until it is
  subscribed again. No tick copies the SDK's participant map:
  `Room::remote_participants` copies the whole map under a read lock, and
  its size is the SFU's choice.
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
* LiveKit 0.11 stops a device before it opens the new one, and a failed
  open leaves it stopped. A switch while a device is stopped only selects
  it. After a failed microphone the worker switches back to the old one and
  starts capture again. A microphone switch counts only when capture runs
  after it. The call always has a published microphone, so this never opens
  a microphone that the call did not use. A device it could not restore is
  switched again by the next change, even to the same choice. The answer is
  then `failed`, and the page says that the sound may have stopped.
* Playout has no public start in LiveKit 0.11. After a failed speaker the
  worker selects the old speaker again, but playout stays stopped. The
  speaker is lost for the rest of the call: each later change selects the
  chosen speaker and answers `failed`, and the page asks the user to rejoin
  the call.
* The worker answers `DevicesSet { failure }`. The core shows a failure as the
  call status's `deviceChange` (`not_found`, `unsupported`, `kept` or
  `failed`) until the next change. `kept` (worker code `DeviceKept`) is a
  failed microphone switch that the worker undid: the old microphone
  captures again and the speaker is not lost, so the page does not ask to
  rejoin. Linux moves the call's own sound server streams on a connection
  that the worker opened before stage 2 (ADR-0049, amended 27 November 2026,
  proposed). Where that connection could not open, Linux reports
  `unsupported`.
* `SetDevices` also carries the camera choice. The worker keeps it, and the
  call's next camera start opens it; a camera that is on keeps running until
  it is turned off. On Linux without that connection, a choice whose
  microphone and speaker equal the join's is no sound change, so a change of
  the camera only does not report `unsupported`.

### Device tests

* `call_device_test { test, device }` starts a test of the microphone,
  speaker or camera. `device` is a platform ID, or null for the system
  default. The core starts a worker and sends `ToWorker::TestDevice` as its
  first message. That worker never connects to a server.
* The microphone test listens for 15 seconds and sends the own level as in a
  call. The speaker test plays a 440 Hz tone for 1.5 seconds. The camera test
  opens the camera as a call does and shows its pictures on video tile 0 for
  at most 30 seconds. Each OS opens the microphone and speaker through the
  layer its call audio uses:
  * Windows: WASAPI in shared mode on the selected endpoint.
  * Linux (amended 24 November 2026): one PulseAudio record or playback
    stream through `libpulse.so.0`, which libwebrtc loads too (also with
    `pipewire-pulse`). The test opens the default stream with no device
    name, as libwebrtc does. A `pulse:` choice goes to the launch as for a
    call: the core passes only the tested device's name, the broker sets
    `PULSE_SOURCE` or `PULSE_SINK`, and the worker checks the name against
    the environment and the server's list (`mapping::sound_path`) before it
    opens the stream. The stream opens before containment stage 2, which
    forbids a new `AF_UNIX` socket, and has `PA_STREAM_DONT_MOVE`. No
    Landlock or seccomp rule changes.
  * macOS (amended 24 November 2026): a Core Audio HAL I/O procedure on the
    default device or on the device with the chosen UID, the layer that
    libwebrtc's Mac audio module uses. A UID that names no device is
    `DeviceNotFound`. The device is found before the microphone permission
    is checked; a permission that was not asked yet asks the user and waits
    at most 60 seconds, as for the camera. The call Seatbelt profile is
    unchanged.
  * Linux camera test (amended 24 November 2026): on the portal path the core
    asks the camera portal and sends the remote before `TestDevice`, as the
    device list does. The user's stop ends the portal wait. Without access
    the camera is `unavailable`.
* The core runs one test at a time, on its own thread. A new test stops the
  old one and waits for its worker to end. A test is refused with `in_call`
  while a call runs or joins, and a join stops a running test. The core ends
  any test after 100 seconds. That is no user stop: a test that has not run
  by then ends as `failed`, not `idle`, so the page says why.
* Each status has an `id`, the number of its test (0 for none and for a
  refusal). `call_device_test_stop { id }` stops only that test while it is
  the newest, checked under the test lock, so a late stop from a closed page
  never ends a newer test. Without `id` it stops any test.
* The states are `idle`, `starting`, `running`, `done`, `not_found`,
  `denied`, `unsupported`, `unavailable`, `in_call` and `failed`. The
  `call_device_test_changed` event tells the page to ask
  `call_device_test_status`.
* `FailureCode::DeviceDenied` (wire value 11) is an operating system refusal,
  shown as `denied` with a pointer to the privacy settings. The microphone
  and speaker tests report it: `E_ACCESSDENIED` on Windows, `PA_ERR_ACCESS`
  on Linux, and a refused or restricted microphone permission on macOS. The
  camera path reports one `unavailable`
  for a camera another app holds and for one the privacy settings block,
  so the camera's `unavailable` text names both causes.
* A camera that ran but was not released in time still ran: the test ends
  as `done`.
  `FailureCode::Unsupported` (10) is a test this platform does not have.
* The call protocol is version 10.

### Positive Consequences

* The user can see which device fails, and whether the system refused it,
  before a call.
* The meter now shows whether the call hears the user.
* A device change applies to a running call without a rejoin.

### Negative Consequences

* The Linux and macOS sound tests use their own small native path, not
  libwebrtc's audio module, because that module gives no level and plays no
  sound without a connection. They open the same device through the same
  system layer, but a passing test does not prove that libwebrtc opens it.
* The macOS sound test was compiled for an Apple target only. It was not
  linked or run on a Mac, and the Seatbelt profile was not measured with it.
  The Linux sound tests ran in the contained worker against `pipewire-pulse`
  ([evidence](../evidence/device-tests-linux-macos.md)); the Linux camera
  test with the portal was not run.
* The Linux camera test may show the desktop's camera portal dialog.
* Linux switches the microphone or speaker during a call only through the
  sound server connection (ADR-0049 amendment, proposed). Without it, Linux
  cannot.
* Each test starts a worker process, which takes about a second.
