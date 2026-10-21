# Persephone

Persephone is a planned Rust desktop Matrix client for Windows, Linux, and macOS, with Merovingian as a primary integration target. Its first release is intended to support one account, rooms, message history, text and image messaging, and end-to-end encrypted chats. It's made for everyone, but engineered to withstand a targeted attack, not just everyday threats.

Project page: <https://omg-software.github.io/Persephone/> (source in [`site/`](site)).

Persephone is in early development: no milestone has passed its acceptance gates, and nothing is ready for real use. Progress, deliverable by deliverable, is in the [milestone status](docs/architecture/development-plan.md#milestone-status); recorded results and open gates are in the [feasibility workbench](docs/planning/feasibility.md). The repository contains a key-holding core process (`crates/core`) that starts a keyless Tauri UI (`src-tauri/`), keeps an encrypted local profile, signs in to a Matrix homeserver through a native Windows credential prompt and shows real rooms and history; a contained WebAssembly media decoder and worker, not yet connected to the app; shared Rust building blocks; and integration and security test programs. Only Windows is built and tested so far, and no production security controls exist yet.

Tauri is the accepted desktop framework. See the [ADR index](docs/adr/index.md), [project plan](docs/planning/project-plan.md), and [architecture and security plans](docs/README.md#plans). The framework choice and Sliding Sync preference are accepted; other architecture details remain proposals pending implementation evidence.

## Development

The root Cargo workspace uses one committed `Cargo.lock`. Run `python tools/dev.py bootstrap` once to install the locked frontend dependencies, then `python tools/dev.py check` for Rust, frontend, and contract checks. The Windows containment check is separate: `python tools/dev.py sandbox --offline` currently exits 1 on Windows because the AppContainer worker is not network-denied (image decoding moves into a WebAssembly sandbox, per ADR-0003), and exits 2 as unsupported elsewhere. The OSV dependency audit is separate and needs network access: `python tools/dev.py audit`. See [Development](docs/development.md) for prerequisites, local output paths, and the component layout.

GNU General Public License v3.0 applies; see [LICENSE](LICENSE).
