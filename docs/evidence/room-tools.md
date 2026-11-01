# Room tools: verification record

1 November 2026. The coordinated development check completed with exit 0.
Native UI, live homeserver and platform acceptance remain open. The
[user guide](../room-tools.md) describes the feature limits. ADR-0032 to
ADR-0035 remain proposed; this result does not accept their technical designs.

Later correction, 1 November 2026: the lock/sign-out wording below now separates
frontend invocation from core completion. The original run identity and source
manifest are retained. A [separate review-fix run](room-tools-review-fixes.md)
records the later implementation and regression checks.

## Run identity

| Field | Value |
| --- | --- |
| Branch | `codex/drafts-abuse-search-room-sections` |
| Base commit | `393c8e5290c5ae7006f5709a1e55c3f02098084b` plus uncommitted feature changes |
| Tested sources | [57-file source manifest](room-tools-sources.json), including changed code/tests, generated contracts/permissions, locked manifests and the check runner |
| Manifest SHA-256 | `52cfa7756bb7d88c2df10a43f8643097cb23a8d10dbd34ecb6b39d342d3ced73` |
| Cargo.lock SHA-256 | `a3e9b1bfb9026a4b20fcc50b4859a68297e8e60ea28bf49e8604f0ccb57ff129` |
| frontend/package-lock.json SHA-256 | `7588fe64addd065bd5b358e92001923bb7b2496bac0925a67637d6339ae7a89a` |
| OS and build | Windows x64, OS version 10.0.26300.0; debug/test builds, release Wasm guest |
| Tools | Rust/Cargo 1.99.1, Node 24.15.0, npm 11.13.0, Python 3.14.4 |
| Matrix SDK | Pinned 0.19.1; no dependency version or feature change |
| Raw output | Ignored `.local/runs/room-tools/check.log`; earlier coordinated failures in `check-attempt1.log` and `check-attempt2.log` beside it |

## Completed checks

Command: `python tools/dev.py check`, from the repository root with the
Visual Studio 18 Community x64 developer environment imported. The sandbox
blocked native archive replacement with MSVC LNK1114 error 5. The approved
execution override allowed the build and tests to complete. No dependency
or compiler-policy workaround was added to the project.

| Check | Observed result |
| --- | --- |
| Coordinated development check | Exit 0 |
| Rust format, guest/worker builds, workspace and development-feature Clippy | Pass; Clippy used `-D warnings` |
| Rust tests | 757 successful test executions, 0 failed; 2 existing image benchmarks ignored |
| Frontend | Svelte check: 0 errors/warnings; production build passed; 170 Node tests passed, 0 skipped/cancelled |
| Contracts | Generated DTO drift check and TypeScript contract check passed inside the coordinated command |
| Python harnesses | 2 server-registration tests and 50 tool/controller tests passed |

Counts describe executions, not unique cases: the runner repeats core tests
with `dev-disposable-servers`. The two ignored largest-image benchmarks need
`PERSEPHONE_BENCH_DIR` and a release build. They are not credited as passes.
The [verification ledger](verification.json) records this run as
`workspace_check_after_room_tools`.

The first coordinated attempt failed the exact IPC permission assertion,
which had not yet listed the nine new commands. The second failed two Clippy
lints. Both were corrected before the completed passing run. Earlier focused
runs and interrupted builds do not replace this final result.

## Behavior covered

- Encrypted draft/preferences reopen, room/profile isolation, wrong-key
  refusal, synthetic marker absence, corrupt-record refusal, UTF-8/control/ID
  bounds, aggregate and JSON limits, and reclamation of bounded room slots.
- Draft save ordering, delayed restoration, failed restoration without an
  empty overwrite, edits kept separate from new-message drafts, profile
  changes, capacity recovery through inactive-draft removal, and leave-time
  handling. The frontend starts lock or sign-out without waiting for its
  draft-save promise. These tests use a mock action; they do not prove core
  completion during a stalled write. Admitted writes hold a session lease,
  and the core waits for those leases. A timeout can leave it in `locking`.
- Local search traverses an encrypted SQLite cache fixture with 17 chunks
  of 128 events. It stops at 2,000 scanned events and 50 results. An explicit
  SDK HTTP control increments the request recorder; searching then leaves the
  count unchanged. No live homeserver is involved in this check.
- Search checks edits by sender, message type, timestamp and event-ID tie,
  redactions, reply fallbacks, ignored users, snippets, duplicate events, and
  omission of a room with an oversized cached event.
- Loopback HTTP fixtures check successful and refused ignore writes, immediate
  local filtering, the complete 101-user ignore set with a truncated display,
  malformed-state refusal, and exact report request fields. Timeline filtering
  preserves state events and redactions from ignored senders.
- IPC tests check the exact permission list, approved local commands, denied
  remote origins, nested-field rejection, bounds before relay, and strict replies.
- Frontend tests check grouping, bounded search navigation, stale results,
  inactive-draft management, and inert rendering of hostile text.

## Limits and open gates

No native UI or screen-reader walkthrough, live Merovingian/Synapse run,
Linux/macOS runtime check, release package test, dependency audit, or separate
native containment gate was run for this feature slice. Python harness output
that names Merovingian or Synapse tests the harness; it is not a new live-server
result.

Search coverage is partial. Separate chunk reads are not an atomic snapshot,
and missing cached events can hide later changes. The SDK cache subscription
adds background work; it does not close the [SDK drain gap](../architecture/sdk-lifecycle.md).
The `restart_required` limitation and existing containment, storage, platform
and independent-review gates remain open. These passing checks do not establish
production readiness.
