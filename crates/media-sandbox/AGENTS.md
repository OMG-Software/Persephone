# WebAssembly sandbox host

This is the trusted host for the untrusted decoder. Follow ADR-0003. The runtime boundary must reject capabilities it did not explicitly grant.

- Validate the module and its interface before execution. Keep exact import names and signatures and required exports. Reject WASI and all other imports.
- Keep a fresh store per decode. Enforce memory, fuel, dimensions, pixel count, input size, output range, and output count on the host side.
- Validate guest pointers, lengths, arithmetic, and RGBA shape before exposing pixels. Treat every guest status and output as hostile.
- Keep `validate` and `portable-dispatch` enabled for wasmi. If runtime features or dispatch modes change, review the security effects and repeat adversarial tests and resource measurements.
- Keep `Limits::max_memory_bytes` aligned with `crates/media-decoder/build.rs`. The guest's fixed memory prevents transient growth allocations from exceeding the worker budget.
- Native codec code is a development reference dependency only. Do not add native decoding to the runtime fallback path.
- Extend adversarial module tests when changing imports, exports, memory handling, traps, or limits. Keep interpreter limits separate from the broker's wall deadline.

## Checks

- Build the guest first: `cargo build --locked --release -p persephone-media-decoder --target wasm32-unknown-unknown`.
- Run `cargo test --locked -p persephone-media-sandbox`. Tests load the release guest from `.local/build/rust/wasm32-unknown-unknown/release/`.
- Use `python tools/dev.py rust` for the complete ordered Rust check.
- Benchmarks are ignored by default. Follow `tests/benchmark.rs` for its release build and `PERSEPHONE_BENCH_DIR` setup. Report measurements separately from correctness checks.
