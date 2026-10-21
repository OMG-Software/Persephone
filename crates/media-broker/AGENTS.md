# Contained worker broker

This is trusted code for launching and supervising a hostile-media worker. Follow ADR-0003 and the worker frame protocol.

- Start the Windows worker suspended. Apply the AppContainer, process mitigations, inherited-handle allowlist, and Job limits before it runs.
- Preserve minimal environment forwarding. Never pass account credentials, store paths, keys, or unrelated handles to the worker.
- Enforce the wall deadline outside the decoder. Ensure every success and error path terminates the worker and releases owned handles and profiles.
- Validate one bounded reply and its exit status. Preserve distinctions between containment failure, decode rejection, protocol failure, deadline expiry, and abnormal exit.
- Keep resource reservations attached to returned pixels while they remain live.
- AppContainer alone does not establish sender-side network denial. Keep WebAssembly isolation as the decoder capability boundary, and retain the documented OS residual risk.
- Do not silently launch an unrestricted worker on platforms without implemented containment.

## Checks

- Use `python tools/dev.py rust` to build the guest, build the workspace worker binary, and run broker integration tests in the required order.
- For a focused Windows run, first build the guest with `cargo build --locked --release -p persephone-media-decoder --target wasm32-unknown-unknown`, then build `cargo build --locked -p persephone-media-worker`, then run `cargo test --locked -p persephone-media-broker`.
- `tests/contained_worker.rs` is Windows-only and launches the real contained worker. A package-only test run does not build that worker automatically.
- Benchmarks are ignored by default. Follow `tests/benchmark.rs` for the release build and `PERSEPHONE_BENCH_DIR` setup; do not report an ignored benchmark as executed.
