# Project plan

Status: initial discussion, 19 October 2026. Only the confirmed-requirements section records approved requirements; the remaining design is proposed.

The [architecture and development plan](architecture-plan.md) is the working
implementation guide. This page remains the project brief and scope summary;
its earlier broad milestones are replaced by that plan's P0–P7 acceptance gates.

## Confirmed requirements

- Implement a desktop Matrix chat client in Rust.
- Support Windows, Linux, and macOS.
- Provide a graphical user interface.
- Use Tauri with a Rust core and locally bundled web UI, as recorded in accepted [ADR-0000](adr/0000-use-tauri-for-the-desktop-client.md).
- Make interoperability with Merovingian a primary acceptance criterion.
- Support general Matrix homeservers, with Merovingian tested first.
- Prefer compatible Sliding Sync by default; fall back to regular `/sync` when unavailable, as recorded in accepted [ADR-0001](adr/0001-prefer-sliding-sync-with-classic-sync-fallback.md). Merovingian and Synapse are the first integration targets.
- First usable release: login, one account, room list, message history, sending text and images, and end-to-end encrypted chats.
- Treat security as a first-class architectural requirement for high-risk users: include targeted attacks, metadata exposure, and stricter media handling.
- Treat messages as untrusted, including after decryption. Apply the same policy to server-supplied names, links, avatars, attachments, and other event data.
- Establish the project structure and plan collaboratively with the owner.

Project: Persephone. Repository: [OMG-Software/Persephone](https://github.com/OMG-Software/Persephone).
The repository includes the GNU General Public License v3.0; see [LICENSE](../LICENSE).

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

The high-risk threat model requires evaluating process and OS-enforced security boundaries before choosing the final crate/process layout. See [security design](security-design.md). Source modules alone do not provide isolation.

- UI owns presentation, user input, and view state.
- Application state coordinates sessions and room operations without blocking the UI thread.
- Matrix integration owns SDK calls, synchronization, and protocol errors.
- Platform services own local paths, credential storage, notifications, and OS integration, with explicit permissions and failure behavior.
- Untrusted media decoding runs outside the process holding long-lived keys, subject to a verified OS sandbox and resource limits.

Evaluate an existing Matrix SDK before considering protocol or cryptographic implementation. Dependency versions, features, and compatibility with the repository's GPLv3 license remain to be verified.

Use standard Matrix APIs wherever possible. Any Merovingian-specific behavior needs an explicit requirement and a documented compatibility boundary.

## Proposed milestones

Follow the detailed [P0–P7 milestone table](architecture-plan.md#development-milestones-and-acceptance-gates): feasibility; safe scaffold; protected lifecycle/media foundation; login and both sync modes; encrypted text/verification/recovery; image messaging; product hardening; release readiness. Every milestone is currently unimplemented and unverified.

Encrypted chats are required for the first release, so identity verification, recovery, session handling, and storage protection must inform the foundation.

## Verification approach

- Build and lint on Windows, Linux, and macOS; record completed results for each platform.
- Exercise application behavior with targeted tests, especially session and sync transitions.
- Use disposable accounts and rooms for live Merovingian integration tests. Keep credentials out of source control and logs.
- Record the exact server revision/configuration used for compatibility claims.
- Include manual UI checks for keyboard use, scaling, accessibility, and operating-system behavior.
- Distinguish planned checks, automated checks, and completed live integration checks.

## Current evidence

- The architectural development plan and two accepted ADRs exist. Implementation recommendations, SDK compatibility, credential/storage behavior, and platform sandbox mechanisms still require the planned verification.
- [Slint/iced research](slint-iced-security-research.md) inspected pinned release sources, generated research lockfiles, traced dependency graphs for three representative targets, and checked OSV/RustSec advisories. Neither framework is approved; no GUI was built or tested.
- The project directory was empty at the initial inspection and was not a Git repository.
- Local toolchain: rustc 1.99.1, Cargo 1.99.1, stable Windows MSVC target.
- Tauri is the accepted desktop framework; its exact version and frontend dependencies remain undecided. Slint/iced dependencies were downloaded for isolated research probes; these are not an application scaffold.
- No application builds, tests, or server integration checks have run.
