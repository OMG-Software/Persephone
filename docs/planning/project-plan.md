# Project plan

Status: see the [milestone status](../architecture/development-plan.md#milestone-status)
(no milestone has passed its exit gate; P0 to P7 in progress). The
[P0 feasibility workbench](feasibility.md) records completed checks and open gates.
Only the confirmed-requirements section records approved requirements; the remaining design is proposed.

The [architecture and development plan](../architecture/development-plan.md) is the working
implementation guide. This page remains the project brief and scope summary;
its earlier broad milestones are replaced by that plan's P0–P7 acceptance gates.

## Confirmed requirements

- Implement a desktop Matrix chat client in Rust.
- Support Windows, Linux, and macOS.
- Provide a graphical user interface.
- Use Tauri with a Rust core and locally bundled web UI, as recorded in accepted [ADR-0000](../adr/0000-use-tauri-for-the-desktop-client.md).
- Make interoperability with Merovingian a primary acceptance criterion.
- Support general Matrix homeservers, with Merovingian tested first.
- Prefer compatible Sliding Sync by default; fall back to regular `/sync` when unavailable, as recorded in accepted [ADR-0001](../adr/0001-prefer-sliding-sync-with-classic-sync-fallback.md). Merovingian and Synapse are the first integration targets.
- First usable release: login, one account, room list, message history, sending text and images, and end-to-end encrypted chats.
- Start an encrypted direct conversation by exact Matrix ID, with user-directory search behind a switch that is off by default, and accept or decline direct-message invitations; refuse people without a cross-signing identity. Owner decision, 26 October 2026; see [ADR-0014](../adr/0014-start-encrypted-direct-conversations-by-matrix-id.md).
- Treat security as a first-class architectural requirement for high-risk users: include targeted attacks, metadata exposure, and stricter media handling.
- Use TLS 1.3 only for every network connection; refuse TLS 1.2 and earlier. Owner decision, 20 October 2026.
- Treat messages as untrusted, including after decryption. Apply the same policy to server-supplied names, links, avatars, attachments, and other event data.
- Establish the project structure and plan collaboratively with the owner.

Project: Persephone. Repository: [OMG-Software/Persephone](https://github.com/OMG-Software/Persephone).
The repository includes the GNU General Public License v3.0; see [LICENSE](../../LICENSE).

## Decisions to make

1. Product scope: personal client, small-team client, or broader public release.
2. UI implementation: frontend framework, exact Tauri/dependency versions, required features, and license compatibility. The choice of Tauri is settled by ADR-0000.
3. Compatibility baseline: supported Matrix features/versions, the pinned SDK's Sliding Sync variant, mode-switch behavior, and classic fallback; verify against Merovingian, Synapse, and a classic-only/controlled capability deployment.
4. Later feature priorities: multiple accounts, calls, spaces, threads, and other capabilities beyond the approved first release remain undecided.
5. Authentication: required login methods, account count, discovery, and session restoration.
6. Encryption: required first-release behavior for device verification, key recovery, and encrypted attachments.
7. Design: reference applications, layout, themes, accessibility, and keyboard navigation.
8. Integration environment: current Merovingian revision, disposable test instance, and test accounts.
9. Delivery: minimum OS versions, CPU architectures, Linux display environments, installers, signing, and updates.
10. Ownership: distribution and signing responsibilities, repository visibility, and contribution policy.

## Proposed architectural boundaries

The high-risk threat model requires evaluating process and OS-enforced security boundaries before choosing the final crate/process layout. See [security design](../architecture/security-design.md). Source modules alone do not provide isolation.

- UI owns presentation, user input, and view state.
- Application state coordinates sessions and room operations without blocking the UI thread.
- Matrix integration owns SDK calls, synchronization, and protocol errors.
- Platform services own local paths, credential storage, notifications, and OS integration, with explicit permissions and failure behavior.
- The key-holding core runs in its own process; the Tauri web view process is a keyless UI relay (accepted [ADR-0004](../adr/0004-separate-the-key-holding-core-from-the-web-view-process.md)).
- Untrusted media decoding runs outside the process holding long-lived keys, as a WebAssembly module with no networking capability (accepted [ADR-0003](../adr/0003-run-untrusted-media-decoding-in-a-webassembly-sandbox.md)), inside a verified OS sandbox with resource limits.

Use SDK-owned Matrix protocol and cryptography with explicit features, as recorded in [ADR-0002](../adr/0002-use-sdk-owned-matrix-cryptography-and-explicit-features.md). The exact production dependency version, unified features, and complete compatibility/license review remain conditional on P0 evidence.

Use standard Matrix APIs wherever possible. Any Merovingian-specific behavior needs an explicit requirement and a documented compatibility boundary.

## Proposed milestones

Follow the detailed [P0–P7 milestone table](../architecture/development-plan.md#development-milestones-and-acceptance-gates): feasibility; safe scaffold; protected lifecycle/media foundation; login and both sync modes; encrypted text/verification/recovery; image messaging; product hardening; release readiness. P0 has partial investigation evidence; no milestone has passed its full acceptance gate.

Encrypted chats are required for the first release, so identity verification, recovery, session handling, and storage protection must inform the foundation.

## Verification approach

- Build and lint on Windows, Linux, and macOS; record completed results for each platform.
- Exercise application behavior with targeted tests, especially session and sync transitions.
- Use disposable accounts and rooms for live Merovingian integration tests. Keep credentials out of source control and logs.
- Record the exact server revision/configuration used for compatibility claims.
- Include manual UI checks for keyboard use, scaling, accessibility, and operating-system behavior.
- Distinguish planned checks, automated checks, and completed live integration checks.

## Current evidence

- The architectural development plan and accepted ADRs exist. P0 evidence now records selected component and integration-test results; SDK compatibility, protected lifecycle, full storage coverage, and platform sandbox acceptance still require the remaining planned checks.
- [Slint/iced research](../research/slint-iced-security-research.md) inspected pinned release sources, generated research lockfiles, traced dependency graphs for three representative targets, and checked OSV/RustSec advisories. Neither framework is approved; no GUI was built or tested.
- The project directory was empty at the initial inspection and was not a Git repository.
- Local toolchain: rustc 1.99.1, Cargo 1.99.1, stable Windows MSVC target.
- Tauri is the accepted desktop framework; the shell pins Tauri 2.12.4 with a Svelte frontend (see the [Tauri shell evidence](../evidence/tauri-shell.md) and [frontend evidence](../evidence/frontend.md)). Slint/iced dependencies were downloaded for isolated framework research; those files are not an application scaffold.
- The current Rust test programs exercise SDK behavior, local stores, aggregate resources, and a Windows sandbox candidate; the frontend checks escaping and builds static assets. Disposable Merovingian 0.12.29 and Synapse 1.163.0 smoke tests passed. The initial Merovingian FORTIFY failure was resolved with a separately built optimized/hardened binary and a capability-restricted launch. See the workbench for exact results and limitations. The application has since been built and tested on Windows only, in debug builds: the core and UI processes, protected store, native sign-in, sync and real rooms (see the [milestone status](../architecture/development-plan.md#milestone-status)). No milestone has passed its exit gate, and Linux, macOS and release builds are untested.
