# Persephone

A planned Rust desktop Matrix chat client for Windows, Linux, and macOS, with Merovingian as a primary integration target.

The first release targets one account, login, rooms, message history, text and image messaging, and end-to-end encrypted chats. The product must be designed for high-risk users, including targeted attacks, metadata exposure, and hostile media.

The project is in initial requirements and architecture planning. No application or runnable scaffold exists yet.

The desktop framework is **Tauri**, with a Rust core and locally bundled web UI.
The accepted [decision record](docs/adr/0000-use-tauri-for-the-desktop-client.md)
records the rationale, alternatives, and required security boundaries. See the
[ADR index](docs/adr/index.md) for architectural decisions.

See [the project plan](docs/project-plan.md) for confirmed requirements, open decisions, and proposed milestones.

Follow the [architecture and development plan](docs/architecture-plan.md) for
component boundaries, sync negotiation, security controls, repository layout,
and milestone acceptance checks. Sliding Sync is the default, with classic
`/sync` as the fallback when compatible Sliding Sync is unavailable.

See [the security design](docs/security-design.md) for the proposed threat model, architecture, UI comparison, and release gates. These are design requirements and proposals, not implemented protections.

The [Slint/iced security research](docs/slint-iced-security-research.md) compares pinned releases, dependency graphs, published advisories, and remaining adoption gates.

The [GTK/Tauri security-model comparison](docs/gtk-tauri-security-model.md) explains untrusted message rendering, webview/core separation, native-toolkit risks, and the required controls.

## Development environment

Rust and Cargo are required. The initial development machine has the stable `x86_64-pc-windows-msvc` toolchain installed.

Build commands, platform dependencies, and supported OS versions will be documented when the Tauri application is scaffolded and its remaining dependencies are selected.

## License

GNU General Public License v3.0. See [LICENSE](LICENSE).
