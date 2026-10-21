# Contained media worker

This process handles one encoded image and returns one bounded pixel or failure frame. It must hold no account, SDK, store, token, or key authority.

- Check required containment before reading input. Missing controls, failed checks, and unsupported platforms must refuse decoding.
- Keep stdin and stdout as binary protocol channels. Send diagnostics through an appropriate separate channel; never mix text with frame bytes.
- Use budgeted input framing and the embedded WebAssembly guest. Do not add a native decode fallback or runtime download of decoder code.
- Keep the decoder fixed at worker build time through `build.rs`. `PERSEPHONE_MEDIA_GUEST` selects a build input, not a runtime capability.
- Validate output through the sandbox and frame writer before publication. Preserve explicit exit codes and failure categories used by the broker.
- Coordinate containment bits and checks in `src/containment.rs` with broker launch controls and Windows integration tests. A self-check does not prove complete OS isolation or network denial.

## Checks

- Build the release guest first: `cargo build --locked --release -p persephone-media-decoder --target wasm32-unknown-unknown`.
- `cargo test --locked -p persephone-media-worker` checks the worker package; it does not by itself test a broker launch under containment.
- `python tools/dev.py rust` builds the workspace and runs the broker's real-worker integration tests on Windows.
- Keep the separate `python tools/dev.py sandbox` platform probe result distinct from worker integration results. Do not turn a failed containment gate into a pass.
