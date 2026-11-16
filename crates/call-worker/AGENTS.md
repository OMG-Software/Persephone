# Call worker

The contained process that runs one call (ADR-0043). It joins one LiveKit room with frame encryption and platform audio. It speaks the bounded protocol of `crates/call-protocol` over stdin and stdout.

- Treat the core's messages and the SFU as untrusted. Validate every identity before it goes back to the core. Send no free text, only closed `FailureCode` values.
- Stdout is the frame channel. Write nothing else to it. Write nothing to stderr. Add no logging output.
- Keep the JWT, TURN credentials and frame keys in `Zeroizing` until they go to LiveKit. Move them out with `std::mem::take`; do not copy them. Never log them.
- Use bounded queues only. The LiveKit event receiver is unbounded inside the SDK; the worker drains it in one task.
- Check containment before reading any input. On a missing control send `Ready`, then `Failed(Containment)`, and exit. The feature `dev-uncontained` skips the exit for local tests only. Never enable it in a build that ships.
- A decryption failure of one remote participant does not end the call.
- Keep each LiveKit type inside `session.rs`. Keep `mapping.rs` free of I/O and LiveKit types so it stays unit-testable.

## Build

- The pinned libwebrtc must be verified first: `python tools/dev.py webrtc`. Set `LK_CUSTOM_WEBRTC` to the directory it prints for every cargo command. The build must never download libwebrtc.
- On Windows the prebuilt libwebrtc uses the static C runtime. Build and test this package with `RUSTFLAGS=-C target-feature=+crt-static`. `tools/dev.py` does this and builds the package apart from the rest of the workspace.
- `third_party/` holds vendored LiveKit crates with small marked patches (TLS 1.3 only and no proxy variables in `livekit-net`; key ring of 256 in `libwebrtc` and `webrtc-sys`). Read each `PERSEPHONE-PATCH.md`. They are patch sources, not workspace members.

## Checks

- `cargo test --locked -p persephone-call-worker`
- `cargo clippy --locked -p persephone-call-worker --all-targets -- -D warnings`
