# Add camera video to calls in the call worker

* Status: proposed
* Deciders: project owner
* Date: 2026-11-17

Technical Story: on 17 November 2026 the owner asked for video calls, built
on the audio calls of ADR-0043, and chose the options recorded under "Owner
choices" below.

## Context and Problem Statement

ADR-0043 made calls audio only and left video for a later decision. Video
adds three things that audio did not have: a camera that must be read, video
frames from other members that must be decoded, and pictures that must reach
the screen. The page is keyless (ADR-0004), so it cannot hold the frame keys
that decrypt the video. LiveKit's Rust SDK has no camera capture: it gives a
video source that the caller fills with frames. Where is the camera read,
where is video decoded, and how do the pictures reach the page?

## Decision Drivers

* The page stays keyless and decodes no received media (ADR-0003, ADR-0004).
* The call worker keeps holding no Matrix credentials or Matrix keys.
* Bounded sizes, queues and rates on every hop.
* Interoperate with current Element Call and Element X video.
* The camera is used only when the user turns it on, and only the camera the
  user chose.

## Considered Options

* Read the camera with Media Foundation in the call worker, decode video in
  the worker, and send bounded RGBA pictures to the page through the core.
* The same, but the worker draws video into its own native window.
* A cross-platform camera crate (`nokhwa`) in the worker.
* Receive-only video first.

## Decision Outcome

Proposed option: "Media Foundation in the call worker, bounded RGBA pictures
to the page".

### Processes

* The call worker reads the camera, publishes it, decodes the video of
  other members, and scales every picture down before it leaves the worker.
  Video uses the same LiveKit room, frame encryption and frame keys as
  audio. No new secret leaves the core.
* The core relays pictures. It never decodes video. It checks every picture
  against the limits below before it allocates, and keeps only the newest
  picture of each tile.
* The page receives raw RGBA pixels and draws them with `putImageData`. It
  decodes no video and no image format.

### Camera

* Windows: Media Foundation through the `windows` crate (0.62.2, already in
  `Cargo.lock` through Tauri), with only the features the capture needs. The
  owner's choice named `windows-sys`; it has no COM interface bindings, so
  the `windows` crate is used instead. No new crate version is added.
* The camera code has a platform-neutral interface (list the cameras, open
  one camera for frames) with one backend for each OS. Only the Windows
  backend exists. Other systems list no cameras and report the camera as
  unavailable. Owner request, 17 November 2026: shape it so that later
  backends need no session changes. The intended later backends are the
  PipeWire camera portal on Linux, so that the worker gets a PipeWire handle
  and never opens a device node, and AVFoundation on macOS. Each one needs
  its own decision and containment review.
* The call worker's AppContainer gains the `webcam` capability. Its
  capabilities become `internetClient`, `microphone` and `webcam`. Whether a
  mitigation must be relaxed for the camera is not yet measured; each one
  that is relaxed is measured and recorded, as in ADR-0043.
  Amendment, 24 November 2026: the owner reported that the camera test did
  not work on Windows. The named `webcam` SID from
  `DeriveCapabilitySidsFromName` lists the camera but its open fails with
  `E_ACCESSDENIED`. The token now also carries `webcam` as a device
  capability SID, `S-1-15-3-<device class GUID as four RIDs>`
  (`S-1-15-3-3845273463-1331427702-1186551195-1148109977`), the form a
  packaged app's token has. It is the same capability, not a new one; no
  mitigation changed. Evidence:
  [camera-windows.md](../evidence/camera-windows.md).
* A camera is the Media Foundation symbolic link (its ID) and the friendly
  name. The same limits and name rules as microphones apply (ADR-0043,
  "Audio devices"). `ListDevices` also lists at most 32 cameras. Listing
  opens no camera.
* The camera choice is part of the call device record. It applies from the
  next time the camera turns on. None means the first camera that Media
  Foundation lists.
* A chosen camera that is not connected leaves the camera off, and the page
  says so. The worker never falls back to another camera.
* A camera problem never ends the call: the camera stays off and the worker
  reports a closed camera state (not found, or unavailable). Audio goes on.
