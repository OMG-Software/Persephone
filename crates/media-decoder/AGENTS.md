# Untrusted WebAssembly decoder

This crate decodes PNG and JPEG inside the WebAssembly guest. Native compilation exists for tests and benchmark comparison; it is not the production containment boundary.

- Target `wasm32-unknown-unknown`, without WASI. Keep the guest ABI limited to the reviewed input/output imports used by `crates/media-sandbox`.
- Do not add network, filesystem, environment, clock, randomness, or process capabilities. Review codec features and dependencies before adding formats.
- Check encoded size, dimensions, pixel count, arithmetic, and output length before large allocations or publication of pixels.
- Keep pixel limits aligned with `crates/media-protocol`. Keep fixed guest memory in `build.rs` equal to the sandbox default memory cap.
- Preserve bounded codec buffers and explicit rejection of unsupported or malformed data. Do not silently retry with a native decoder outside the sandbox.
- Change the guest ABI together with host interface checks and adversarial tests. Rebuild the release guest before testing host or worker changes.

## Checks

- `cargo test --locked -p persephone-media-decoder` tests native decoding logic.
- `cargo build --locked --release -p persephone-media-decoder --target wasm32-unknown-unknown` builds the real guest.
- `python tools/dev.py rust` also lints the guest target and runs host and worker checks. Native tests alone do not check the WebAssembly boundary.
