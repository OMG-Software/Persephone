# Share the screen in calls on Linux and Windows

* Status: proposed
* Deciders: project owner
* Date: 2026-11-19

Technical Story: on 19 November 2026 the owner asked for screen sharing in
calls. The owner chose Linux and Windows for this version, with macOS later,
and a warning on systems where the main window is not hidden from capture.

## Context and Problem Statement

Calls have audio and camera video
([ADR-0043](0043-make-audio-calls-through-matrixrtc-in-a-contained-call-worker.md),
[ADR-0048](0048-add-camera-video-to-calls-in-the-call-worker.md)). Media runs
in a contained call worker that holds no Matrix secret
([ADR-0049](0049-contain-the-call-worker-on-linux-and-macos.md)). The worker
already skips remote screen-share tracks. Screen sharing needs three things
that the camera does not: a source picker that shows the user's screen or
windows, a capture API that reads the screen, and pictures large enough to
read text. Where does each part run, and how is it bounded?

Facts from the research of 19 November 2026 (not measured on a running
system):

* LiveKit 0.11.0 publishes a screen share with `TrackSource::Screenshare`
  and `NativeVideoSource::new(resolution, true)`. Its default degradation
  for screen share keeps the resolution. A receiver tells a screen share by
  the publication's source, which the sender sets.
* Windows: Windows.Graphics.Capture (WGC) can capture a monitor through
  `IGraphicsCaptureItemInterop::CreateForMonitor`. The system picker needs a
  window, so the windowless worker cannot show it. WGC draws a yellow border
  by default. Whether WGC runs in the worker's non-packaged AppContainer, and
  which capability it needs there, is not known.
* Linux: the xdg-desktop-portal ScreenCast interface shows the system picker
  and gives a PipeWire remote and node IDs. This is the same route as the
  portal camera of ADR-0049.
* ADR-0045 hides the main window from capture on Windows. Linux has no such
  control, so a whole-screen share there can show Persephone's messages.

## Decision Drivers

* No Matrix secret and no new authority reach the worker. The worker never
  gets a "capture anything" right; each share is one grant from the core.
* The user picks every share. No remembered source, no silent restart.
* Bound the pictures and the work before allocation, as in ADR-0048.
* Fail closed where capture cannot run in the containment.

## Considered Options

* The core picks, the worker captures with the OS capture API
* libwebrtc's own `DesktopCapturer` in the worker
* Capture in the core or the host

## Decision Outcome

Chosen option: "The core picks, the worker captures with the OS capture
API", because it keeps capture and encoding in the contained worker and
keeps the choice in the process that owns authority.

### Scope

* Linux and Windows. macOS reports screen sharing as unavailable until its
  own decision and a measurement on a Mac.
* One screen share for each call from this device. It shares a whole
  screen, or a window where the portal offers it. Audio of the shared
  screen is not captured.

### Starting and stopping

* A "Share screen" button in the call bar asks the core. The page sends no
  source and no handle.
* Windows: the core shows its own window with the screens, named "Screen 1",
  "Screen 2" and so on, with their sizes. The user picks one or cancels. The
  core passes the chosen monitor to the worker as an index in display order
  (`EnumDisplayMonitors`) and the monitor's size in pixels. The worker reads
  its own list when the share starts. A screen that is gone, or that has
  another size at that index, ends the share with "not found".
* Linux: the core first shows a warning in its own window: the shared screen
  can show Persephone's messages, because Linux does not hide the window
  from capture. On "Share", the core runs the ScreenCast portal
  (`CreateSession`, `SelectSources` with monitor and window types,
  `persist_mode` 0, no restore token, embedded cursor where offered,
  `Start`, `OpenPipeWireRemote`). It keeps the portal session open for the
  whole share and closes it when the share ends.
* The core sends the PipeWire remote over the camera channel of ADR-0049.
  The message's one data byte names the kind: `c` for camera, `s` for
  screen. Each kind has its own queue of at most four descriptors. A byte of
  any other value closes the channel. The command carries the node ID.
* A refused or cancelled picker leaves the share off. It never falls back
  to another source.
* "Stop sharing", the end of the source, a closed portal session, or the end
  of the call stops capture and unpublishes the track.
* A screen share problem never ends the call. The worker reports a closed
  screen state (`off`, `starting`, `on`, `not_found`, `unavailable`,
  `refused`), like the camera state.

### Windows capture

* The worker uses WGC through the `windows` crate (already locked at
  0.62.2), with only the features the capture needs. No new crate version.
* The yellow capture border stays on.
* Before the first share, a test in the native containment gate
  (`tools/dev.py sandbox`) measures WGC inside the worker's AppContainer. The
  worker's AppContainer gains only the capability that measurement shows to
  be needed, and the evidence records it. If WGC cannot run in the
  AppContainer, Windows reports screen sharing as unavailable. No mitigation
  is relaxed for screen sharing without a new decision.
