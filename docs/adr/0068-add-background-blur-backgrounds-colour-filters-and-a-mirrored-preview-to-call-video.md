# Add background blur, backgrounds, colour filters and a mirrored preview to call video

* Status: proposed
* Deciders: project owner
* Date: 2026-11-26

Technical Story: on 26 November 2026 the owner asked for background blurring
and effects on the call video, and chose the options recorded under "Owner
choices" below.

## Context and Problem Statement

The call worker reads the camera, makes one I420 picture of at most 640x360
from each camera frame, and gives that picture to the own preview (tile 0)
and to the SFU (ADR-0048, `CameraFeed::frame` in
`crates/call-worker/src/session.rs`). The page never has the camera stream.
It gets bounded RGBA pictures only, and it decodes no video (ADR-0003,
ADR-0004). Browser features that process a camera track in the web view
therefore cannot be used.

Background blur and background replacement need a person mask for each
picture. That needs a segmentation model. Nothing in `Cargo.lock` does
segmentation.

Facts on 26 November 2026:

* `tract-tflite` describes itself as "unimplemented". tract reads ONNX and
  NNEF. A TFLite model must be converted to ONNX before tract can load it.
* Google's MediaPipe Selfie Segmenter has a square (256x256) and a landscape
  (144x256) model. Its model card gives the licence as Apache-2.0.
* `onnx-community/mediapipe_selfie_segmentation_landscape` on Hugging Face
  has `onnx/model.onnx` (float32, 462 kB) and smaller float16 and quantized
  copies. It is tagged Apache-2.0. Its input is 256x144 pixels scaled to 0..1,
  and its output is called `alphas`. The page does not state the source file
  or the conversion tool.
* The owner did not yet allow the download of the model or of the tract
  crates. Nothing in this change measures tract, the model, or their speed.

Where do the effects run? How does the preview match what others see? How
does the client fail when an effect cannot run?

## Decision Drivers

* Others see exactly what the own preview shows.
* An effect that cannot run never sends the background that the user chose to
  hide.
* The page stays keyless and decodes nothing. The worker gets no new
  authority: no file, network or device access for the effects.
* Bounded memory and work per picture. No new thread and no new queue.
* Closed sets and bounded values, checked in every process that receives
  them.
* The same behaviour on Windows, Linux and macOS.

## Considered Options

* Process the I420 picture in the call worker, before the preview and the SFU
  get it.
* Process the pictures in the page.
* Use the operating system's camera effects only (Windows Studio Effects,
  macOS Video Effects).
* For segmentation: tract with an ONNX model embedded in the worker;
  ONNX Runtime through `ort`; own inference code for one fixed model.

## Decision Outcome

Proposed option: "Process the I420 picture in the call worker", with tract and
an embedded ONNX model as the later segmenter (owner choice).

### Effects

* **Background**: one of
  * none;
  * blur behind the person, with a strength from 10 to 100 percent in steps
    of 10 (owner choice: a slider). The page offers 50 percent first;
  * one of four pictures: Dusk, Sea, Forest and Studio. The worker draws them
    from code (gradients and soft shapes). They are not image files, so no
    image decoding and no picture licence is needed.
* **Colour filter**: none, greyscale, warm, cool or brighten (lifts dark
  parts of the picture).
* **Mirror own preview**: the page shows tile 0 mirrored, with CSS only.
  Others see the picture as it is. The page mirrors tile 0 already
  (`tileLayout` in `frontend/src/video.ts`), so the choice is on by default
  and the current preview does not change; the user can turn it off.
* The background applies first, then the colour filter. Both work on the I420
  picture after it is scaled to fit 640x360. The worker converts that same
  picture for tile 0 and sends it to the SFU.

### Segmenter

* `crates/call-worker/src/segmenter.rs` is the interface: one person mask for
  one picture, 0 for background and 255 for the person, at most 640x360.
  `effects.rs` scales the mask to the picture and composites. Neither has
  LiveKit or OS types.
* This change has no segmenter. `segmenter::available()` is `false`, so blur
  and pictures cannot run yet.
* `FromWorker::Devices` ends with `background_effects`. The page offers blur
  and pictures only when it is `true`. Otherwise it says why.

### Failing closed

* A camera start with an effect that needs the mask, while no segmenter is
  available, leaves the camera off. The worker reports
  `CameraState::EffectUnavailable`. Audio goes on.
* A `SetVideoEffect` that the worker cannot apply is refused. The old effect
  stays, and the worker answers `VideoEffect { effect: <old>, refused: true }`.
* For the later segmenter: a picture without a mask is dropped, so the remote
  members keep the last processed picture. After 15 pictures without a mask
  in a row (about one second) the worker stops the camera as for a camera
  failure, with `EffectUnavailable`.
* A picture larger than 640x360 is refused by the effects code itself. This
  repeats the scaling limit.

### Limits and cost

* Only the own camera picture is processed. Remote video and the screen share
  are not.
* The blur is a box blur of three passes in each direction, so its cost does
  not depend on the strength. The radius is `strength x width / 3200` pixels:
  2 to 20 pixels at 640 wide.
* The worker keeps its scratch planes and the drawn background for the
  current picture size, at most about 1.4 MB, and makes them again only when
  the size changes.
* The effect is shared with the capture thread through one atomic value. The
  frame callback reads it for each picture. No lock and no queue are added.

### Protocol (wire version 15)

* A video effect is three bytes: the background kind (0 none, 1 blur,
  2 picture), its value (0 for none, the strength for blur, the picture
  number 1 to 4) and the filter (0 to 4). Decoding refuses any other value.
