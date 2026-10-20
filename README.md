# Persephone

Persephone is a planned Rust desktop Matrix client for Windows, Linux, and macOS, with Merovingian as a primary integration target. Its first release is intended to support one account, rooms, message history, text and image messaging, and end-to-end encrypted chats for high-risk users.

Project page: <https://omg-software.github.io/Persephone/> (source in [`site/`](site)).

The project remains in the P0 feasibility phase. Current status, recorded evidence, and open acceptance gates are in the [feasibility workbench](docs/planning/feasibility.md). The repository contains shared Rust building blocks, integration and security test programs, a contained media worker prototype, and a first desktop shell slice (`src-tauri/`, one allowlisted command); no Matrix functionality is wired into the app and no production security controls exist yet.

Tauri is the accepted desktop framework. See the [ADR index](docs/adr/index.md), [project plan](docs/planning/project-plan.md), and [architecture and security plans](docs/README.md#plans). The framework choice and Sliding Sync preference are accepted; other architecture details remain proposals pending implementation evidence.

## Development

The root Cargo workspace uses one committed `Cargo.lock`. Run `python tools/dev.py bootstrap` once to install the locked frontend dependencies, then `python tools/dev.py check` for Rust, frontend, and contract checks. The Windows containment check is separate: `python tools/dev.py sandbox --offline` currently exits 1 on Windows because the AppContainer worker is not network-denied (image decoding moves into a WebAssembly sandbox, per ADR-0003), and exits 2 as unsupported elsewhere. The OSV dependency audit is separate and needs network access: `python tools/dev.py audit`. See [Development](docs/development.md) for prerequisites, local output paths, and the component layout.

GNU General Public License v3.0 applies; see [LICENSE](LICENSE).
