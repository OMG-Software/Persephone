# Share the screen in calls on macOS

* Status: proposed
* Deciders: project owner
* Date: 2026-11-25

Technical Story: [ADR-0053](0053-share-the-screen-in-calls-on-linux-and-windows.md)
left macOS for a later decision and a measurement on a Mac. The owner asked
for screen sharing on macOS with ScreenCaptureKit.

## Context and Problem Statement

Calls share the screen on Linux through the ScreenCast portal. The worker
captures, the core picks ([ADR-0053](0053-share-the-screen-in-calls-on-linux-and-windows.md)).
macOS has no portal. Its screen capture API, ScreenCaptureKit, needs the
Screen Recording permission of the user (TCC) and, for a contained process, a
few system services. Where does each step run, and how is it bounded?

Facts from the research of 25 November 2026 (not measured on a Mac):

* ScreenCaptureKit (macOS 12.3 and later; the project's minimum is macOS 13)
  lists the displays with `SCShareableContent`, captures one display with an
  `SCContentFilter` and delivers pictures to a sample handler on a dispatch
  queue. Its pixel format and size are set in `SCStreamConfiguration`, and it
  scales to that size.
* `CGPreflightScreenCaptureAccess` reads the Screen Recording permission.
  `CGRequestScreenCaptureAccess` shows the system prompt and returns `false`
  at once; the user allows the app in System Settings, and the app may need a
  restart before it counts.
* The capture shows the system's own recording indicator. The user can stop a
  share from it; the stream then ends with the error `-3817`.
* Persephone hides no window from screen capture on macOS (ADR-0045 covers
  Windows only). Whether ScreenCaptureKit honours a window's sharing type is
  not known, and this record does not rely on it.

## Decision Drivers

* The worker gets one grant for each share and no listing right. The user
  picks every share.
* Bound the pictures and the work before allocation, as in ADR-0048 and
  ADR-0053.
* Fail closed where the permission, the display or the containment is not
  there.
* No new authority for the worker beyond what ScreenCaptureKit needs.

## Considered Options

* The core asks for the permission and picks the display; the worker captures
  it with ScreenCaptureKit
* The system picker (`SCContentSharingPicker`) in the worker
* Capture in the core

## Decision Outcome

Chosen option: "The core asks for the permission and picks the display; the
worker captures it with ScreenCaptureKit", because it keeps the choice in the
process that owns authority and the capture and encoding in the contained
worker, as on Linux.

### Scope

* macOS 13 and later, whole displays only. No window share and no audio of
  the shared screen. One share for each call from this device.
* The worker does not ask for the permission and never shows a window.

### Starting and stopping

* The "Share screen" button asks the core. The page sends no source.
* The core checks `CGPreflightScreenCaptureAccess`. If the permission is
  missing it calls `CGRequestScreenCaptureAccess` once. If that is also
  `false`, the share is `refused`, and the page shows: "Your system did not
  allow the screen share. On macOS, allow Persephone in System Settings,
  Privacy & Security, Screen Recording." The user starts the share again
  after the change.
* With the permission, the core reads `CGGetActiveDisplayList` (at most 16
  displays) and each display's size in points (`CGDisplayBounds`). It asks one
  Yes or No question for each display in its own alert, the main display
  first. The text names the screen, its size, that everyone in the call can
  see it, and that it can show Persephone's messages. "No" goes to the next
  display; "No" on the last one cancels. No display question runs without the
  permission.
* The core sends the target `ScreenTarget::Display { id, width, height }`:
  the display's CoreGraphics ID (`CGDirectDisplayID`, never 0) and its size in
  points when the user said Yes. This adds a target kind to the call protocol
  (version 12).
* The worker reads `SCShareableContent` when the share starts and takes only
  the display with that ID. A display that ScreenCaptureKit does not list, or
  another size, is `not_found`. It never takes another display, also when the
  displays change while the question is open. A refusal by the system
  (`-3801`) is `refused`; any other failure is `unavailable`.
* "Stop sharing", the end of the call, a stop from the system's indicator
  (`-3817`, the share is `off`) or any other stream error (the share is
  `unavailable`) stops the capture and unpublishes the track. A screen share
  problem never ends the call.
* As on Windows (ADR-0053's amendment), the core queues the worker's stop
  when it accepts "Stop sharing", before the call bar shows `off`, not after
  the call's other work.

### Capture

* One `SCContentFilter` for the display with no window left out, one
  `SCStreamConfiguration`: BGRA, at most 1920x1080 (the display's size in
  points, scaled down and never up, aspect ratio kept, even sides), one frame
  every 200 ms, the cursor drawn in, no audio, no microphone, a queue depth
  of 3. The microphone switch (`captureMicrophone`) exists from macOS 15; it
  is set only there (`objc2::available!`), because an older system raises an
  exception for the unknown selector and never captures the microphone in a
  stream.
* The sample handler runs on its own serial queue. It drops a picture with no
  image buffer, in another pixel format, with planes, larger than 1920x1080,
  with a size or stride that `CapturedFrame::expected_len` refuses, or less
  than 150 ms after the last one, all before it reads a pixel. It locks the
  pixel buffer and passes the bytes to the callback without a copy; the
  callback converts them as the Windows and Linux backends do.
* Stop sets the stop flag, stops the stream (waiting at most 1.5 s for its
  answer), removes the output and waits until the queue is idle. The existing
  `CameraGate` and `Capture` deadlines apply.
* There is no own preview.

### Containment

* The call Seatbelt profile (ADR-0049) gains only three Mach lookups
  (`com.apple.replayd`, `com.apple.windowserver.active`,
  `com.apple.CoreDisplay.master`) and one I/O Kit class
  (`IOSurfaceRootUserClient`). The list is marked "to be measured on a Mac",
  like the existing lists: a missing service fails the share closed, and an
  unneeded one is removed after the measurement. No file write, no process
  start and no Apple Events are added. The profile does not stop a window
  listing: the `SCShareableContent` that the worker reads to find its display
  also lists the windows and applications on screen. The worker reads only
  its displays.
* The Screen Recording permission belongs to the app that the system holds
  responsible for the worker. Whether a sandboxed helper that has no bundle of
  its own is covered by the permission of Persephone is not known.

### Dependencies

* New crate: `objc2-screen-capture-kit` 0.3.2 (licence Zlib OR Apache-2.0 OR
  MIT, on the list in `tools/about.toml`), the binding of the same project and
  version family as the locked `objc2` 0.6.5, `objc2-foundation` 0.3.2,
  `objc2-core-media` 0.3.2 and `objc2-av-foundation` 0.3.2. macOS target of the
  call worker only, `default-features = false`, features `std`, `block2`,
  `dispatch2`, `objc2-core-graphics`, `objc2-core-media`, `SCShareableContent`
  and `SCStream`. `Cargo.lock` gains this one package and one dependency line;
  every other locked version stays.
* No other new crate. `objc2-core-graphics` is already locked and is enabled
  only for `CGDirectDisplayID`. The few CoreGraphics functions
  (`CGPreflightScreenCaptureAccess`, `CGRequestScreenCaptureAccess`,
  `CGGetActiveDisplayList`, `CGMainDisplayID`, `CGDisplayBounds`) are declared
  with `extern "C"` in the core and the worker, as the idle time is
  (ADR-0025).

### Positive Consequences

* macOS users can share a display. The core and the page learn nothing about
  the picture, and no new secret reaches the worker.
* The pure rules (size match, output size, frame bounds, rate gap, error
  causes, the core's flow and its display target) have unit tests that run on
  every OS.

### Negative Consequences

* On macOS the shared screen can show Persephone's messages. The question
  says so, but the content is shared if the user goes on.
* The user answers a question for each display, in order, with no preview. A
  list with a picture is not built.
* Displays are named by size in points. On a Retina display the capture is at
  most the size in points (for example 1440x900), not the physical pixels, so
  small text is softer than the screen.
* The first ask for the permission always ends in `refused`; the user must
  start the share again, and macOS may need Persephone restarted.
* The core and the worker read the displays in two calls. A display that is
  gone or has another size in between gives `not_found`. The display ID keeps
  another display of the same size from taking its place.
* The stop from the system's indicator and the worker's end report are not
  tested on a Mac.

### Not compiled or run on a Mac

This record was written on Windows. The macOS code was not compiled with the
full worker and was not run on a Mac. It was type-checked and linted for
`aarch64-apple-darwin` and `x86_64-apple-darwin` in a separate crate that
includes the new files with small stubs for crate types (the Matrix
dependencies and libwebrtc need a Mac toolchain). The permission prompt, the
capture, the display order, the Retina sizes, the stop paths and the Seatbelt
list are all unverified. A Mac measurement must run before this record is
accepted: share each display, deny the permission, allow it and restart,
stop from the indicator, unplug a display during a share, and record which
Seatbelt denials the system log shows.

## Pros and Cons of the Options

### The core asks for the permission and picks the display; the worker captures it with ScreenCaptureKit

* Good, because the worker gets one display and no listing right.
* Good, because it reuses the Windows target and the Linux flow.
* Bad, because the pick is a plain question for each display.

### The system picker (`SCContentSharingPicker`) in the worker

* Good, because the system draws the picker and offers windows.
* Bad, because the contained, windowless worker cannot show it, and the
  picker needs more services than the profile may allow.

### Capture in the core

* Good, because the core has windows and no Seatbelt profile.
* Bad, because the key-holding core would hold the capture and the encoding
  (ADR-0004, ADR-0043).

## Links

* Refines [ADR-0053](0053-share-the-screen-in-calls-on-linux-and-windows.md)
* Uses the containment of [ADR-0049](0049-contain-the-call-worker-on-linux-and-macos.md)
* Related: [ADR-0011](0011-macos-platform-layer.md), [ADR-0045](0045-exclude-the-main-window-from-screen-capture-by-default.md)
