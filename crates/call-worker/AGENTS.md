# Call worker

The contained process that runs one call (ADR-0043). It joins one LiveKit room with frame encryption and platform audio. It speaks the bounded protocol of `crates/call-protocol` over stdin and stdout.

- Treat the core's messages and the SFU as untrusted. Validate every identity before it goes back to the core. Send no free text, only closed `FailureCode` values.
- Stdout is the frame channel. Write nothing else to it. Write nothing to stderr. Add no logging output.
- Keep the JWT, TURN credentials and frame keys in `Zeroizing` until they go to LiveKit. Move them out with `std::mem::take`; do not copy them. Never log them.
- Use bounded queues only. The LiveKit event receiver is unbounded inside the SDK; the worker drains it in one task.
- Check containment before reading any input. On a missing control send `Ready`, then `Failed(Containment)`, and exit. The feature `dev-uncontained` skips the exit for local tests only. Never enable it in a build that ships.
- A decryption failure of one remote participant does not end the call.
- Keep each LiveKit type inside `session.rs`. Keep `mapping.rs` and `video.rs` free of I/O and LiveKit types so they stay unit-testable.
- Camera and video (ADR-0048): `camera/mod.rs` is the platform-neutral API (`list`, `open`, `Capture`, `CapturedFrame`) with no OS or LiveKit types. New OS backends go in `camera/<os>.rs` behind the same API. Each one needs its own ADR and containment review. Do not add one without them.
- Containment (ADR-0049): `containment/mod.rs` holds the bit constants and `missing_controls()`; one module per OS (`windows.rs`, `linux.rs`, `macos.rs`). Never change a released bit value. macOS (`containment/macos.rs`) applies the call Seatbelt profile with `sandbox_init` before any input and checks it; its service list is "to be measured on a Mac" and was not compiled or run on the development machine. Linux (`containment/linux.rs`) applies stage 1 before any input (non-dumpable, Landlock ABI 3 or newer, seccomp deny list; `clone3` returns `ENOSYS`) and `containment::apply_stage2()` after the audio connection and before `Room::connect` (no new `AF_UNIX` socket, no Landlock calls); a stage 2 failure ends the call with `Containment`. Keep the Landlock path table to the ADR; add no path without an ADR change.
- Arguments (`args.rs`): Linux takes at most one `--camera=portal|v4l2|none` (none by default). Any other argument, or any argument on another OS, sets `CHECK_FAILED`, so the worker sends `Ready` and then `Failed(Containment)`. Read the mode with `camera::mode()`.
- Camera channel (`camera_channel.rs`, Linux): descriptor 3 carries one byte and one `SCM_RIGHTS` descriptor per message. It keeps at most one pending descriptor and closes every other. `take_handle()` is the only reader. Keep the module free of other worker modules so it builds alone.
- Linux cameras (ADR-0049): `camera/linux.rs` dispatches by `camera::mode()`: `linux_portal.rs` (PipeWire over the descriptor from the core, `node.name` IDs, shared-memory buffers only) or `v4l2.rs` (raw ioctls, `bus_info` IDs). `yuyv.rs` holds the format policy (NV12, I420, YUYV, at most 1280x720, no MJPEG) and the YUYV conversion. A refused portal never leads to V4L2, and a chosen camera that is absent is `NotFound`.
- macOS camera (`camera/macos.rs`, AVFoundation through `objc2`): `uniqueID` IDs, NV12 at most 1280x720, no fallback camera. A `NotDetermined` permission asks the user and waits at most 60 s (`OPEN_TIMEOUT` in `camera/mod.rs` is 65 s on macOS for this). A denied or restricted permission is `Unavailable`.
- A camera error never ends the call: report `CameraState`. A chosen camera that is absent is `NotFound`; never fall back to another camera. Turning the camera off stops and joins the capture thread, then unpublishes the track.
- Every own track (microphone and camera) needs its frame cryptor set to the current own key index. `SetOwnKey` updates all of them. The camera sends no picture before that.
- Never await a camera open in the event loop. `begin_camera` opens on its own thread, and the result comes back through a bounded channel to `camera_opened`. `CameraGate` allows one open at a time, drops a stale result and releases its camera. `SetOwnKey`, `SetKey` and `Leave` are served while an open waits.
- Pictures never use the control queue. `output.rs` keeps at most one waiting picture per tile (newest wins), and control messages always go out first. Pictures are RGBA in byte order R, G, B, A, at most 640x360 and 15 per second per tile.

## Build

- The pinned libwebrtc must be verified first: `python tools/dev.py webrtc`. Set `LK_CUSTOM_WEBRTC` to the directory it prints for every cargo command. The build must never download libwebrtc.
- On Windows the prebuilt libwebrtc uses the static C runtime. Build and test this package with `RUSTFLAGS=-C target-feature=+crt-static`. `tools/dev.py` does this and builds the package apart from the rest of the workspace.
- `third_party/` holds vendored LiveKit crates with small marked patches (TLS 1.3 only and no proxy variables in `livekit-net`; key ring of 256 in `libwebrtc` and `webrtc-sys`). Read each `PERSEPHONE-PATCH.md`. They are patch sources, not workspace members.

## Checks

- `cargo test --locked -p persephone-call-worker`
- `cargo clippy --locked -p persephone-call-worker --all-targets -- -D warnings`