* Turning the camera off stops capture and unpublishes the track, so the
  camera light goes out. It is not a mute.
* The worker captures at about 640x360 and publishes one VP8 layer at
  15 frames per second.
* Each time the camera track is published, the worker sets its frame
  cryptor to the current own key index, and `SetOwnKey` changes the index of
  every own track, not only the microphone (ADR-0043, "Use").

### Starting video

* Owner choice: a "Video call" button joins with the camera on and writes
  `m.call.intent` `video` in the membership. The existing call button joins
  with the camera off and writes `audio`.
* A camera button in the call bar turns the camera on and off in either kind
  of call. The intent is not rewritten during a call.

### Pictures

* Owner choice: at most 640x360 at 15 frames per second, at most 4 remote
  tiles and the own preview.
* A picture is at most 640 pixels on each side and at most 230,400 pixels
  (640x360) in all, so at most 921,600 bytes of RGBA. The worker scales to
  fit and keeps the aspect ratio. It sends the rotation (0, 90, 180 or 270
  degrees) beside the picture; the page rotates it on screen.
* Tile 0 is the own camera. Tiles 1 to 4 are remote video tracks, given in
  the order they are subscribed, only to identities that are members of the
  LiveKit room. Other video tracks are not decoded into pictures; their
  members show as audio tiles. A free tile goes to the longest-waiting track.
* The worker sends at most 15 pictures per second for each tile and keeps
  only the newest picture of each tile when the pipe is busy. Control
  messages go first; a picture never delays them.
* The core keeps the newest picture of each tile (at most 5, so at most
  about 4.6 MB), numbered in one rising sequence. It tells the page through
  a content-free, coalesced event. The page asks for each tile's picture
  newer than the one it has, with at most one request in flight for each
  tile.
* A picture travels as a call-protocol control message followed by one
  media-protocol `RgbaPixels` frame. Wire version 3.

### Session lifecycle

* Unchanged from ADR-0043: lock, sign-out, an ended session and exit kill
  the worker at once. The core drops every picture with the call.

### Owner choices

* Camera: Media Foundation in the call worker (Windows only, like calls).
* Display: bounded pixels to the page.
* Limits: 640x360, 15 frames per second, 4 remote tiles.
* Start: a dedicated video call button, and a camera button during any call.

### Positive Consequences

* Video adds no secret to any process, and the page still decodes nothing.
* One protocol, one SFU path and one key exchange for audio and video.

### Negative Consequences

* The worker now decodes VP8, VP9, H.264 and AV1 in native libwebrtc code,
  which is a much larger attack surface than Opus. The worker has network
  access, so a compromised worker could send the call's video anywhere.
* A compromised worker can turn the camera on. Containment does not stop
  that; only the camera light and the OS privacy indicator show it.
* Raw pictures cost bandwidth: up to about 69 MB/s through two pipes and
  the web view at the limits above.
* Remote video is subsampled to 640x360 and 15 frames per second.
* No video on Linux or macOS until their call profiles exist.
* Not verified live. A live run with Element Call, like the audio run, is
  needed before any claim that video calls work.

## Pros and Cons of the Options

### Media Foundation in the worker, pictures to the page

* Good, because the page stays keyless and decodes nothing.
* Bad, because every picture is copied through the core and the host.

### A worker-owned native window

* Good, because no pixels pass through the core.
* Bad, because the contained worker would need window access and placement
  over the main window, which weakens containment.

### `nokhwa`

* Good, because it is cross-platform.
* Bad, because it adds a dependency tree to audit, while calls run only on
  Windows.

### Receive-only video first

* Good, because no camera access is needed.
* Bad, because the owner asked for two-way video.

## Links

* [ADR-0003](0003-run-untrusted-media-decoding-in-a-webassembly-sandbox.md)
* [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
* [ADR-0043](0043-make-audio-calls-through-matrixrtc-in-a-contained-call-worker.md)
* [ADR-0045](0045-exclude-the-main-window-from-screen-capture-by-default.md)
