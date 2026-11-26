# Capture the Windows screen in a helper outside the call worker

* Status: proposed
* Deciders: project owner (this record needs the owner's approval)
* Date: 2026-11-25

Technical Story: the owner asked for a capture process outside the call
worker's sandbox that talks to the sandboxed worker. The reason: the
Windows capture that the worker uses today, GDI `BitBlt` inside the
AppContainer ([ADR-0053](0053-share-the-screen-in-calls-on-linux-and-windows.md),
amendment of 25 November 2026), draws no capture border and no cursor, and
nobody has measured whether it respects the capture exclusion of
[ADR-0045](0045-exclude-the-main-window-from-screen-capture-by-default.md).

If the owner accepts this record, it replaces the Windows capture parts of
the amendment of 25 November 2026 in ADR-0053: the capture method, the
"Limits of GDI" and the "Why this is not a containment loosening" items.
The rest of that amendment stays: the target
(`ScreenTarget::Monitor`), the core's window and its layout checks, the
stop rules, and the list rules. This record does not change macOS
([ADR-0064](0064-share-the-screen-in-calls-on-macos.md)) or Linux.

## Context and Problem Statement

Windows.Graphics.Capture (WGC) and DXGI Desktop Duplication fail with
`E_ACCESSDENIED` in the call worker's AppContainer. Only GDI copies the
screen there
([evidence](../evidence/screen-share-windows.md#measurement-of-25-november-2026-dxgi-and-gdi)).
GDI works, but it has these weaknesses:

* Windows draws no capture border. The call bar is the only sign that the
  screen is shared.
* GDI copies no cursor.
* ADR-0045 hides Persephone's windows with `WDA_EXCLUDEFROMCAPTURE`.
  Windows documents that flag for the capture APIs. Nobody measured it for a
  GDI copy of the desktop.
* The call worker has no Win32k lockdown, because its audio stack needs
  Win32k ([ADR-0043](0043-make-audio-calls-through-matrixrtc-in-a-contained-call-worker.md)). So the
  worker can copy any monitor with GDI at any time, with or without a
  share. The core only decides which monitor the worker is told about; it
  cannot stop the worker from copying another one.

Where can the capture run so that it uses WGC, and what does that cost in
trust and in code?

## Decision Drivers

* The user must see the Windows capture border, and the cursor must be in
  the picture.
* The documented `WDA_EXCLUDEFROMCAPTURE` behaviour of WGC must apply.
* The key-holding core must stay free of capture code
  ([ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md)),
  and the web view must never receive the screen.
* A process with the authority to read the screen should be small, have no
  network code and take no untrusted input.
* No new third-party crate. Every crate must still build on Linux and macOS.
* A failure must end the share and keep the call.

## Considered Options

* Keep GDI in the call worker (the amendment of 25 November 2026).
* Run WGC in the call worker with more capabilities.
* Capture in the core process.
* Capture in a helper process outside the AppContainer, one for each
  share, and give its frames to the worker over a pipe.

## Decision Outcome

Proposed option: "a helper process outside the AppContainer", because it is
the only option that uses WGC and keeps capture code out of the core. This
record is proposed. It needs the owner's approval before the project treats
it as accepted.

### The helper

* A new program, `persephone-screen-helper.exe` (crate
  `crates/screen-helper`), ships next to the core, as the call worker does.
  It holds no key, opens no network connection and reads no homeserver or
  peer data. Its only input is its command line and the end of its control
  pipe. On Linux and macOS the crate builds a stub that exits with an error.
* The core starts one helper for each share, after the user said Yes and
  the core read the layout again. The core starts it through
  `crates/media-broker` (`start_screen_helper`).
* The helper captures the one monitor that the user approved. It takes the
  monitor with the same device name, the same size and the same rectangle
  as the target; none, another size or another rectangle is "not found". It
  uses the existing WGC code (`wgc.rs`, moved from the call worker to the
  helper). The yellow capture border stays on and the cursor is captured.
  It checks the monitor's place on each loop, and a monitor that moved,
  changed or went away ends the share.
* A picture is BGRA, at most 3840x2160 pixels and 7680 on a side (the
  limits that the worker had). The helper copies at most 5 pictures a
  second. Windows gives a frame only when the screen changed, so the
  helper is built to keep a frame that comes too soon, in place of an
  older kept one, and to copy it when its time comes, so that the last
  change of a burst reaches the call. The worker does the same with the
  pictures it reads, a little slower than its own limit and counted from
  the end of the last hand-on, so that its limiter does not drop one.
  While the screen stays still, the worker hands the newest picture on
  again once a second: pictures that come before the track is published
  are dropped, and a still screen would otherwise send nothing after
  that. The worker's side is covered by unit tests; the helper's kept
  frame only by a unit test of its rule (`wgc::take_next`), see "Not
  verified".

### How the helper starts

* The helper runs with the user's normal token. WGC is refused in an
  AppContainer, so the helper cannot run in one.
* It starts suspended, in a Job with kill-on-close, one active process,
  a memory limit of 512 MiB, and UI limits (no clipboard, no system
  parameter or display setting change, no global atoms, no desktop switch,
  no log-off, no USER handles of other processes). The peak measured on
  25 November 2026 was 87 MiB for one 2560x1440 monitor.
* It gets the creation-time mitigations of the call worker that WGC and
  Direct3D allow: DEP, heap termination, bottom-up and high-entropy ASLR,
  strict handle checks, extension points disabled, font loading disabled,
  and the image-load limits (no remote images, no low-label images,
  System32 first). It also gets the child process policy.
* It has **no Win32k lockdown and no dynamic-code prohibition**. WGC and
  Direct3D need Win32k, and the graphics driver generates code in the
  process.
* Child processes are blocked twice: by the child process policy and by the
  Job's limit of one process. The helper has no console
  (`DETACHED_PROCESS`, and the Windows subsystem): a console program needs
  `conhost.exe` as a child process, and with the policy on, such a helper
  fails to start with `0xC0000142` (measured on 25 November 2026).
* It inherits only two handles, through an explicit
  `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`: the read end of its control pipe and
  the write end of its frame pipe. Its standard handles are null and its
  environment holds only `LOCALAPPDATA`, `SystemDrive`, `SystemRoot` and
  `windir`.
* No other process can inherit those pipe ends. They are not inheritable
  when they are made. The launch makes the helper's two ends inheritable
  only while it holds the core's process-start lock, and it closes them
  before it lets the lock go. Every process start of the core takes the
  same lock: the call and media workers, the UI host and a profile switch.
  A start without a handle list (the standard library's) would otherwise
  inherit every inheritable handle. The call worker's own pipes now follow
  the same rule.
* The target comes on the command line: the share number, the two handle
  values, the monitor's device name (as hexadecimal bytes), its size and its
  rectangle. It holds no secret. Both the core and the helper validate it
  with the same code (`persephone_call_protocol::screen_helper`).

### How frames reach the worker

* The core makes the frame pipe for each share. It keeps nothing of it
  except the read end, which it duplicates into the running call worker
  (`DuplicateHandle` with the process handle that the broker got when it
  started the worker), and then closes its own copy. Frames never pass
  through the core or the web view.
* The core tells the worker with `ToWorker::ScreenFrames { share, handle }`
  directly before the share's `SetScreen`. The call protocol stays at
  version 12, which has not been released.
* The worker checks that the handle is a pipe (`GetFileType`) before it
  reads, and never closes a handle that is not a pipe.
* The pipe carries the bounded frames of `crates/media-protocol`: one
  `Control` frame with a 23-byte message (version, tag, share, format,
  width, height, stride, length), then one pixel frame. The reader checks
  the share number, the format, the size, the stride (exactly `width * 4`)
  and the length (`height * stride`) before it allocates the body. A last
  `Control` frame says why the pipe ends (stopped, not found, unavailable).
* The worker reads with bounded waits. A helper that stalls inside a frame
  cannot hold the capture thread past a stop. A bad item, a closed pipe
  without an end item, or a stall ends the share as "unavailable". The
  worker feeds each good frame into the same `CapturedFrame` path that the
  other backends use, so publishing does not change.
* The worker no longer captures the screen on Windows. `CAPTURE_IN_CONTAINER`
  and the in-container GDI code are gone from it, and the worker's `windows`
  dependency lost its graphics features. The gate keeps its GDI and WGC
  measurements, which measure what the container can do. The worker gets no
  new capability and no relaxed mitigation.

### Stop and lifecycle

* "Stop sharing", the end of the call, the worker ending the share, the
  helper exiting and the core's session end all end the helper. The
  worker's stop is queued at once, as before.
* The core owns an explicit stop-and-join. It closes the helper's control
  pipe, waits up to 2 seconds for the exit, terminates the Job if the
  helper is still running, and joins the thread that waits for the process.
  The end of the call and the end of the session do this before they go on.
  They also cancel a share pick that still runs and wait for it (at most
  2 seconds), so a helper that it started is stopped and joined too. Then
  they wait until no helper that an earlier stop left to its reaper is
  active (at most 5 seconds). A pick looks at the cancel again just before
  it starts the helper, and from that look until the helper counts as
  active the launch counts as active too, in the same count, so the end of
  the call also waits for a launch that passed the look. Both waits are
  bounded, so a stuck launch cannot hold the end of the call or of the
  session; a launch that ends after them has no worker, and its pick
  stops the helper when it reads the cancel.
  A stop from the page closes the helper's control pipe at once, before
  the page shows "off", also when the worker's command queue is full and
  the call task is busy; the exit that follows is not taken as the
  helper's own end. The helper looks at the control pipe before each
  picture that it writes, so no new picture goes after it saw the stop; a
  picture that it is writing goes whole, so the worker reads only complete
  items. Pictures that were written before the stop was seen can still
  reach the call before the worker reads its own stop: "off" means that
  the capture was told to stop, as with the Linux portal, not that every
  sent picture was revoked. The call task then drops the share's helper, and a
  reaper thread does the wait, the terminate and the join. The
  next share starts its helper only after no earlier helper is active. A
  helper counts as active until its exit is seen. One that the terminate
  did not end keeps the next share from starting, and that share is
  "unavailable".
* The start takes the command queue slots of `ScreenFrames` and `SetScreen`
  together. A full queue fails the share, not the call, before the pipe
  goes into the worker.
* A helper exit maps to the share state: a missing or changed monitor is
  "not found", a refused or failed capture is "unavailable", and a normal
  stop is "off". A frame pipe that the worker closed is "unavailable": the
  core's own stop drops the helper first, and the exit of a dropped helper
  is not reported, so a reported closed pipe is the worker's own end. The helper says the same in its last pipe item, so the
  worker's state and the core's agree. A crash, a kill or an unknown exit
  code is "unavailable".
* Any failure ends the share and keeps the call. The core never falls back
  to capturing in the worker or in itself.

### Positive Consequences

* The picture has the WGC capture border and the cursor.
* The documented `WDA_EXCLUDEFROMCAPTURE` behaviour of WGC applies. This is
  not measured yet, so the share question keeps its warning that the shared
  screen can show Persephone's messages. It no longer says that Windows
  draws no border, because a WGC capture has one.
* The capture authority of a share sits in a small process with no network
  code and no untrusted input. The worker, which parses SFU data and media,
  no longer needs to read the screen to share it.
* Linux and macOS do not change.

### Negative Consequences

* **The worker still has no Win32k lockdown, so it can still copy any
  monitor with GDI, with or without a share.** The helper does not remove
  that ability. The gain of this record is the border, the cursor, the
  capture exclusion and the placement of the capture code. It is not a
  reduction of what a compromised worker can read. Only a Win32k lockdown
  for the call worker would do that, and it needs its own decision
  (the audio stack must stop needing Win32k first).
* There is a new process that is **not contained** and can read the whole
  screen with the user's token. It has the mitigations, the Job and no
  child processes, but it has Win32k and dynamic code. A flaw in the helper
  gives an attacker the screen and the rights of the user, and not only
  what an AppContainer gives. The helper's input is small (a validated
  command line and a pipe that only the core holds), and its code is small,
  but this is a real addition to the attack surface.
* There is a new frame channel and a new core ability: duplicating a handle
  into the worker process. A bug in the handle check could let the core
  give the worker a handle that is not a pipe. The worker takes a value
  only when it is a pipe end (`GetFileType` and `GetNamedPipeInfo`, which
  refuses a socket) and none of its standard handles, and it does not close
  what it refuses. These checks cannot prove that a pipe end is the frame
  pipe; the core names the handle that it duplicated just before. A value
  that names no handle ends the worker (strict handle checks), and so the
  call.
* There is more Windows code to review: the helper, its launch in the
  broker, the helper's lifecycle in the core, the frame format and the
  worker's reader.
* One more executable is built, signed, bundled and checked in the Windows
  release, and a missing or unsigned helper leaves sharing "unavailable".
* A helper starts for each share. The border can show for a moment before
  the worker takes its first frame.

### Not verified

* The helper and the worker were not run together in a call with a remote
  participant who sees the share. Each side was tested alone: the real
  helper writes a real, non-black picture to a pipe under its full launch
  (ignored test `platform::windows::screen_helper`), and the worker's reader
  takes good frames and refuses bad ones (unit tests).
* No run has read more than one picture from the real helper. Its kept
  frame over the rate gap, whether a kept frame and a frame being copied
  ever leave the two-buffer frame pool without a buffer, and the pacing of
  the helper and the worker together over many pictures are designed and
  unit tested, not measured. Each frame from the pool is closed when it is
  dropped (`wgc::PoolFrame`), so its buffer goes back on every path.
* Whether Persephone's windows are hidden from the helper's WGC picture
  (ADR-0045) was not measured. Until it is, the share question keeps its
  warning that the shared screen can show Persephone's messages.
* Multi-monitor setups, other DPI scales, protected content, and a locked
  or secure desktop.
* The Windows release workflow changes were not run in CI.
* The sandbox gate was not run for this change, and it does not measure the
  helper's launch.

## Pros and Cons of the Options

### Keep GDI in the call worker

* Good, because it is built, measured and needs no new process.
* Bad, because there is no capture border and no cursor, and the capture
  exclusion of ADR-0045 is not measured for it.

### Run WGC in the call worker with more capabilities

* Bad, because the gate measured WGC with `graphicsCaptureProgrammatic`,
  `graphicsCapture` and both: all failed with `E_ACCESSDENIED`. No
  capability the project may grant makes WGC work in the container.

### Capture in the core process

* Good, because it needs no new process and no frame channel.
* Bad, because the core holds the keys (ADR-0004). It would add Direct3D,
  WinRT capture and a large Windows surface to the process that must stay
  small, and the frames would pass through the core.

### A helper process outside the AppContainer

* Good, because WGC works, the border and cursor show, and the capture code
  is outside the core and outside the worker.
* Bad, because it adds an uncontained process and a frame channel, and does
  not remove the worker's ability to copy the screen with GDI.

## Links

* Replaces, if accepted, the Windows capture parts of the amendment of
  25 November 2026 in [ADR-0053](0053-share-the-screen-in-calls-on-linux-and-windows.md)
* Relies on [ADR-0004](0004-separate-the-key-holding-core-from-the-web-view-process.md),
  [ADR-0043](0043-make-audio-calls-through-matrixrtc-in-a-contained-call-worker.md) and
  [ADR-0045](0045-exclude-the-main-window-from-screen-capture-by-default.md)
* [Evidence](../evidence/screen-share-windows.md), including the helper
  measurement of 25 November 2026
