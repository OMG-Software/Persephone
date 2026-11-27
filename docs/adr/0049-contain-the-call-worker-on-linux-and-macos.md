# Contain the call worker on Linux and macOS

* Status: proposed
* Deciders: project owner
* Date: 2026-11-17

Technical Story: on 17 November 2026 the owner asked to connect Linux and
macOS to the camera interface of ADR-0048 and to let calls run on both. The
owner chose the PipeWire camera portal with a V4L2 fallback on Linux, and
calls on both systems, not camera backends alone.

## Context and Problem Statement

ADR-0043 refuses calls on Linux and macOS "until each OS has its own reviewed
profile and recorded checks". The media worker's profiles (ADR-0010,
ADR-0011) allow almost nothing: no files, no network, no devices. A call
worker needs the network, the sound server, the camera, name resolution and
the TLS root certificates. How is the call worker contained on each system,
how does it reach the camera, and what does it still reach that the
Windows AppContainer would deny?

## Decision Drivers

* No Matrix secret enters the call worker (ADR-0043, unchanged).
* Fail closed: the core refuses a call when a control is missing.
* No root rights, setuid helper or privileged daemon (ADR-0010).
* The worker reads hostile media only after its containment is complete.
* A camera is used only when the user turns it on (ADR-0048).

## Considered Options

* Linux: seccomp and Landlock applied by the worker; namespaces
  (bubblewrap); a seccomp allowlist only, as for the media worker.
* macOS: a call-specific Seatbelt profile applied by the worker; an XPC
  service with App Sandbox entitlements.
* Linux camera: the PipeWire camera portal; direct V4L2; both, with V4L2
  only when no portal runs.

## Decision Outcome

Proposed options: "seccomp and Landlock" on Linux, "a call-specific
Seatbelt profile" on macOS, and "the portal, with V4L2 only when no portal
runs" for the Linux camera.

### Linux: launch

* The broker starts the call worker as it starts the media worker
  (ADR-0010): a new session, every descriptor above 2 closed, a parent-death
  `SIGKILL`, `no_new_privs`, no core dumps, files of at most 256 MiB
  (`RLIMIT_FSIZE`), at most 256 descriptors and an address-space limit of
  4 GiB. The address-space limit is a bound, not the 512 MiB of the Windows
  Job: libwebrtc reserves much more address space than it uses. The file
  limit is not 0 (amended 19 November 2026): the sound server's client
  library sizes its 64 MiB shared-memory pool, a memfd, with `ftruncate`,
  and Linux counts memfd and `/dev/shm` files against `RLIMIT_FSIZE`. With
  0 the kernel killed the worker with `SIGXFSZ` before it reached the sound
  server. Landlock gives the worker no writable path (without V4L2), so
  the only files it can grow are memfds, which go away with the worker.
  The limit is per file: while the worker runs, it can hold up to 256
  memfds of 256 MiB each in memory. This is not bounded in total, as CPU
  use is not.
* Descriptor 3 is one end of a Unix socket pair, the camera channel. It is
  the only extra descriptor.
