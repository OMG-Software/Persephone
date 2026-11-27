# Choose the camera send resolution

* Status: proposed
* Deciders: project owner
* Date: 2026-11-27

Technical Story: on 27 November 2026 the owner asked for a choice of the
camera send resolution in calls: Auto, 1440p, 1080p, 720p and 480p, with Auto
as high as the connection allows. The owner must approve this record. Nothing
here is verified with a live camera and SFU yet.

## Context and Problem Statement

The call worker makes one picture from each camera frame. Until now, that
picture was at most 640x360, and the worker gave it to the own preview and
to the SFU (ADR-0048, ADR-0068). The user could not ask for a sharper
picture, and a fast connection did not help. A slow connection could not ask
for a smaller one.

The owner wants a menu with Auto, 1440p, 1080p, 720p and 480p. Auto must
use the highest size that the connection allows. How does the call choose
and change the size that the camera sends?

## Decision Drivers

* The user can choose a fixed size.
* Auto adapts to the connection and does not need a user action.
* The camera and the computer limit the size. A picture is never enlarged.
* The page stays without keys and without a camera stream (ADR-0004).
* The call worker stays contained, and the core owns the call (ADR-0048).
* The frame rate and the CPU cost stay bounded.

## Considered Options

* A fixed size
* WebRTC's own adaptation only
* Simulcast
* A worker-side controller with a user choice

## Decision Outcome

Chosen option: "A worker-side controller with a user choice", because it is
the only option that gives the user a fixed choice and gives Auto a size that
follows measured bandwidth and delay, with one stream and a bounded cost.

### Worker side

The call worker is built by the worker change of this record. The call
protocol is at version 16: `VideoQuality` (`Auto`, `P480`, `P720`, `P1080`,
`P1440`) is in `ToWorker::Join`, and `ToWorker::SetVideoQuality` changes it
during a call. The worker has no answer to it.

* Send tiers: 640x360, 854x480, 1280x720, 1920x1080 and 2560x1440, with VP8
  bitrates of 0.6, 0.9, 1.5, 2.5 and 4 Mbit/s at 15 frames a second.
* A fixed choice sends that tier. The camera's best mode limits it. A camera
  is never enlarged.
* Auto starts at 480p and climbs up to 1440p.
* Auto steps up one tier after 3 good samples (about 6 seconds). A sample is
  good when the available outgoing bandwidth is at least the tier's bitrate,
  the round-trip time is at most 250 ms, and it is at most 1.5 times the
  lowest time seen plus 50 ms. After a step up that fails, a hold of 10
  seconds applies. The hold doubles for each failure, up to 120 seconds.
* Auto steps down one tier after 2 bad samples. A sample is bad when the
  bandwidth is below 0.8 times the tier's bitrate, or the round-trip time is
  above 400 ms. 360p is the floor.
* The samples come from WebRTC's candidate-pair statistics.
* Samples less than 1.5 seconds apart are skipped, so the statistics view's
  one-second reads do not speed Auto up. The hold goes back to 10 seconds only
  after a tier reached by a step up has held for 60 seconds.
* Auto's top tier is 1440p. A fixed choice's top tier is that tier. The size
  of the camera's first picture caps either one. The open thread waits up to
  2 seconds for that picture, so the publish knows the real top tier.
* While the camera is on, Auto reads the WebRTC statistics every 2 seconds,
  also with the statistics view off. With the view off, it reads only the
  own camera track. The core sees no change: statistics go to it only while
  the view is on.

The worker applies a new quality at its next camera start. A camera that is on
keeps its size until it restarts.

### Core, host and page

* The profile's call device record keeps the choice (`video_quality`). A
  record without it reads as Auto. The core maps it to the protocol in one
  place (`calls::devices::worker_quality`). The core sends it in the join.
* The command `call_video_quality { quality }` takes one of `auto`, `480p`,
  `720p`, `1080p` or `1440p`. The type allows no other value, and every
  process that receives the command decodes it again. The permission
  `allow-call-video-quality` is in the main window's and the call window's
  capabilities. It saves the choice also without a call. The choice then
  applies to the next call.
