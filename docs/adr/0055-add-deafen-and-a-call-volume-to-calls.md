# Add deafen and a call volume to calls

* Status: proposed
* Deciders: project owner
* Date: 2026-11-19

Technical Story: on 19 November 2026 the owner asked for a volume setting and
mute and deafen buttons that can be changed during a call. The owner said
that deafen must turn off all incoming call audio and mute the microphone.

## Context and Problem Statement

Calls (ADR-0043, ADR-0048) had a mute button and a choice of speaker, but no
way to make the call quieter or silent. Remote audio is decoded and played
by libwebrtc in the contained call worker, not by the page. LiveKit's Rust
SDK 0.11 can turn a remote audio track on or off, but it has no playout
volume. How does the user silence or adjust the call, and where is the
volume kept?

## Decision Drivers

* The page stays keyless and plays no call audio (ADR-0004, ADR-0043).
* Deafen must silence every remote track, including a track that arrives
  after the user deafened.
* Deafen must never leave the microphone open.
* A volume change during a call must apply at once, and must not flood the
  core or the worker while a slider moves.
* Bounded values on every hop.
* No change to the call's cryptography or Matrix events.

## Considered Options

* Deafen and volume in the call worker, through a small patch that exposes
  libwebrtc's remote audio source volume.
* Deafen only, through LiveKit's existing track enable switch, with no
  volume.
* Volume through the operating system mixer for the worker process.
* Scale decoded audio samples in a custom audio sink in the worker.

## Decision Outcome

Proposed option: "Deafen and volume in the call worker, through a small
patch".

### Behaviour

* **Mute** turns the microphone off or on, as before.
* **Deafen** turns off every remote audio track in the call and mutes the
  microphone. The core sends `SetMuted(true)` before `SetDeafened(true)`,
  so the microphone is closed before the call goes silent.
* **Undeafen** turns remote audio back on and restores the user's own
  earlier mute choice: a user who was muted before deafening stays muted.
  Remote audio comes back before the microphone opens.
* **Unmute while deafened** also undeafens, because a user who unmutes
  wants to talk and must hear the reply.
* **Mute while deafened** keeps the call deafened, and the later undeafen
  keeps the microphone muted.
* **Call volume** is one playout level from 0 to 100 percent for all remote
  audio. The default is 100, the sound as received. It cannot amplify.
* A new call always starts undeafened, with the saved volume.

The core keeps the user's mute and deafen choices for each call
(`AudioWants` in `crates/core/src/calls/controller.rs`) and sends the worker
the ordered commands. The worker reports `Deafened`, as it reports `Muted`,
and the page shows the worker's state, not its own request.

### Worker playout

* The worker keeps one `Playout` (deafened and volume) for the whole call.
  It covers the own SFU and every connection to another SFU (ADR-0054). It
  applies it to every subscribed remote audio track when it changes, to the
  tracks of another SFU's room when that connection opens, and to each new
  remote audio track when the track's `TrackSubscribed` event is handled.
* A track is set to the volume first and then enabled or disabled. A
  disabled remote track plays at volume 0 in libwebrtc, and the receiver
  keeps the volume for when the track is enabled again.
* The volume reaches libwebrtc through `RemoteAudioSource::SetVolume` with
  a gain of percent / 100. libwebrtc accepts 0 to 10; Persephone uses only
  0 to 1.

### Vendored patch

LiveKit's Rust crates do not expose a remote volume, so `webrtc-sys` 0.3.54
and `libwebrtc` 0.4.1 get one marked addition each (see each crate's
`PERSEPHONE-PATCH.md`):

* `webrtc-sys`: `AudioTrack::set_volume(double)` clamps the value to 0..1
  (NaN becomes 0) and calls `SetVolume` on the track's source on the
  signaling thread, where `AudioRtpReceiver::OnSetVolume` expects it.
* `libwebrtc`: `RtcAudioTrack::set_volume(f64)` calls it.