* Frames are BGRA. The worker converts them to I420 with the libyuv
  functions in libwebrtc.
* Measured on 19 November 2026
  ([evidence](../evidence/screen-share-windows.md)): `CreateForMonitor` fails
  with access denied in the worker's AppContainer, with its capabilities and
  with `graphicsCaptureProgrammatic` and `graphicsCapture`. The worker gets no
  new capability. Windows reports screen sharing as unavailable, the core shows
  no Windows chooser, and the capture code stays for the gate's probe.

### Linux capture

* The worker reads the PipeWire stream with the existing `pipewire` crate
  code of the portal camera, shared-memory buffers only. Formats are BGRx,
  RGBx, BGRA and RGBA; others are refused.
* The Landlock and seccomp policy of ADR-0049 does not change. The worker
  does not talk to D-Bus.

### Publishing

* The worker scales the capture to fit 1920x1080, keeps the aspect ratio,
  and publishes one VP8 layer at up to 5 frames per second and at most
  1.5 Mbit/s, with `TrackSource::Screenshare` and the screencast flag. No
  simulcast.
* The worker sets the track's frame cryptor to the current own key index
  before the first frame, as for every own track. `SetOwnKey` updates it.
* There is no own preview picture. The call bar shows that the screen is
  shared.

### Receiving

* The worker decodes at most one remote screen share at a time: the first
  that a member of the LiveKit room publishes. Others show as "is sharing" on
  that member's tile and are not decoded. When the decoded one ends, the
  longest-waiting one takes its place.
* The screen share has its own tile, tile 5. `VIDEO_TILES` becomes 6.
* A screen picture is at most 1280x720 (921,600 pixels, 3,686,400 bytes of
  RGBA) and at most 5 pictures per second. Camera tiles keep the ADR-0048
  limits. Each limit is checked by tile index before allocation, in the
  worker, the core and the page.
* The core keeps the newest picture of each tile, so at most about 8.3 MB in
  all. The call protocol wire version becomes 4.
* A track is a screen share only when its source says so. That label comes
  from the sender and is only a display hint.

### Session lifecycle

* Unchanged from ADR-0043 and ADR-0048. Lock, sign-out, an ended session and
  exit kill the worker at once. The core closes the portal session and drops
  the pictures with the call.

### Positive Consequences

* Screen sharing adds no secret to any process, and the page still decodes
  nothing.
* The user picks each share in a window that the core or the OS draws.
* On Windows, the main window stays out of the shared screen (ADR-0045).

### Negative Consequences

* On Linux the shared screen can show Persephone's messages. The warning
  says so, but the content is shared if the user goes on.
* Windows screen sharing depends on a measurement that is not done yet. It
  can end as "unavailable".
* A compromised worker can capture the chosen screen for as long as the
  share runs. It cannot pick another source on Linux. On Windows it can
  open another monitor while it holds the AppContainer capability.
* The screen picture path raises the core's picture memory to about 8.3 MB
  and the pipe rate by up to about 18 MB/s.
* Remote screen shares are shown at most at 1280x720 and 5 pictures per
  second.
* X11 sessions without a ScreenCast portal backend cannot share.
* No macOS support.
* Not verified live.

## Pros and Cons of the Options

### The core picks, the worker captures with the OS capture API

* Good, because the worker gets one grant for each share.
* Bad, because each OS needs its own capture code and containment check.

### libwebrtc's own `DesktopCapturer` in the worker

* Good, because it is one cross-platform API in the existing library.
* Bad, because on Linux it needs the D-Bus session bus and a GLib loop in
  the worker, which the ADR-0049 policy denies, and its Windows path is not
  known to work in an AppContainer.

### Capture in the core or the host

* Good, because those processes have windows and no AppContainer.
* Bad, because the key-holding core or the web view host would encode
  untrusted-path media and hold capture authority, against ADR-0004.

## Links

* [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)
* [ADR-0043](0043-make-audio-calls-through-matrixrtc-in-a-contained-call-worker.md)
* [ADR-0045](0045-exclude-the-main-window-from-screen-capture-by-default.md)
* [ADR-0048](0048-add-camera-video-to-calls-in-the-call-worker.md)
* [ADR-0049](0049-contain-the-call-worker-on-linux-and-macos.md)

## Amendment of 2026-11-25: Windows captures with GDI

Status of this amendment: proposed, needs owner approval.

