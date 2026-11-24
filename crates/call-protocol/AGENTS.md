# Call worker control protocol

This crate defines the bounded binary messages between the key-holding core and a contained call worker (ADR-0043). They travel as media-protocol Control frames over the worker pipes.

- Treat both directions as hostile. The core must not trust worker messages. The worker must validate core messages.
- Check every limit before allocation. Reject unknown versions and tags, over-limit fields, bad UTF-8, control characters, truncation, and trailing bytes.
- Keep errors as closed enums. Never add free text, because untrusted input must not travel back as a message. The one exception is `FromWorker::Diagnostic` (ADR-0058), which the core only writes to the diagnostic log through its filter and never shows or acts on.
- Keep secrets (JWT, TURN credentials, frame keys) in `Zeroizing`. Never log them. Keep `Debug` output free of secret contents.
- A `VideoFrame` message is always followed by one `RgbaPixels` frame inside the video limits (ADR-0048). The core reads worker output with `read_worker_item`, which checks that frame's header before allocation. The worker writes both with `write_video_frame`. Never read worker output with `read_message` alone.
- The picture limits belong to the tile (ADR-0053): tiles 0 to 4 are camera tiles (640x360, 15 per second), tile 5 is the one remote screen share (1280x720, 5 per second). Use `picture_fits(tile, width, height)` before any allocation, in the reader and in the writer. Wire version 4 added `SetScreen`, `Screen` and `ScreenSharer`. Version 5 added the share number to `SetScreen` and `Screen`: each state names the share it describes, so the core never takes a late state of an older share for the current one. The screen states are a closed set. A command names a screen only as a portal node or as a monitor index with its size; the page never does.
- Keep the `ws://` loopback SFU exception behind the `dev-disposable-servers` feature only.
- Other SFUs (ADR-0054): wire version 6 added `ToWorker::Subscribe { index 1..=3, url, jwt }` (the JWT in `Zeroizing`, the URL checked like the `Join` URL) and `FromWorker::Connection { connection 1..=3, up }`. `ParticipantJoined`, `ParticipantLeft`, `Speaking`, `ScreenSharer` and `VideoStarted` carry the connection number (0 is the own SFU, 1 to 3 are other SFUs, `MAX_OTHER_CONNECTIONS`); the reader refuses a larger number. The worker only reports the number. The core decides which connection may speak for which member.
- Deafen and volume (ADR-0055): wire version 7 added `ToWorker::SetDeafened`, `ToWorker::SetVolume { percent }` and `FromWorker::Deafened`, and `Join` ends with the volume. `Reader::volume` refuses a volume over `MAX_VOLUME_PERCENT` (100).
- Diagnostic log (ADR-0058): wire version 8 added `ToWorker::SetDiagnostics { on }` (allowed before `Join`) and `FromWorker::Diagnostic { level 1..=4, target, message }`. The target is 1 to 64 bytes and the message at most 2 KiB, both without control characters.
- Sound level meter (ADR-0055 amendment of 2026-11-23): wire version 9 added `FromWorker::SoundLevel { level }`, 0 to `MAX_SOUND_LEVEL` (100). `Reader::sound_level` refuses a larger value.
- Devices and levels (ADR-0059): wire version 10 added `ToWorker::TestDevice { test, device }` (a first message only, like `ListDevices`; `DeviceTest` is a closed set), `ToWorker::SetDevices { recording, playout }`, `FromWorker::ParticipantLevel { connection, identity, level }`, `FromWorker::ParticipantMuted { connection, identity, muted }` and `FromWorker::DevicesSet { failure }` (0 for none, else a `FailureCode`). `SoundLevel` is now the own microphone's level.
- Coordinate wire changes with `crates/core` and `crates/call-worker` (planned), and bump the version byte.

## Checks

- `cargo test --locked -p persephone-call-protocol`
- `cargo test --locked -p persephone-call-protocol --features dev-disposable-servers`
- `cargo clippy --locked -p persephone-call-protocol --all-targets -- -D warnings`