The patch adds no new native object, thread, callback or observer. A test in
the call worker (`native_volume_tests.rs`) runs the lifecycle workload that
`webrtc-sys`'s `AGENTS.md` asks for: a factory, a loopback pair of peer
connections and their local and remote audio tracks, used with in and out of
range volumes, closed, used again and dropped, 26 times. It checks that the
process's handles (Windows) or threads and descriptors (Linux) come back to
their level after the warm-up. macOS runs the workload without the count.

### Wire and storage

* Call protocol version 7: `Join` carries `volume`, and the new messages are
  `SetDeafened` (tag 11), `SetVolume` (tag 12) and `Deafened` (tag 17). A
  volume over 100 is refused when decoded. Version 6 messages are refused.
* Core protocol: `CallDeafen { deafened }` and `CallVolume { percent }`. The
  core and the host both refuse a volume over 100 before any work.
* The Tauri host has two new commands, `call_deafen` and `call_volume`, each
  with its own permission in the main window's capability.
* `CallStatus` has `deafened` and `volume`. `CallDevices` has `volume`.
* The volume is saved in the profile's call device record (optional field
  `volume`), next to the chosen devices. Saving the devices keeps it.
  Deafen is not saved.

### Coalescing

* The page keeps at most one volume request in flight. Values that arrive
  meanwhile replace each other, and the last one is always sent
  (`LatestOnly` in `frontend/src/calls.ts`).
* The core keeps the newest volume of a running call in a `watch` channel.
  The call task sends the worker only the newest value, so a fast slider
  cannot fill the bounded command queue.
* A volume change applies to the running call before it is saved. If the
  save fails, the call keeps the new volume and the page shows the error,
  in the call bar as in Settings. Only the last value's outcome counts: a
  failure that a later saved value replaced shows nothing.

### Controls

* The call bar has a Deafen button with an accessible description and a
  volume slider (0 to 100 in steps of 5). Both work during a call.
* Settings, Calls has the same volume slider. It saves at once and changes
  a running call.

### Positive Consequences

* The page still plays no audio and holds no key. No secret moves.
* Deafen covers every remote track, also one that arrives later, and closes
  the microphone first.
* A volume change reaches the worker within one round trip and costs one
  small message.

### Negative Consequences

* A third vendored change to maintain in `webrtc-sys` and `libwebrtc`, which
  must be reapplied for each LiveKit update.
* A remote track that arrives while deafened is silenced when its
  subscription event is handled, not before. Its frames cannot decrypt
  before the worker has its frame key, so sound before that is unlikely,
  but this is not proven.
* An older build cannot read a device record that has a volume: the record
  refuses unknown fields. After a downgrade, call device settings are
  unreadable and calls are refused until the device choice is saved again
  in the older build.
* Version 7 of the call protocol is not compatible with an older call
  worker. The core and worker ship together, so this affects only mixed
  development builds.
* Deafen does not tell the other members. Element Call has no standard
  deafened state; the members see only that the microphone is muted.
* Not verified live. A live run is needed before any claim that deafen and
  the volume work with Element Call.

## Pros and Cons of the Options

### Patch for the remote source volume

* Good, because libwebrtc already applies the volume in its playout path.
* Good, because the patch is a few lines with no new ownership edges.
* Bad, because it is one more vendored change.

### Deafen only

* Good, because it needs no patch.
* Bad, because it does not meet the owner's request for a volume.

### Operating system mixer

* Good, because it needs no WebRTC change.
* Bad, because each OS has a different mixer API, and the contained worker
  may not be allowed to use it.
* Bad, because the mixer can also be changed outside Persephone, so the
  saved volume would not be reliable.

### Custom audio sink

* Good, because it needs no change to `webrtc-sys`.
* Bad, because the worker would have to replace libwebrtc's playout
  device module and mix the audio itself, which is a much larger change.

## Links

* [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
* [ADR-0043](0043-make-audio-calls-through-matrixrtc-in-a-contained-call-worker.md)
* [ADR-0048](0048-add-camera-video-to-calls-in-the-call-worker.md)
* [ADR-0049](0049-contain-the-call-worker-on-linux-and-macos.md)
* [ADR-0054](0054-connect-to-call-members-on-other-sfus.md)
