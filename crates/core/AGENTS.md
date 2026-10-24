# Key-holding core

This crate is the application entry process. ADR-0004 assigns sessions, SDK work, keys, stores, and the media broker to this process. Current view fixtures and demo events are synthetic; do not describe them as Matrix integration.

- Treat every UI request as hostile. Use the budgeted frame reader before body allocation, strict protocol decoding, and explicit argument validation.
- Keep bounded projections in `src/views.rs`. Preserve room, item, serialized-byte, name, body, and cursor limits. Truncate text on valid character boundaries.
- Keep identity normalization separate from message-body handling. Preserve the documented treatment of controls and bidirectional text.
- Keep room subscriptions bounded. Coalesce change notifications and fetch content through bounded calls. A slow UI must not create an unbounded event queue.
- Review lock order, wakeups, writer failure, pipe closure, and thread completion together when changing the serving loop. Do not infer full shutdown from a dropped handle or task status.
- Keep protocol violations fatal to the affected UI connection. Return bounded public error codes instead of internal diagnostics.
- Preserve UI process supervision and Windows Job ownership in `src/main.rs`, `src/ui_host.rs` and `src/platform/windows/ui.rs`. Unsupported platforms need measured equivalent controls before security claims.
- Keep per-OS code in `src/platform/<os>/` behind the traits in `vault.rs`, `credentials.rs`, `pick.rs`, `media.rs` and `ui_host.rs`. An OS without an implemented layer fails closed.
- Coordinate commands and events with `crates/core-protocol`, `src-tauri/src/core_link.rs`, the Tauri command surface, and frontend bridge.

## Checks

- `cargo test --locked -p persephone-core` includes view and serving-loop tests.
- Run `cargo test --locked -p persephone-core-protocol` for wire changes, then the Tauri and frontend checks described in their guidance.
- Use `python tools/dev.py rust` for ordered workspace verification. Keep synthetic serving-loop results separate from production lifecycle evidence.
