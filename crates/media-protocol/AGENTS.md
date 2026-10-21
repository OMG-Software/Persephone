# Bounded frame and resource protocol

This dependency-free crate supplies binary frames and shared resource admission for media and core/UI IPC. Keep it usable by both native code and the WebAssembly guest.

- Validate frame kind, length, dimensions, pixel count, and checked arithmetic before allocation or reading an untrusted body.
- Reserve aggregate and per-class budgets before allocation or queueing. Keep RAII reservations alive for the full lifetime of the data and release them on all failure paths.
- Preserve exact wire encoding and explicit failure messages. Coordinate changes with core-protocol, media broker, worker, decoder, sandbox, and their tests.
- Keep encoded-image, pixel, and control-frame limits separate. A valid frame must also satisfy its applicable admission budget.
- Review atomic admission and rollback paths for concurrent limit enforcement. Test rejection and reservation release, not only successful frames.
- Do not add OS APIs, SDK types, or runtime authority to this shared protocol crate.

## Checks

- `cargo test --locked -p persephone-media-protocol`
- `cargo test --locked -p persephone-core-protocol` for shared framing changes.
- Use `python tools/dev.py rust` for coordinated limit or wire changes. It also verifies that the guest still compiles for WebAssembly.
