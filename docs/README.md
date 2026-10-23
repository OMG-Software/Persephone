# Documentation

## Plans

- [Project plan](planning/project-plan.md): approved scope, open decisions, and milestones.
- [Feasibility workbench](planning/feasibility.md): current status, recorded results, and P0 acceptance blockers.
- [Architecture and development plan](architecture/development-plan.md): current component layout, planned application structure, and implementation sequence.
- [SDK lifecycle completion contract](architecture/sdk-lifecycle.md): required stop-and-join ownership before lock, logout or store handoff can be approved.
- [Security design](architecture/security-design.md): proposed threat boundaries, controls, and release gates.
- [Development guide](development.md): workspace commands, toolchain requirements, and local data layout.

## Decisions

- [ADR index](adr/index.md): accepted and proposed architectural decisions.
- [ADR-0000](adr/0000-use-tauri-for-the-desktop-client.md): Tauri as the desktop framework.
- [ADR-0001](adr/0001-prefer-sliding-sync-with-classic-sync-fallback.md): Sliding Sync preference and classic fallback.
- [ADR-0002](adr/0002-use-sdk-owned-matrix-cryptography-and-explicit-features.md): SDK-owned cryptography and explicit features.
- [ADR-0003](adr/0003-run-untrusted-media-decoding-in-a-webassembly-sandbox.md): WebAssembly decoder with no networking capability, inside OS containment.
- [ADR-0004](adr/0004-separate-the-key-holding-core-from-the-web-view-process.md): the key-holding core runs in its own process; the Tauri process is a keyless UI relay.
- [ADR-0005](adr/0005-share-room-keys-only-with-cross-signed-devices.md): room keys only for cross-signed devices, messages shown only from cross-signed devices, confirmed sends in unencrypted rooms, secrets in the core's windows.
- [ADR-0006](adr/0006-require-an-unlock-secret-for-the-local-profile.md): a profile passphrase (Argon2id, inside DPAPI) to open the local profile, upgrade of DPAPI-only profiles, and a reset confirmed in the core's window.

## Evidence

Run records and supporting artifacts are grouped under [evidence](evidence). The [verification ledger](evidence/verification.json) records the checks, statuses, and source hashes. Historical evidence preserves original command lines and paths as provenance; use the Development guide for current commands and locations.

- [Independent recipients with SAS-verified devices](evidence/verified-recipients.md): two-user verification and bidirectional encrypted messaging on both disposable homeservers.
- [Encrypted native Sliding Sync and classic transition](evidence/encrypted-sliding-sync.md): sync-handler delivery, key/device continuity, and continued encrypted messaging across a completed-operation switch.

- [Encrypted-store restart](evidence/encrypted-restart.md): separate-process session, device-trust and room-key recovery, offline decryption and resumed native encrypted delivery.

- [Capability-selected sync and classic fallback](evidence/capability-fallback.md): real classic-only Synapse, controlled capability origins for both servers, and failure controls with independent request counters.
- [Bounded room and timeline views](evidence/mock-views.md): synthetic data through the core; hostile names and bodies, byte-bounded paging, real-app walkthrough.
- [Native sign-in and real rooms](evidence/matrix-sign-in.md): core-owned Windows prompt, encrypted session, Sliding/classic sync, Matrix views; live runs on Merovingian, Synapse and classic-only Synapse.
- [Failed window retry](evidence/window-retry.md): on Windows, the core offers Retry when the web view's processes exit while the UI runs, or the page sends no request within 30 s; a UI that exits on its own is not retried. This host's crash dumps all fail inside a third-party overlay hook (not tested with it removed).
- [Sign-out](evidence/sign-out.md): remote revocation, key deleted first, crash-safe wipe of the local profile; live runs on Merovingian and Synapse.
- [Encryption setup](evidence/encryption-setup.md): identity-based key sharing, cross-signing and recovery set up from the page with secrets only in the core's windows, recovery-key verification; live runs on Merovingian and Synapse.
- [Encrypted sending](evidence/encrypted-sending.md): outbox with per-room order and same-transaction retries, consent before unencrypted sends; live two-account runs on Merovingian and Synapse with an unsigned-device control.
- [Own-device verification](evidence/own-device-verification.md): emoji comparison with another device of the same account, in both directions and with a mismatch; live runs on Merovingian and Synapse, and why a request from an unknown device is lost.
- [Identity changes](evidence/identity-changes.md): sending pauses while a room member's identity changed (ADR-0007) until the user accepts it in the core's window or verifies it; live runs on Merovingian and Synapse.
- [Contact verification](evidence/contact-verification.md): emoji comparison with another person in an existing direct-message room, in both directions and with a mismatch; live runs on Merovingian and Synapse, lazy members, `m.direct` refresh and Synapse's message rate limit.
- [Session owner](evidence/session-owner.md): lock and unlock while running; leases, generation fencing, retained ownership on timeout; real-app walkthrough.
- [Unlock secret](evidence/unlock-secret.md): profile passphrase with Argon2id inside DPAPI, upgrade of DPAPI-only profiles, wrong-entry limit, native-confirmed reset, Lock help text.
- [Protected local store](evidence/protected-store.md): DPAPI-wrapped store key, core-owned SDK SQLite stores, fail-closed key states, real-app walkthrough.
- [Live room updates](evidence/live-updates.md): content-free, coalescing core events for subscribed rooms; reply routing; real-app walkthrough including page reloads.
- [Web view to host IPC size](evidence/ipc-transport.md): measured 9× host allocation for page payloads up to 256 MiB; decision pending.
- [Tauri shell and IPC permissions](evidence/tauri-shell.md): one allowlisted command, strict CSP, navigation and window policy; mock-runtime and real web view checks.
- [Contained media worker](evidence/media-worker.md): self-checking worker under AppContainer, Job and mitigations; memory and deadline results.
- [WebAssembly decoder sandbox](evidence/wasm-decoder.md): import allowlist, memory/fuel limits, hostile-module tests and 16-megapixel benchmark.
- [SDK task ownership and cooperative stop](evidence/sdk-lifecycle.md): retained Rust task ownership, controlled SDK barriers and the unresolved full SDK shutdown contract.

## Research

- [Slint and iced security research](research/slint-iced-security-research.md): dependency and feature investigation retained after the Tauri decision.
- [GTK and Tauri security-model comparison](research/gtk-tauri-security-model.md): rendering and containment considerations.
