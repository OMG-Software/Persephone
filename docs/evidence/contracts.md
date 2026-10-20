# P0 contract-generation probe

> Historical run record: commands and source paths below describe the pre-restructure checkout. See [Development](../development.md) for current paths and commands.

## Finding

A small standalone contract-generation path is feasible with the current pinned candidate `ts-rs 12.0.1`. Its explicitly enabled `serde-compat` feature mirrors supported Serde field renames, internally tagged enums, and `Option<T>` as nullable fields. The probe keeps exported types as explicit frontend DTOs, generates checked TypeScript and synthetic fixtures from those Rust definitions, and detects drift with an explicit `--check` command. This supports a provisional P0 choice; adoption remains provisional until the real Tauri/webview boundary and runtime validation strategy are tested.

The architecture plan already requires explicit public DTOs and warns that TypeScript types do not validate hostile runtime values ([`docs/architecture-plan.md`](../architecture/development-plan.md#ipc-contracts-and-frontend-policy)). The probe contains no SDK, credential, key, or Tauri types.

## Probe behavior

`probes/p0/contracts` pins direct dependencies exactly and checks in `Cargo.lock`. The package is `publish = false` and declares `GPL-3.0-only`. The tested direct dependency versions and registry license expressions are:

| Dependency | Version | License |
| --- | --- | --- |
| `ts-rs` | `12.0.1` | MIT |
| `serde` | `1.0.229` | MIT OR Apache-2.0 |
| `serde_json` | `1.0.154` | MIT OR Apache-2.0 |

Cargo locked 21 packages total, including the probe package. `cargo metadata --locked` reported these upstream transitive license expressions: `itoa 1.0.18` MIT OR Apache-2.0; `memchr 2.8.3` Unlicense OR MIT; `proc-macro2 1.0.107`, `quote 1.0.47`, `serde_core 1.0.229`, `serde_derive 1.0.229`, `syn 2.0.119`, `syn 3.0.9`, `thiserror 2.0.21`, `thiserror-impl 2.0.21`, `windows-link 0.2.1`, `windows-sys 0.61.2` MIT OR Apache-2.0; `termcolor 1.4.1` and `winapi-util 0.1.11` Unlicense OR MIT; `unicode-ident 1.0.27` (MIT OR Apache-2.0) AND Unicode-3.0; `zmij 1.0.23` MIT; `ts-rs-macros 12.0.1` MIT. (The lockfile and metadata output are the authority for exact resolution.)

The public projection defines `RoomSummary`, `TimelineItem`, and `TimelinePage`. Optional fields are emitted as required `T | null` fields, matching default Serde serialization. `TimelineItem` uses Serde's `kind` tag and snake-case variant names. The probe restricts JSON numeric fields to `u32`; its maximum `4,294,967,295` is below JavaScript's exact integer ceiling `9,007,199,254,740,991`. Larger values should cross the boundary as decimal strings or receive an explicit separately tested encoding policy.

`export-contracts -- --write` is the only command that updates `generated/contracts.ts`; the default and explicit `--check` modes only compare content. Rust tests compare the synthetic serialized DTO values with checked-in JSON fixtures, reject unknown struct fields, tagged enum variants, enum fields, and nested fields, and verify generated output without writing files. Numeric tests accept `u32::MAX` and reject negative, fractional, and `u32::MAX + 1` JSON values. `ts-rs` emits its documented warning that it does not interpret Serde's `deny_unknown_fields` attribute; the TypeScript declaration is not responsible for runtime rejection, and the explicit Serde tests cover that behavior. The TypeScript output includes fixture literals serialized from those same Rust DTO constructors and uses `satisfies` against the generated declarations; the installed frontend TypeScript compiler typechecks these fixtures and negative examples for an invalid enum tag and renamed field.

## Validation

All commands ran from `C:\dev\Persephone` with `CARGO_HOME=C:\dev\Persephone\.local\p0\cargo-home`, `CARGO_TARGET_DIR=C:\dev\Persephone\.local\p0\contracts-target`, and the existing LLVM librarian path set for Windows MSVC builds.

| Command | Exit | Result |
| --- | ---: | --- |
| `cargo fmt --manifest-path probes/p0/contracts/Cargo.toml` | 0 | formatted probe |
| `cargo fmt --manifest-path probes/p0/contracts/Cargo.toml -- --check` | 0 | clean |
| `cargo run --locked --manifest-path probes/p0/contracts/Cargo.toml --bin export-contracts -- --write` | 0 | explicit generation succeeded |
| `cargo run --locked --manifest-path probes/p0/contracts/Cargo.toml --bin export-contracts -- --check` | 0 | checked-in bindings current |
| Temporary generated-file drift injection followed by `--check` | 1 expected; restored check 0 | stale content rejected; file restored |
| `cargo test --locked --manifest-path probes/p0/contracts/Cargo.toml` | 0 | 4 tests passed |
| `cargo clippy --locked --manifest-path probes/p0/contracts/Cargo.toml --all-targets -- -D warnings` | 0 | clean |
| `cargo metadata --locked --manifest-path probes/p0/contracts/Cargo.toml --format-version 1` | 0 | 21 packages and license expressions inspected |
| `probes/p0/frontend/node_modules/.bin/tsc.cmd --version` | 0 | TypeScript 6.0.3 |
| `probes/p0/frontend/node_modules/.bin/tsc.cmd --project probes/p0/contracts/tsconfig.json` | 0 | generated types and Rust-serialized fixtures typecheck |

The package reports `ts-rs 12.0.1`, MIT, MSRV 1.78 on its exact registry entry; the probe itself is built with the repo's current Rust 1.96-compatible dependency resolution. The official pinned `ts-rs` API docs describe manual export, the default `serde-compat` feature, and supported attributes including tags and field renames ([`ts-rs 12.0.1 API docs`](https://docs.rs/ts-rs/12.0.1/ts_rs/trait.TS.html), [`ts-rs 12.0.1 package`](https://crates.io/crates/ts-rs/12.0.1)). The expected macro warning says `ts-rs` ignores `deny_unknown_fields` while generating TypeScript; Serde tests validate that runtime deserialization behavior separately.

## Decision limits

This is a feasibility result for DTO serialization and generated compile-time types. The generated integer field is only TypeScript `number`; its declaration does not encode integer or range checks. It does not make TypeScript a runtime validator, enforce payload sizes, validate actual Tauri IPC values, or prove Svelte/webview rendering safety. Those need boundary-level tests in the real app and runtime validation before any hostile IPC object is trusted.

`tauri-specta` is a reasonable P1 alternative if command/event bindings should be generated together with Tauri registration. Its upstream project describes typesafe Tauri commands and event generation. This isolated P0 uses `ts-rs` because it can generate from a standalone DTO package before a Tauri application scaffold exists; that keeps the feasibility measurement independent of command wiring. Revisit this tradeoff once actual commands and event shapes are available ([`tauri-specta project`](https://github.com/specta-rs/tauri-specta)).

Confidence: 91% that the candidate supports a deterministic, Serde-aligned DTO-to-TypeScript probe; adoption confidence remains provisional pending actual Tauri IPC/runtime validation and webview behavior.