* `Join` ends with the video effect.
* `ToWorker::TestDevice` ends with the video effect that a camera test starts
  with. The worker sets it before the camera opens and answers
  `FromWorker::VideoEffect` then. When the worker accepts it, the first
  picture has it; when the worker refuses it, the test starts without an
  effect (see below). A sound test must send no effect; decoding refuses
  another.
* `ToWorker::SetVideoEffect` (tag 19) changes it in a running call or in a
  camera test. The worker answers `FromWorker::VideoEffect` (tag 24) with the
  effect in use and whether the request was refused.
* `CameraState::EffectUnavailable` is 4.
* `FromWorker::Devices` ends with `background_effects` (one byte, 0 or 1).

### Core, host and page

* The effect and the mirror choice are part of the call device record
  (`persephone.call_devices.v1`, fields `effect` and `mirror`). A record
  without them reads as no effect and a mirrored preview.
* The command `call_video_effect` sets both. It saves both for later calls,
  then applies the effect to a running or joining call and to a running
  camera test. It refuses a value outside the closed sets, and a failed save
  changes nothing. The main window and the call window may call it; the call
  window's device panel (ADR-0065) shows the same controls as the settings.
* The core numbers each effect request. A join reads that number before it
  reads the record, and an effect asked after that read replaces the saved
  one, also when it was asked before the join took the call slot (for
  example during the proxy question). So no change is lost during a join.
* A camera test starts with the saved effect in `TestDevice`, so the settings
  preview never shows a picture without an effect that can run. Later
  changes go as `SetVideoEffect`. When the worker refuses the saved effect
  (it needs the person mask), the test shows the camera without an effect
  and its status says why. A join keeps the camera off in that case, because
  others would see the picture; the test publishes no track, its pictures
  go only to the local preview, and it is there to check the camera.
* The call status carries `effectRefused` (the worker refused the newest
  effect change of this call) and `mirror` (from the record at the join, then
  each change). The device test status carries `effectRefused` too, while a
  camera test runs. The worker answers after the command's reply, so the page
  shows a refusal from these statuses, never from the reply. The main window
  and the call window show the own tile from `mirror`, so neither reads the
  record during a call.
* The page offers blur and the pictures only when the device list's
  `backgroundEffects` is true. In this build it is false, and the page shows
  a note in their place. The call window's effect saves go into the same
  queue as its device saves, because both change the same record.

### Later step: the segmenter

Owner choice: tract with the MediaPipe Selfie Segmenter (landscape) in ONNX
form, embedded in the worker binary, so the worker opens no file for it. That
step needs, before any code:

* the owner's approval to download the model and the tract crates;
* the model's source: convert Google's file with a recorded tool and version,
  or record why the `onnx-community` copy is used, and pin its SHA-256;
* a measurement on each OS: model load time, operator support, and the time
  for one mask at 256x144 on the capture thread at 15 pictures per second;
* the tract dependency tree through the dependency audit and the licence
  list, and the model's Apache-2.0 notice in the third-party notices;
* a containment check on each OS: whether tract starts threads or makes
  calls that the call profiles refuse.

This record is then amended with the results.

### Owner choices

* Effects: background blur, bundled backgrounds, colour and light filters,
  and a mirrored own preview.
* Segmentation: tract with the MediaPipe model.
* Blur control: a slider.
* Control: saved with the call devices, and changeable from the call window.
* Downloads: not yet. Build the parts that need no model first, behind a
  segmenter interface.

### Positive Consequences

* Colour filters and the mirror work now on every OS that has a camera.
* The preview is the picture that others get.
* No new secret, process, thread or authority.

### Negative Consequences

* Blur and backgrounds do not work until the segmenter step.
* A new wire version. The core and the worker must come from the same build,
  as before.
* Processing adds work on the camera's capture thread for each picture.
* The effect is a privacy aid, not a security control. A compromised worker
  can send the camera without it. A blurred background can still show light,
  colours and large shapes, and the mask can miss parts of the background.
* Not verified live.

## Pros and Cons of the Options

### Process in the call worker

* Good, because the preview and the SFU get the same picture.
* Good, because the page and the core do not change their media handling.
* Bad, because the work adds to the worker's capture thread.

### Process in the page

* Bad, because the page does not have the camera stream (ADR-0048), and the
  SFU would still get the unprocessed picture.

### Operating system effects only

* Good, because no code is needed.
* Bad, because there is nothing on Linux, no control in the app, and the
  owner asked for effects in the app.

### tract with an embedded ONNX model

* Good, because it is pure Rust, builds from source, and needs no download at
  build time.
* Bad, because it adds a dependency tree to audit.

### ONNX Runtime through `ort`

* Bad, because it takes a prebuilt native library, and the build must never
  download native code.

### Own inference code

* Good, because it adds no crate.
* Bad, because it is a large amount of numeric code to write, test and keep.

## Links

* [ADR-0003](0003-run-untrusted-media-decoding-in-a-webassembly-sandbox.md)
* [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
* [ADR-0048](0048-add-camera-video-to-calls-in-the-call-worker.md)
* [ADR-0059](0059-test-call-devices-show-each-participants-level-and-switch-devices-during-a-call.md)
* [ADR-0060](0060-show-calls-in-a-separate-call-window.md)
* [ADR-0062](0062-set-the-microphone-gain-and-automatic-level-in-calls.md)
* [ADR-0065](0065-choose-the-large-video-and-change-devices-in-the-call-window.md)
* [MediaPipe image segmenter models](https://developers.google.com/edge/mediapipe/solutions/vision/image_segmenter)
* [onnx-community/mediapipe_selfie_segmentation_landscape](https://huggingface.co/onnx-community/mediapipe_selfie_segmentation_landscape)
