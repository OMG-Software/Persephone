# Rust components

Apply the root `AGENTS.md` and the nearest crate guidance. Run commands from the repository root.

- Keep dependency direction explicit. The keyless UI may depend on contracts and protocols. It must not gain core, SDK, store, or key authority through a shared crate.
- Keep the workspace lockfile and explicit dependency features. Review added parsers, native libraries, and platform capabilities.
- Enforce resource limits before allocation or queue admission. Keep reservations alive while their data remains in use.
- Treat protocol and DTO changes as coordinated changes across Rust producers, Rust consumers, the frontend bridge, fixtures, and tests.
- Preserve separate boundaries for the core, UI, media broker, worker, and WebAssembly decoder. A separate process alone does not prove containment.
- Use accepted ADRs in `docs/adr/` for architecture. Check current code and evidence when plans contain older status text.

## Checks

- Use `cargo test --locked -p <package>` for a focused crate check. Child guidance lists prerequisites and package names.
- Use `python tools/dev.py rust` for the ordered Rust checks. It builds the frontend and release WebAssembly guest before workspace tests and Clippy.
- Use `python tools/dev.py contracts` when shared frontend DTOs change, and `python tools/dev.py frontend` when their consumers change.
- Use `python tools/dev.py check` for the full development check. It does not run live homeservers, dependency audits, or the separate native containment probe.
- Add `--offline` only when required dependencies are cached. Report platform exclusions and ignored benchmarks separately from executed checks.
