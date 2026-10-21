# P0 feasibility workbench

Started 19 October 2026. **P0 is in progress; its exit gates have not passed.**
The shared crates and integration/security test programs turn architecture
assumptions into bounded checks. They do not constitute the P1 application
scaffold and must not handle real chats.

The [verification ledger](../evidence/verification.json) records passing,
failing and unrun checks with source hashes. The current root workspace check
passed 175 tests (152 Rust, 8 frontend, 2 server-registration and 13 controller tests), along with formatting, Clippy,
TypeScript, generated-contract drift, and frontend build checks. The dependency
audit completed against the single root lockfile. The recorded live smoke passed
on both disposable homeservers; the native sandbox gate remains a failure, now
for a decisive reason: the tested Windows worker configurations are not
network-denied.

Individual evidence reports preserve command lines and hashes from before the
repository restructure. Their old paths identify those run inputs and do not
describe current lockfile or build-output locations. Current sources are grouped
under `crates/`, `tests/`, `frontend/`, and `tools/`, with one root Cargo
workspace lockfile. Use the [Development guide](../development.md) for current
commands and paths.

## Evidence and status

| P0 requirement | Recorded evidence | Gate status |
| --- | --- | --- |
| Versioned frontend/SDK candidates | [Registry candidates](../evidence/versions.json), [frontend](../evidence/frontend.md), [dependencies](../evidence/dependencies.md) | Exact candidate pins exist; production adoption conditional |
| Inert local UI | Svelte static build/type checks and 5 frontend tests pass, including bridge-only host access and no raw HTML; npm audit reports 0 advisories. [Tauri shell](../evidence/tauri-shell.md) runs in WebView2 under a strict CSP (no inline script, no string eval, no external fetch, navigation and new windows blocked) | Accessibility, keyboard and release/packaged runs pending |
| Bounded views | [Synthetic room list and timeline](../evidence/mock-views.md) served by the core: names projected (bidi/controls replaced, 128 chars), bodies capped at 16 KiB and direction-isolated, pages bounded by 100 items and 112 KiB; complete back-paging verified in the real app. [Live updates](../evidence/live-updates.md): content-free events coalesce per subscribed room (at most 16), so pending state stays bounded | Real Matrix data, room-list events, keyboard/screen-reader verification pending |
| Sliding default and classic fallback | [Initial SDK/live sync checks](../evidence/matrix-sync.md) and [native encrypted sync with a completed-operation classic switch](../evidence/encrypted-sliding-sync.md) pass on both servers | [Initial capability selection](../evidence/capability-fallback.md) passes on both controlled origins and real classic-only Synapse; runtime renegotiation, cache freshness and crash recovery pending |
| Encrypted message capability | [Native Sliding Sync and classic regression](../evidence/encrypted-sliding-sync.md): independent users, mutual SAS device verification, exact decrypted sync callbacks in both directions, key/identity/trust continuity after transition and both logouts pass on both servers | Simulated trusted out-of-band comparison; [normal-process restart](../evidence/encrypted-restart.md) passes; human UI, cross-signing, crash recovery and product sharing policy pending |
| SDK concurrency/lifecycle | Both servers pass sync/history/key-query scheduling and completed-operation switch; [retained task ownership and SDK barrier tests](../evidence/sdk-lifecycle.md) cover timeout, cancelled waits, stale callbacks and cooperative frame completion | Pinned task inventory exposes gaps in public SDK completion APIs; full lock/logout/store handoff remains blocked. Tracked completion, pause and outer cancellation do not establish SDK-wide drain |
| Credential/store protection | [7 storage tests](../evidence/storage.md): generated Olm account signatures and configured crypto/state/event/media/send-queue markers absent in encrypted DB/WAL/SHM, with a positive plaintext control; wrong key fails; no-key path permits plaintext writes | [Missing-key guard and separate-process key/trust recovery](../evidence/encrypted-restart.md) pass in the fixture; production unlock, broader records/artifacts, corruption/crashes and other OS vaults pending. [Core-owned profile](../evidence/protected-store.md): DPAPI-wrapped random key, exclusive profile lock, missing/unreadable/rejected keys leave the profile locked and unchanged; same-user access, unlock prompt, lock/logout and Linux/macOS vaults pending |
| OS media containment | [Windows test program](../evidence/windows-sandbox.md): environment fixed, 26 regression tests pass; current native run verifies paths, exact file denials, token, child/allocation limits and broker deadline. [Endpoint-observed network differential](../evidence/windows-sandbox.md#network-denial-differential) is decisive across three worker policies | **No tested Windows policy denies the kernel socket.** Zero-capability AppContainer is not network-denied: worker packets reach the TCP stack (closed ports refuse it), listener-side filtering blocks observed connections and datagrams, and port state leaks; it opens the AFD device. LPAC breaks Winsock initialization but still opens the AFD device. An AppContainer + restricted token (write-restricted, untrusted integrity) cannot initialize at all (`STATUS_DLL_INIT_FAILED`), yielding no running worker. Socket-denying configuration or residual-risk decision, CPU-time cap, signed packaging/Linux/macOS pending |
| WebAssembly decoder sandbox | [ADR-0003](../adr/0003-run-untrusted-media-decoding-in-a-webassembly-sandbox.md), [evidence](../evidence/wasm-decoder.md): `wasmi 2.0.0` guest with exactly three host imports; 22 tests cover WASI/foreign imports, signatures, validation, fuel, memory caps, traps and output violations; 16 MP decodes byte-identical to native | Not yet inside the OS worker or the media flow; worst 16 MP decode 7.2 s of the 10 s deadline on this host (3× faster with tail-call dispatch); memory budgets need reconciling; decoder/host fuzzing and Linux/macOS pending |
| Contained media worker | [Evidence](../evidence/media-worker.md): worker self-checks AppContainer, Job, dynamic-code and Win32k policies before reading input; broker limits inheritance to two pipes, enforces the deadline and reads Job peak memory; 8 new broker tests | All 4000 × 4000 test images peak at 178–194 MiB of 256 MiB; a maximum-size progressive 4:4:4 JPEG exceeds the 10 s deadline (decision pending); worker syscall/file/handle probing, CPU-limit demonstration, packaging and Linux/macOS pending |
| Aggregate resources and framed IPC | [10 dependency-free tests](../evidence/media-protocol.md) pass | Candidate budgets only; Tauri pre-body cap and real process/copy measurements pending |
| Public DTO generation | [4 Rust tests, generated fixtures and TypeScript checks](../evidence/contracts.md) pass with explicit `ts-rs 12.0.1` | Candidate feasible; runtime validation and actual Tauri command wiring pending |
| Tauri transport and diagnostics | [Pinned 2.12.4 source review](../evidence/tauri-ipc.md) establishes parsing/allocation order and optional payload tracing. [Shell slice](../evidence/tauri-shell.md): one allowlisted command; 14 core plugin commands, unregistered commands, remote origins and forged invoke keys refused in mock-runtime tests and the real web view; `tracing` off | [Measured](../evidence/ipc-transport.md): a page makes the host allocate about 9× any payload up to 256 MiB (2.4 GB), on the custom-protocol and `postMessage` paths, unauthenticated; `postMessage` cannot be bounded host-side. Owner decision: separate key-holding core ([ADR-0004](../adr/0004-separate-the-key-holding-core-from-the-web-view-process.md)), [implemented](../evidence/ipc-transport.md#after-the-coreui-split): floods of 64–256 MiB leave the core's memory unchanged and stop at the UI's Job limit. [Core→UI events](../evidence/live-updates.md) share the pipe as whole frames. Linux/macOS controls pending |
| Exact dependency advisory review | [OSV result](../evidence/rust-advisories.json), npm locked audit, [maintenance disposition and compiled-graph review](../evidence/dependencies.md#maintenance-disposition-and-compiled-graph-review) | `derivative` is not compiled for our features; `anymap2` is retained with regression tests until a matrix-sdk release ships the merged anymap3 switch. Windows x64 graph: one native crypto provider, MPL-2.0 crates GPL-compatible by notice review. TLS 1.3-only policy decided and enforced with wire-level tests; other-target graphs, file-level native licenses and SBOM pending |

The Windows host was independently identified as **Windows 11 Pro**, kernel
version 10.0.26200. WSL Ubuntu 24.04 ran both disposable homeservers; that is
not a desktop Linux sandbox/webview validation. No macOS runtime was available.
Minimum supported OS/CPU versions remain undecided until packaged tests exist.

## Decisions carried forward

* Retain Tauri and native Sliding Sync preference as accepted by ADR-0000/0001.
* Evaluate stable **Tauri 2.12.4 / tauri-build 2.7.3**, not the Tauri 3 alpha
  returned as the registry's newest version. These Tauri versions were queried,
  not built or security-approved in this run.
* [ADR-0002](../adr/0002-use-sdk-owned-matrix-cryptography-and-explicit-features.md)
  accepts SDK-owned Matrix cryptography and explicit feature selection. Use
  **Matrix Rust SDK 0.19.1** for these compatibility test programs with explicit
  feature selection. SDK defaults include automatic room-key forwarding; the
  Matrix integration test disables defaults. Product key-sharing policy remains a reviewed
  decision rather than an inherited feature default.
* Continue evaluating **Svelte 5.57.2 / Vite 8.3.4 / TypeScript 6.0.3**. The newest
  TypeScript 7 was excluded because the checked `svelte-check` peer range accepts
  TypeScript 5/6. Locks record exact dependencies; no frontend security approval
  follows from SSR escaping alone.
* Keep worker pixels in bounded binary frames and reserve bytes before body
  allocation. Prototype payload ceilings total 200 MiB within a 256 MiB
  admission ceiling, leaving 56 MiB unadmitted headroom. Runtime/allocator/
  webview copies and decoder scratch are additional memory requiring measurement.
* Keep real account data out until unlock, store coverage, process containment,
  and lifecycle gates have concrete evidence. Missing secrets never select an
  unencrypted store. Caller-supplied SQLite key names must not contain secrets.

## Next acceptance work, in order

1. Prove cooperative SDK drain across response processing, encryption tasks,
   lock/logout and mode switching, including failure/interruption cases.
   [Tracked ownership tests and the pinned task inventory](../evidence/sdk-lifecycle.md)
   now pass their bounded scenarios; the [remaining SDK completion contract](../architecture/sdk-lifecycle.md)
   requires supported stop-and-join ownership of SDK workers before production
   lock or store handoff. This acceptance item remains open.
   [Initial capability selection and classic fallback](../evidence/capability-fallback.md)
   now pass, including a real classic-only Synapse and controlled origins for
   both servers. Runtime unsupported-endpoint reclassification and replacement
   still require reviewed failure policy and safe lifecycle ownership.
2. [Decisive Windows network evidence](../evidence/windows-sandbox.md#network-denial-differential)
   now exists and is negative across three worker policies: neither AppContainer
   nor LPAC denies the kernel socket device, and an AppContainer combined with a
   restricted token (write-restricted to the RESTRICTED SID, untrusted integrity)
   cannot initialize — the LowBox worker fails DLL initialization
   (`STATUS_DLL_INIT_FAILED`) before any probe runs, so it yields no socket
   verdict. The restricting-SID set tight enough to deny the AFD device is also
   too tight for the loader to boot; a set loose enough to boot grants the
   device. The remaining options are a different socket-denying mechanism (for
   example a listener/WFP-based egress control owned by the broker, or a
   kernel-level filter) or an owner-approved residual-risk decision for
   listener-side filtering plus the port-state oracle. Verify CPU accounting and
   packaged behavior; establish signed helper policies and equivalent
   denial/resource tests on Linux/macOS.
3. [Maintenance disposition](../evidence/dependencies.md#maintenance-disposition-and-compiled-graph-review)
   is recorded: `derivative` is unreachable for our features, and `anymap2` is
   retained until the next matrix-sdk release adopts upstream's merged anymap3
   switch. TLS 1.3 only is decided and tested. Resolve the Linux/macOS graphs,
   review file-level licenses in `aws-lc-sys` and bundled SQLite, and produce
   an SBOM.
4. Carry the standalone DTO generator into real Tauri command wiring and establish
   transport containment before application deserialization. Keep the current UI
   choices; settle remaining SDK/OS-baseline decisions with evidence, then begin
   the P1 shell. Production unlock, cross-signing, human verification and
   crash/corruption recovery remain separate requirements.

## Reproduction and artifact policy

See the [Development guide](../development.md) for current commands and evidence links.
The root Cargo lock and frontend package lock belong in source control. Build outputs,
downloaded sources, virtual environments, synthetic server credentials/profiles,
and unredacted logs stay in ignored `.local/` or ignored local caches.
The published JSON results contain no server credentials or real chat data.

A failed, interrupted or unrun check is not a pass. P0 remains open even though
individual checks are useful and passing. No production security or three-OS
compatibility claim is made by this workbench.
