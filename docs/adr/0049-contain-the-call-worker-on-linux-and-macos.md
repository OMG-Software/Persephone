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
  `SIGKILL`, `no_new_privs`, no core dumps, no file writes (`RLIMIT_FSIZE`
  0), at most 256 descriptors and an address-space limit of 4 GiB. The
  address-space limit is a bound, not the 512 MiB of the Windows Job:
  libwebrtc reserves much more address space than it uses.
* Descriptor 3 is one end of a Unix socket pair, the camera channel. It is
  the only extra descriptor.
* The environment holds only `XDG_RUNTIME_DIR` and `HOME` (the sound
  server's socket and cookie) and `LANG=C`. It has no
  `DBUS_SESSION_BUS_ADDRESS`.
* One argument, `--camera=portal`, `--camera=v4l2` or `--camera=none`,
  tells the worker which camera path it may use (see "Linux: camera"). The
  worker refuses any other argument.

### Linux: containment

The worker applies two stages itself.

* **Stage 1, before it reads any input:**
  * `PR_SET_DUMPABLE` 0. The core stays dumpable (ADR-0046); the worker
    calls no portal, so the portal problem of ADR-0046 does not apply.
  * Landlock, ABI 3 or newer (Linux 6.2), handling every file system right
    of the running ABI. Read and execute: `/usr`, `/lib`, `/lib64`, `/etc`,
    `/run/systemd/resolve`, `/proc/self`. Read: `$HOME/.config/pulse`,
    `/dev/urandom`, `/dev/null`. Read and write: `/dev/shm`. With
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
    the core sends no `SetCamera` for it. Each camera-on on this path goes
    directly after its own descriptor; the worker closes a descriptor that
    its camera-on does not use.
* Portal path: when the camera turns on (or a list of cameras is asked
  for), the core calls `AccessCamera` and waits for its `Response` for at
  most 60 seconds. On success it calls `OpenPipeWireRemote` and sends the
  returned descriptor to the worker over the camera channel (`SCM_RIGHTS`,
  one descriptor and one byte). It then sends `SetCamera` to turn the
  camera on, or `ListDevices` to list the cameras. The worker
  connects to that PipeWire remote, which shows only the camera nodes the
  portal allows. It never opens a device node or the session bus.
* Listing on the portal path asks the portal for access, so the desktop may
  show its permission dialog. Without access, no cameras are listed;
  microphones and speakers still are.
* V4L2 path: the worker opens `/dev/video*` itself. It lists only nodes with
  the video-capture capability; it does not stream to list them.
* Formats: NV12, I420 and YUYV only, at most 1280x720. The worker converts
  YUYV to I420. It decodes no MJPEG. PipeWire buffers must be shared memory
  (`MemFd` or `MemPtr`); DMA buffers are refused.
* Camera IDs: the PipeWire `node.name` on the portal path, the V4L2 bus
  information on the V4L2 path. A camera chosen on one path is not found on
  the other, so the camera stays off with `NotFound`; the worker never
  takes another camera.

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
  of 16 November 2026 passes there, including the containment checks below.

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

## Pros and Cons of the Options

### Linux: seccomp and Landlock

* Good, because no root rights or user namespaces are needed.
* Bad, because Landlock cannot stop a connection to a Unix socket path.

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
