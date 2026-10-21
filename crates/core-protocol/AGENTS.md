# Core and UI protocol

This crate defines bounded commands, replies, and change events between the keyless UI and core. It must remain independent of the core implementation, Matrix SDK, stores, and keys.

- Use `persephone-media-protocol` framing and resource budgets. Reject invalid headers before allocating a body.
- Keep command and payload schemas strict. Preserve unknown-command and unknown-field rejection, including empty command objects. Serde unit variants do not provide the same field rejection as struct variants.
- Distinguish a required nullable field from a missing field. Test the actual Serde wire representation.
- Preserve request/reply correlation and distinguish pushed events from replies. Keep events small; message content belongs in bounded view calls.
- Keep IDs, cursors, subscription counts, and error codes bounded and explicit. Coordinate any limit change with the receiving core, Tauri relay, and frontend consumers.
- Test malformed frames, unexpected kinds, truncated input, extra fields, invalid variants, and oversized messages. Preserve fail-closed behavior.

## Checks

- `cargo test --locked -p persephone-core-protocol`
- `cargo test --locked -p persephone-core`
- Use `python tools/dev.py check` for coordinated protocol changes across the core, Tauri relay, and frontend.