* For a running call, the core keeps the newest value in a watch and the call
  task sends `SetVideoQuality` when it changes, and always before a
  camera-on.
* The core restarts a camera that is on or starting when the quality or the
  camera device changes. It uses the user's own camera commands, `set_camera`
  off and then on, so every portal request, gate and request number applies.
  The call task sends the new device choice and quality to the worker before
  the camera-on. A joining call has no camera to restart. A camera that is off
  stays off.
* The page offers a "Video quality" menu next to the camera menu, in the
  main window's call settings and in the call window's device panel. The
  hint says that the camera and the computer limit the highest resolution.

### Limits

* The frame rate stays at 15 frames a second.
* A video effect caps the send size at 640x360 (ADR-0068).
* macOS sends at most 1280x720. The 1280x720 preset is tried first from 720p
  up. The macOS code was not compiled for this record.
* While any effect runs, the send box is 640x360, also with a fixed 1440p,
  because the effect works at that size.
* Linux V4L2 takes raw YUYV, NV12 and I420 only, up to 2560x1440. MJPEG is
  refused. The worker reads each mode's frame intervals and asks the driver
  for the chosen rate, at most 30. Among modes of at least 15 frames a second,
  it picks the smallest that covers the tier, else the largest of those. An
  unknown rate counts as fast enough. Buffers are bounded: at most 16 MiB each
  and at most 16 buffers.
* On the Linux camera portal path, the worker names only a preferred size (the
  tier's box) and a maximum of 2560x1440. PipeWire's negotiation chooses the
  camera format, often the camera's first listed raw format. The path cannot
  prefer 15 frames a second, because a least rate would make cameras with
  only slow modes fail. A camera could therefore give a large, slow mode.
  This needs a live test with a camera.
* The own preview and the received tiles stay at 640x360. The page shows no
  larger picture.
* A higher tier costs more CPU and bandwidth for the sender and for each
  receiver.
* A restart of a camera on Linux with the camera portal can ask the portal
  again. The portal may show its prompt a second time.
* Not verified: a live camera, a live SFU, and the Auto controller against
  real WebRTC statistics.

### Privacy

The setting stays on the device, in the encrypted profile record. The SFU sees
the resolution of the stream as before. No other party gets new data.

### Positive Consequences

* The user can choose a sharper or a lighter picture.
* Auto follows the connection and needs no user action.
* One stream, one encoder, and no new dependency.

### Negative Consequences

* A change of the quality restarts the camera, so the picture stops for a
  moment.
* Auto reacts in seconds, not at once.
* The measured limits are not verified on real hardware.

## Pros and Cons of the Options

### A fixed size

* Good, because it is simple.
* Bad, because a slow connection cannot use a lower size, and a fast one
  cannot use a higher size.

### WebRTC's own adaptation only

* Good, because it needs no controller of our own.
* Bad, because it lowers the size by itself and does not raise it to a size
  that the user asked for, and the user has no fixed choice.
* Bad, because it reacts to the encoder, not to the measured connection.

### Simulcast

* Good, because each receiver can get the size that suits it.
* Bad, because the sender encodes several streams, which costs CPU and
  upload bandwidth, and it needs SFU support that this project has not
  tested.

### A worker-side controller with a user choice

* Good, because a fixed choice and Auto share one path.
* Good, because the samples come from the statistics that the worker has.
* Bad, because the controller is code that the project must test and tune
  with real devices.

## Links

* [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
* [ADR-0048](0048-add-camera-video-to-calls-in-the-call-worker.md)
* [ADR-0059](0059-test-call-devices-show-each-participants-level-and-switch-devices-during-a-call.md)
* [ADR-0060](0060-show-calls-in-a-separate-call-window.md)
* [ADR-0065](0065-choose-the-large-video-and-change-devices-in-the-call-window.md)
* [ADR-0068](0068-add-background-blur-backgrounds-colour-filters-and-a-mirrored-preview-to-call-video.md)
