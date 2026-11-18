# Call worker control protocol

This crate defines the bounded binary messages between the key-holding core and a contained call worker (ADR-0043). They travel as media-protocol Control frames over the worker pipes.

- Treat both directions as hostile. The core must not trust worker messages. The worker must validate core messages.
- Check every limit before allocation. Reject unknown versions and tags, over-limit fields, bad UTF-8, control characters, truncation, and trailing bytes.
- Keep errors as closed enums. Never add free text, because untrusted input must not travel back as a message.
- Keep secrets (JWT, TURN credentials, frame keys) in `Zeroizing`. Never log them. Keep `Debug` output free of secret contents.
- A `VideoFrame` message is always followed by one `RgbaPixels` frame inside the video limits (ADR-0048). The core reads worker output with `read_worker_item`, which checks that frame's header before allocation. The worker writes both with `write_video_frame`. Never read worker output with `read_message` alone.
- Keep the `ws://` loopback SFU exception behind the `dev-disposable-servers` feature only.
- Coordinate wire changes with `crates/core` and `crates/call-worker` (planned), and bump the version byte.

## Checks

- `cargo test --locked -p persephone-call-protocol`
- `cargo test --locked -p persephone-call-protocol --features dev-disposable-servers`
- `cargo clippy --locked -p persephone-call-protocol --all-targets -- -D warnings`
