# P0 cooperative sync drain probe

> Historical run record: commands and source paths below describe the pre-restructure checkout. See [Development](../development.md) for current paths and commands.

**Date:** 2026-10-19  
**Status:** Narrow SDK-backed investigation; production lifecycle remains unimplemented and P0 drain remains open.

## Why this probe exists

The architecture requires a session owner to stop admitting protected work, stop and await the current sync engine and related processing, and only then start another sync mode or allow another store owner. The pinned Matrix SDK 0.19.1 investigation already showed a response handler can outlive an aborted outer sync task. That makes `abort()` followed by awaiting the outer `JoinHandle` insufficient evidence of drain. See [P0 Matrix evidence](matrix-sync.md) and the [lifecycle and switching requirements](../architecture/development-plan.md).

This probe adds one integration test at `probes/p0/matrix/tests/drain.rs`. It uses the pinned SDK's real `SlidingSync::sync_once()` and a synthetic Wiremock homeserver. It adds no production API and uses no real account, credential, persistent store, or E2EE extensions.

## Probe behavior

The test-only supervisor retains the spawned sync future in a `JoinHandle` and holds an admission lease for that engine. Its small gate has bounded active work (two leases in this fixture) and phases for running, draining, timed out, and stopped. A lock protects only brief gate state updates; no SDK client is behind a shared mutex.

The fixture returns a synthetic Sliding Sync room/message response. The registered SDK event handler acquires a work lease and pauses on `Notify`. The test waits until that handler is active, requests stop with a 40 ms deadline, and observes a timeout. The gate rejects further work and refuses to start a Classic replacement while the handler and retained sync future are still owned. The test then releases the handler, retries stop, waits for the work lease and supervised sync future to finish, and confirms that a Classic replacement task can start and complete. The test uses notifications and the bounded stop deadline instead of sleeps to establish ordering.

The lease is released only when the admitted handler returns or the supervised engine future completes. If reading a large SDK background task graph requires more than this tracked work, this gate cannot prove that untracked task is complete. A production owner must identify and supervise every relevant SDK-owned task and the selected SDK-UI sync service lifecycle before treating this as a sufficient drain design.

The probe's `Supervisor::drop` closes new admission. Rust async `Drop` cannot await: dropping its Tokio `JoinHandle` detaches that task, so the task continues without a handle to join it. The gate remains in a protected draining/timed-out phase while a tracked lease retains it, and no replacement can start through that gate. This is only an admission-closure fallback; it does not establish drain, report eventual completion, or authorize releasing a store. The pinned Tokio 1.53.3 `JoinHandle` source documents this behavior at `tokio-1.53.3/src/runtime/task/join.rs` in the configured Cargo cache.

## Verification record

Environment: Windows, Rust/Cargo 1.99.1, Matrix Rust SDK 0.19.1 from the locked probe. Cargo used `.local/p0/cargo-home` and `.local/p0/matrix-target`; the installed LLVM librarian path used by the existing matrix probe avoided its MSVC archive-update failure.

| Command | Exit | Result |
| --- | ---: | --- |
| `cargo fmt --manifest-path probes/p0/matrix/Cargo.toml -- --check` | 0 | Formatting clean. |
| `cargo test --locked --manifest-path probes/p0/matrix/Cargo.toml` | 0 | 9 existing SDK unit tests and the new drain integration test passed; 10 total, 0 failed. |
| `cargo clippy --locked --manifest-path probes/p0/matrix/Cargo.toml --all-targets -- -D warnings` | 0 | All targets pass with compiler/lint warnings denied. |

Cargo printed warnings that hard links were unavailable in the isolated incremental build cache and it copied files instead. Commands completed successfully. The first targeted run rejected the longer test-only Sliding Sync identifier; changing it to the SDK-accepted `p0-drain` identifier fixed the fixture, and the final full locked run passed.

## Limits and remaining work

This test covers one paused event handler associated with a single low-level Sliding Sync request. The replacement task is a gate/admission exercise; it is not a real Classic Sync start. The fixture does not establish how every SDK event/cache task, to-device or encryption-sync task, pagination request, media worker, store operation, or SDK-UI service behaves during shutdown. It does not cover persistent stores, multiple OSes, process crashes, or a full lock/logout sequence.

The test demonstrates that a cooperative owner can keep a future and admitted application work alive across a bounded stop timeout, refuse replacement during that time, and admit a replacement after tracked work actually finishes. It does not close the SDK-wide lifecycle question. Production switching and lock/logout remain blocked on task inventory, SDK-supported stop behavior, real SDK-UI service tests, store ownership checks, and cancellation/drain coverage for related work. A timeout must remain a protected Draining/TimedOut state; dropping the owner cannot turn it into success.

**Assessment:** The stop protocol can be tested behaviorally for this one SDK handler and retained sync future; it is not sufficient evidence for total SDK drain.  
**Confidence:** 91% for the integration test's observed sequence; low for complete production drain because the SDK-wide task inventory and service lifecycle remain unverified.