The measurement of 19 November tried only Windows.Graphics.Capture. On
25 November the gate also measured DXGI Desktop Duplication and GDI `BitBlt`
inside the call AppContainer, with the call launcher's own capabilities
(`internetClient`, `microphone`, `webcam`) and mitigations
([evidence](../evidence/screen-share-windows.md#measurement-of-25-november-2026-dxgi-and-gdi)):

* WGC `CreateForMonitor` and DXGI `DuplicateOutput` fail with
  `E_ACCESSDENIED`.
* GDI `GetDC(NULL)` and `BitBlt` copy a real picture (its mean colour equals
  the control's, so it is not a blank copy).

Decision:

* The worker captures a monitor with GDI (`screen/gdi.rs`). It adds no
  capability and relaxes no mitigation. The "Windows capture" rules above
  that say Windows reports screen sharing as unavailable no longer apply.
  The WGC code stays, because the gate measures it.
* The core shows its own window for each monitor (a message box per monitor:
  Yes shares, No shows the next one, Cancel stops; No is the default), as the
  "Starting and stopping" section says. When the call ends while a box is
  open, a thread timer on the asking thread ends the box's loop within about
  100 ms (a quit message, the way Windows documents to time out a message
  box), and the pick is cancelled
  ([measured](../evidence/screen-share-windows.md#message-box-cancel-25-november-2026)).
  When that timer cannot start, no box
  opens and the pick is cancelled: a box that the end of the call cannot
  close is never shown.
* The target is no longer the index in display order. It is
  `ScreenTarget::Monitor { name, width, height, rect }`: the monitor's GDI device
  name (`MONITORINFOEXW::szDevice`, such as `\\.\DISPLAY1`, printable ASCII,
  at most 32 characters), its size in pixels when the user said Yes, and its
  rectangle on the virtual screen in physical pixels (call protocol version
  12). The worker takes only the listed monitor with that name, size and
  rectangle; none, another size or another rectangle is "not found". An index could name
  another monitor of the same size after a monitor was removed or the order
  changed while the question was open.
* Windows documents `szDevice` only as the monitor's device name, not as
  stable across display changes; after a change it can give a name to
  another output. So the core reads every monitor's device name, rectangle
  and display mode before the first question and again after a Yes. Any
  change voids the Yes, and the share stays off (cancelled). For the moment
  between that second read and the worker's open, the target carries the
  rectangle from the second read: the core and the worker both read it with
  `GetMonitorInfoW` on a thread set to per-monitor DPI awareness, and the
  worker copies exactly that rectangle. If a name moved to another output
  in that moment, the worker still copies only the part of the desktop the
  user approved, or finds no monitor with both the name and the rectangle
  and shares nothing.
* The GDI copy reads a rectangle of the whole desktop. Before and after each
  copy the worker reads the monitor's name and rectangle again
  (`GetMonitorInfoW`, the same reading as the list's); a monitor that
  moved, changed or went away ends the
  share as "off" (the source ended), and the copy made while it changed is
  dropped. The copy is that rectangle, not the display mode's size (the two
  can differ), so it never reaches past the monitor and a monitor whose
  mode is larger than its rectangle can still be shared. The device name
  names the display output: a different monitor
  plugged into the same output, at the same size and place, counts as the
  same screen.
* The core lists monitors with the worker's list rule, but it asks only about
  a monitor that the GDI capture takes (`screen_capture_size_ok` in the call
  protocol, which the worker also uses: at least 2x2, at most 7680 on a side
  and 3840x2160 pixels). A monitor whose device name is not printable ASCII
  is not listed. The question numbers the offered monitors ("Screen 2 of 2")
  and says how many it left out. With none that fits, no window opens and the
  share is "unavailable".
* The question says that the shared screen can show Persephone's messages,
  as on macOS, because the display affinity of ADR-0045 was not measured
  against a GDI copy (see below).
* Limits of GDI: Windows draws no capture border and no cursor into the
  picture, the copy is the whole monitor (no window share), and the buffer is
  at most 3840x2160 pixels and 7680 on a side. The core does not offer a
  larger monitor, and the worker refuses one at the start ("unavailable")
  instead of a share that sends no picture. The capture
  thread sets per-monitor DPI awareness so that the monitor list and the copy
  use physical pixels; this needs the `Win32_UI_HiDpi` feature of the locked
  `windows 0.62.2` (no new crate, no `Cargo.lock` change).
* Why this is not a containment loosening: a process in this AppContainer
  could already copy the screen with GDI before this change. The worker does
  not use Win32k lockdown (`LaunchSpec::call()` turns it off, because the audio stack needs it). Screen sharing only
  uses an existing ability, and the core still grants one monitor for each
  share. Turning on the Win32k lockdown for the call worker would stop this
  capture and also close that ability; it needs its own decision.
* The call bar shows that the screen is shared, because Windows shows no
  border for a GDI copy. ADR-0045 hides the Persephone main window from
  capture; whether that hides it from a GDI copy was not measured.
* Not verified live: a share seen by another call member, a multi-monitor
  setup (the machine had one monitor), a monitor of another DPI scale, a
  protected (DRM) window, and a locked or secure desktop.