* The environment holds only `XDG_RUNTIME_DIR` and `HOME` (the sound
  server's socket and cookie), `LANG=C`, and for a call with a chosen
  microphone or speaker `PULSE_SOURCE` or `PULSE_SINK` (see "Linux:
  microphones and speakers"). It has no `DBUS_SESSION_BUS_ADDRESS`.
* One argument, `--camera=portal`, `--camera=v4l2` or `--camera=none`,
  tells the worker which camera path it may use (see "Linux: camera"). The
  worker refuses any other argument.

### Linux: containment

The worker applies two stages itself.

* **Stage 1, before it reads any input:**
  * `PR_SET_DUMPABLE` 0. The core stays dumpable (ADR-0046); the worker
    calls no portal, so the portal problem of ADR-0046 does not apply.
  * Landlock, ABI 3 or newer (Linux 6.2), handling a fixed set of file
    system rights: the ABI 1 rights (bits 0 to 12), `REFER` (ABI 2),
    `TRUNCATE` (ABI 3) and, from ABI 5, `IOCTL_DEV`. The code knows the
    rights up to ABI 7. A right that a newer ABI adds stays unhandled and
    restricts nothing, so it cannot block the sound server's socket by
    surprise. Read and execute: `/usr`, `/lib`, `/lib64`, `/etc`,
    `/run/systemd/resolve`, `/proc/self`. Read: `$HOME/.config/pulse`,
    `$XDG_RUNTIME_DIR/pulse`, `/dev/urandom`, `/dev/null`, `/dev/shm`
    (amended 19 November 2026: read only, so no file the worker writes
    outlives it; the sound server's pool may be a file there, the worker's
    pools are memfds, and recording, playback and the PipeWire camera
    worked without write access). The client library opens `$XDG_RUNTIME_DIR/pulse` and checks
    its owner before it connects to the socket in it (amended 19 November
    2026: without the rule the check failed and no sound server was
    reached). The rest of `$XDG_RUNTIME_DIR` (the session bus, PipeWire and
    other sockets) gets no rule. With
    `--camera=v4l2` only: read and write on the `/dev/video*` nodes (and
    their device ioctls, ABI 5 or newer). No other path. Where the ABI
    has them, abstract Unix sockets and signals are scoped to the worker
    (ABI 6).
  * A seccomp filter that kills the process on: `execve`, `execveat`,
    `ptrace`, `process_vm_readv`, `process_vm_writev`, `kill`,
    `pidfd_open`, `pidfd_getfd`, `pidfd_send_signal`, the mount and
    namespace calls (`mount`, `umount2`, `move_mount`, `fsmount`,
    `fsopen`, `open_tree`, `pivot_root`, `chroot`, `unshare`, `setns`),
    `bpf`, `perf_event_open`, `keyctl`, `add_key`, `request_key`,
    `userfaultfd`, `io_uring_setup`, `io_uring_enter`, `io_uring_register`,
    `open_by_handle_at`, `name_to_handle_at`, module, `kexec`, `reboot`,
    `swapon`, `swapoff`, `acct`, `quotactl` and `personality` calls,
    `clone` with any `CLONE_NEW*` flag, and `socket` with a family other
    than `AF_UNIX`, `AF_INET`, `AF_INET6` and `AF_NETLINK`. `clone3`
    returns `ENOSYS`, so that the C library falls back to `clone`, whose
    flags the filter can read.
  * The same filter kills on `tgkill`, `rt_tgsigqueueinfo` and
    `rt_sigqueueinfo` when the target thread group is not the worker's own
    process. `tkill` returns `ENOSYS`, because a bare thread ID cannot be
    tied to the worker. Landlock ABI 3 to 5 has no signal scope, and the
    worker runs with the core's user ID, so without this rule the worker
    could signal the core. The rule applies on every ABI.
  * Then the worker reports `Ready`. Its containment bits say which control
    is missing.
* **Stage 2, after the sound server connection and before
  `Room::connect`:** a second seccomp filter that kills on `landlock_*`,
  and refuses `socket` and `socketpair` with `AF_UNIX` with `EACCES`. The
  refusal is not a kill because glibc's name lookup first tries a Unix
  socket (nscd, and on Ubuntu Avahi for `mdns4_minimal`) and goes on to DNS
  when that fails; a kill would end every call at the SFU lookup. Either
  way, no new Unix socket exists after stage 2. It cannot stop a
  `connect` on a Unix socket made before it, so the worker makes none
  except the sound server's. The PipeWire descriptor of the portal path
  arrives already connected over the camera channel, so it needs no new
  socket. The worker sends nothing to the SFU before
  stage 2 is in place, so remote media is read only under both stages. A
  stage 2 that cannot be installed fails the call with `Containment`.
* The broker sends input only when `/proc/<pid>/status` shows
  `NoNewPrivs: 1`, `Seccomp: 2` and more filters than the broker's own
  thread, as for the media worker. Landlock cannot be read from outside; the
  worker's own bit is the evidence for it.
* A kernel without Landlock ABI 3, or with Landlock turned off, refuses
  calls. This includes the stock Debian 12 kernel (6.1).
* This is a deny list, not the allow list of the media worker. libwebrtc,
  the sound server's client library and PipeWire make too many system calls
  for an allow list that would not break with a library update. Landlock is
  the file boundary; the deny list removes process, kernel and namespace
  access.

### Linux: camera

* The core decides the camera path before it starts the worker:
  * `portal` when the session bus has `org.freedesktop.portal.Desktop` with
    the `org.freedesktop.portal.Camera` interface;
  * `v4l2` when it has no such portal (for example a window manager without
    `xdg-desktop-portal`);
  * a portal that refuses access is not a missing portal. The camera then
    stays off with `Unavailable`. There is no fallback after a refusal, and
    the core sends no `SetCamera` for it.
* Portal path: when the camera turns on (or a list of cameras is asked
  for), the core calls `AccessCamera` and waits for its `Response` for at
  most 60 seconds. On success it calls `OpenPipeWireRemote` and sends the
  returned descriptor to the worker over the camera channel (`SCM_RIGHTS`,
  one descriptor and one byte). It then sends `SetCamera { on: true,
  remote: true }` to turn the camera on, or `ListDevices` to list the
  cameras. The worker connects to that PipeWire remote, which shows only
  the camera nodes the portal allows. It never opens a device node or the
  session bus.
* Every camera-on on the portal path carries its own remote. The worker
  keeps the descriptors it has not taken in the order they came, at most
  four, and a camera-on with `remote: true` takes the oldest with the
  command. A camera-on that opens nothing closes its own remote; one
  without `remote` never reads the channel. One descriptor more than four
  closes all of them, and the channel gives out none after that.
* Listing on the portal path asks the portal for access, so the desktop may
  show its permission dialog. Without access, no cameras are listed;
  microphones and speakers still are.
* V4L2 path: the worker opens `/dev/video*` itself. It lists only nodes with
  the video-capture capability; it does not stream to list them.
* Formats: NV12, I420 and YUYV only, at most 1280x720. The worker converts
  YUYV to I420. It decodes no MJPEG. PipeWire buffers must be shared memory
  (`MemFd` or `MemPtr`); DMA buffers are refused.
* Camera IDs: each ID starts with its path, `pipewire:` or `v4l2:`. After
  `pipewire:` comes the PipeWire `node.name`. After `v4l2:` come the
  driver, card (model name) and bus information of the node, joined with
  `|` (a `|` or `\` inside a field is escaped); a node without one of these
  fields is not listed. The bus information alone names a port, not a
  device, so it is not the ID by itself: a camera of another model or
  driver in the same port does not match a saved choice. A saved camera
  that does not match stays off with `NotFound`. So does a camera chosen on
  the other path, or saved without a prefix: the worker never reads a
  `node.name` as V4L2 information or the reverse. The worker never takes
  another camera in its place.
* Assumption: no released build saved a Linux camera choice without a
  prefix. Released builds before the prefixes had no camera support and
  refused calls on Linux. Profiles from development builds made before the
  prefixes are not supported; choose the camera again in them. If an
  unprefixed format is ever released, it needs its own format marker.
  Reading an old choice by its first characters alone is not enough,
  because a `node.name` could begin with `v4l2:`.
* Limit: the V4L2 ioctls on `/dev/video*` give no serial number, so two
  cameras of the same model swapped in the same port have the same ID. A
  serial number would need the media controller (`/dev/media*`) or sysfs,
  which the Landlock policy does not allow.

### macOS: launch and containment

* The broker starts the call worker as it starts the media worker
  (ADR-0011): a new session, every descriptor above 2 closed, no core dumps,
  no file writes. macOS does not enforce an address-space limit.
* Before it reads input, the worker applies a call profile with
  `sandbox_init`. It denies by default and allows only: reading the system
  frameworks and libraries, `/private/etc` (name resolution and
  certificates) and the audio and camera plug-in folders; outgoing network
  connections, binding local UDP and TCP sockets, and incoming UDP on them
  (ICE answers arrive on the bound socket); the microphone and
  camera devices; the I/O Kit and Mach services that Core Audio, Core Media
  IO, name resolution, `trustd` and TCC need. The exact service list is
  not yet known. It must be measured on a Mac and recorded with the evidence
  before this record can be accepted. No file writes, no
  process start, no Apple Events.
* The broker sends input only when `sandbox_check` reports the worker as
  sandboxed. The worker's own bit says that the call profile, not another
  one, is applied.
* TCC: the app's `Info.plist` gets `NSMicrophoneUsageDescription` and
  `NSCameraUsageDescription`. The call worker is signed with the hardened
  runtime and only the `com.apple.security.device.audio-input` and
  `com.apple.security.device.camera` entitlements. The main app and the
  other helpers keep no entitlements.
* Camera: AVFoundation through the `objc2` crates (the `objc2` family
  already locked, plus `objc2-av-foundation`, `objc2-core-media` and
  `objc2-core-video` of the same release line), NV12 output, at most
  1280x720. The ID is the device's `uniqueID`. Listing reads the devices
  without starting a session.

### Both systems

* The core starts the call host only where the platform has these controls.
  The worker's containment bits and the broker's checks stay the gate.
* Releases ship the call worker on Linux (x64 and arm64) and macOS
  (universal), built against the pinned, hash-checked libwebrtc archives.
* Calls on each system stay unverified until a live run like the Windows run
  of 16 November 2026 passes there, including the containment checks in
  [the live call test plan](../planning/live-call-test.md#linux-and-macos-containment-checks-adr-0049-not-yet-run).
  Those checks have not been run.

### Linux: microphone and speaker changes during a call (amended 27 November 2026)

Amendment status: proposed, needs owner approval.

Problem: the worker's sound target is fixed at launch (`PULSE_SOURCE` and
`PULSE_SINK`), libwebrtc gives only `index:` IDs on Linux, and stage 2 allows
no new socket. A change of microphone or speaker during a call therefore
answered `Unsupported`, and every change made in the call window is a change
during a call. The call kept the system default or the join's choice.

Decision:

* The worker opens one PulseAudio connection before stage 2, in the `Join`
  path next to the device check, and keeps it for the call (`pulse::Session`).
  It uses the same `libpulse.so.0` handle and a blocking `pa_mainloop`.
  Dropping the call closes it. If it cannot open, the call still starts and a
  sound change answers `Unsupported` as before.
* A `SetDevices` sound change runs on that one connection, on a blocking
  thread, one operation at a time, each with a 3 s deadline. It reads the sink
  and source names, the stream lists, and (once, at join) the server's default
  names; then it moves streams with `pa_context_move_source_output_by_name`
  and `pa_context_move_sink_input_by_name`. "System default" during the call
  means the default when the call started (ADR-0059). The worker's event
  loop never waits for it: each sound job (a change, or a try of a waiting
  target) runs on a task of its own, one at a time, and sends its result to
  the loop, so `Leave` and keys are served while the sound server is slow. A
  change asked while a job runs waits (the newest only) and is answered after
  that job. A job is over only when its blocking thread has ended, also when
  its answer went out at the deadline, so two never run at once.
* It moves only this process's own streams: those whose
  `application.process.id` equals the worker's process ID. No other
  program's stream is read for any other purpose or moved. Both choices are
  checked before either stream moves. A non-`pulse:` ID or a name the server
  lacks is `DeviceNotFound`. A "System default" whose call-start device the
  server lacks now is `Unsupported`; a start default may be a monitor. A kind that must move but has no own stream
  yet (for example no playback stream while nobody else is in the call) is
  not a failure: the target waits, the change counts as done, and the move
  runs when a stream exists (after remote audio is subscribed or the own
  microphone is published or unmuted, tried at 250 ms, 1 s and 3 s, and no
  timer runs when nothing waits). One change takes at most 5 s. The forward
  moves must end 1.5 s before that (`ROLLBACK_RESERVE`), so the undo has time;
  an operation that has no time left is not sent. A stream is where the
  server shows it: the worker reads each own stream's sink or source index
  and resolves it to a device name, and plans from that, not from what it last
  asked for, because the server moves streams by itself when a device goes
  away. A move whose reply is late is not a refusal: the server may have done
  it, so the stream is undone too. A move that fails is undone, and the routes
  are read again: `DeviceKept` only when every own stream is on its old
  device, else `AudioDevice`. The new waiting targets are stored only when
  every immediate move worked. A waiting target whose move is refused at the
  last try is dropped and reported with an unasked `DevicesSet`
  (`AudioDevice` also when a try moved the other kind's stream); a stream
  that is not open yet only waits. A try that runs late, after a newer
  change was accepted, does nothing. A choice equal to the last accepted
  one (a camera-only change) is no change; after a lost route it goes to the
  session again. If the defaults query failed at the join, every other
  change is `Unsupported`.
* libwebrtc's own device selection is not touched.

Containment: this adds no capability after stage 2. It creates no socket and
opens no file after stage 2; every later request is a read and a write on the
socket that was open at stage 2. The seccomp deny list and the Landlock rules
are unchanged. The connection can ask the sound server to move the worker's
streams, and also to move other programs' streams, because the sound server
trusts any client of the user. The worker never sends such a request: the
code selects own streams by process ID. A compromised worker could move other
streams already, through the socket that the call needs; this change does not
widen that. The process ID is the stream's `application.process.id`, which the
client that opened the stream sets itself; PulseAudio does not check it.
Another client of the same user can name the worker's process ID, and its
stream then moves with the worker's, to the device the user chose. That gives
it nothing it lacks, because it can already move any stream itself. PulseAudio
gives no server-checked owner for a stream, and a PipeWire-only check was not
reviewed, so this is a known limit. A listing keeps only streams that name the
worker's process ID, before its limit, so other programs' streams never push
the worker's own out of it.

Verification: unit tests cover the stream and name choice, the routes check,
the commit rule and the rollback with a fake sound server. The session test
`pulse_session_test.rs` is ignored by default and needs a disposable sound
server with two speakers and two microphones. It is a synthetic test, not a
call: `given_a_call_session` starts the test binary as a child process that
opens its own record and playback streams and calls `pulse::Session`
directly, under both containment stages, and checks the moves of those
existing streams from outside. It runs no call loop and connects to no SFU.
The second test, `given_a_speaker_that_waits_for_its_stream`, applies stage 1
only. Both passed once on Linux Mint 22.3 with a disposable PipeWire 1.0.5 and
`pipewire-pulse` (27 November 2026). Still open: a change during a running
call, a PulseAudio daemon, sound hardware and the native containment gate.
The earlier contained sound tests show that the worker checks and opens
devices at launch (`PULSE_SOURCE`, `PULSE_SINK`). See
[the device test evidence](../evidence/device-tests-linux-macos.md) for both.
This amendment is not accepted until the owner approves it.

### Positive Consequences

* One call path on all three systems, with no Matrix secret in the worker.
* On the portal path, the Linux worker never touches a device node; the
  desktop asks the user.

### Negative Consequences

* **Linux: the worker can use the sound server and any same-user Unix
  socket it connects to before stage 2.** Only the sound server connection
  is made then, but stage 1 alone does not stop a compromised library
  initialiser from connecting elsewhere.
* Linux: the deny list is weaker than an allow list; a system call that is
  missing from it is allowed.
* Linux: calls need Linux 6.2 or newer with Landlock turned on.
* Linux V4L2 path: the worker can open any camera node, and there is no
  desktop permission dialog.
* macOS: the Seatbelt profile must be measured, not derived. That
  measurement is still pending (see the service list above). After it is
  recorded, a macOS update can break calls until the profile is measured
  again.
* macOS: the call worker gains two entitlements, and `docs/release.md` no
  longer says that the app uses no entitlements.
* New dependencies: the `pipewire` crates and the system `libpipewire-0.3`
  on Linux, and three `objc2` crates on macOS. Each Linux build needs the
  PipeWire development package.
* Neither system is verified live. The development machine runs Windows;
  Linux is built and its stage 1 and stage 2 filters were exercised on a
  Linux Mint 22.3 VM (kernel 6.14, Landlock ABI 6) without a camera or a
  call; macOS is built in CI only.
* Linux: the call worker links `libpipewire-0.3.so.0`, so the `.deb` and
  `.rpm` packages depend on it. Without it the worker does not start and
  audio calls fail too. The AppImage relies on the system's copy.
* Linux: the statically linked libwebrtc defines weak `pw_*` stubs for its
  own screen capture, and the worker's `pipewire` crate binds to them. The
  stubs call through null pointers until libwebrtc loads PipeWire, so the
  worker calls `webrtc::InitializePipeWire()` before its first PipeWire
  call (19 November 2026). A libwebrtc update must keep that function or
  replace this step.
* Linux: microphones and speakers. libwebrtc's PulseAudio device module
  (also used with `pipewire-pulse`) reports no device GUIDs, only list
  positions (`index:N`), which ADR-0043 never lists or selects (measured
  19 November 2026). The owner chose sound server names set at launch
  (19 November 2026):
  * The worker lists the sound server's sinks and sources itself, through
    the PulseAudio client library that libwebrtc already loads (`dlopen`,
    one short connection before stage 2, read only; `pulse.rs`). Monitor
    sources are left out. Each ID is `pulse:` and the name; a name must be
    1 to 128 bytes of ASCII letters, digits, `.`, `-`, `_` or `:`. No
    Landlock or seccomp rule changes.
  * A `pulse:` choice goes to the launch: the core gives the names to the
    broker, which checks the name rule again and sets `PULSE_SOURCE` and
    `PULSE_SINK`. libwebrtc then opens its default stream (position 0, no
    device name), and the client library sends it to that name. The
    worker never calls libwebrtc's device selection for these IDs.
  * The sound server quietly uses its default for a name it does not have
    (measured with `pipewire-pulse`: `PULSE_SINK=no_such_sink pacat` and
    `pacat -d no_such_sink` both played to the default with exit code 0).
    So the worker checks, before audio starts, that the `Join` choice
    matches the environment (`Protocol` if not) and that the server has
    the device now (`DeviceNotFound` if not). A device that goes away
    after this check, or during the call, is moved by the sound server to
    its default; the worker cannot see that. This differs from Windows,
    where the endpoint ID stays bound.
  * If the server cannot be asked (no library, no server, 3 s passed),
    the lists keep the system default only, and a `pulse:` choice is
    `DeviceNotFound`. Windows and macOS launchers ignore the names; a
    `pulse:` choice there is `DeviceNotFound` too.

## Pros and Cons of the Options

### Linux: seccomp and Landlock

* Good, because no root rights or user namespaces are needed.
* Bad, because this policy does not stop a connection to a Unix socket
  path. It handles no right that controls such a connection.

### Linux: namespaces (bubblewrap)

* Good, because a private mount namespace could hide the session bus.
* Bad, because unprivileged user namespaces are restricted on current
  Ubuntu, and ADR-0010 chose no namespace support.

### Linux: a seccomp allow list only

* Good, because it is the strongest filter.
* Bad, because it breaks with library updates, and it has no file boundary
  for a worker that must open files.

### macOS: an XPC service with App Sandbox entitlements

* Good, because the App Sandbox is the supported mechanism.
* Bad, because it needs an XPC service bundle and a new IPC path in place
  of the worker pipes.

### Linux camera: portal, then V4L2

* Good, because the portal path gives the strongest containment, and the
  fallback keeps the camera on desktops without a portal.
* Bad, because there are two code paths and two profiles to review.

## Links

* [ADR-0010](0010-linux-platform-layer.md)
* [ADR-0011](0011-macos-platform-layer.md)
* [ADR-0043](0043-make-audio-calls-through-matrixrtc-in-a-contained-call-worker.md)
* [ADR-0046](0046-do-not-block-same-user-attach-to-the-linux-core.md)
* [ADR-0048](0048-add-camera-video-to-calls-in-the-call-worker.md)
