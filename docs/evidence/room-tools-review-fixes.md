# Room tools: review fixes

1 November 2026. Local verification completed with exit 0. This report records
the fixes after the [original feature run](room-tools.md). It does not replace
that run or establish a passing remote review.

## Changes and regression coverage

- SonarQube reported `rust:S9334` for `RoomDraft.reply_to` and
  `RoomPreference.section`. Explicit input types now accept a present string or
  null and reject a missing field. Adding a default would weaken the required
  nullable IPC contract. Tests cover round trips, missing, wrong-type, duplicate
  and unknown fields. Exported TypeScript bindings remain unchanged.
- Search navigation stops before an older page exceeds 500 retained items. It
  keeps recent items, visible gaps and the input cursor for the unretained page.
  An oversized initial context starts from a fresh newest page within the same
  five-request budget. Tests cover initial counts 1 through 99, overlapping
  pages, retry cursors, oversized input and stale results.
- A successful ignored-user list reload clears its previous error. Tests cover
  failure followed by success and results received after the dialog closes.
- The lock/sign-out documentation now separates frontend invocation from core
  completion. Admitted writes hold session leases; timeout can leave the core
  in `locking`. The mock frontend test does not establish a full core drain.
- The Windows ignore-limit test checks zero requests to the ignore account-data
  endpoint, with an explicit unrelated concurrent HTTP control. It permits
  unrelated SDK background work. An initialization waiter is not a full drain.
- Function comments explain persistence, stale-result guards, input bounds and
  disclosures. CodeRabbit's remote docstring metric still needs a new run.

## Run identity

| Field | Value |
| --- | --- |
| Branch | `codex/drafts-abuse-search-room-sections` |
| Base commit | `9e79063fa35d9a31fbd23bedce33d81f090e88f0` plus review fixes |
| Source identity | [Review-fix source manifest](room-tools-review-fixes-sources.json); original source manifest is unchanged |
| OS and build | Windows x64, OS 10.0.26300.0; debug/test builds and release Wasm guest |
| Tools | Rust/Cargo 1.99.1, Node 24.15.0, npm 11.13.0, Python 3.14.4 |
| Dependencies | No version, lockfile or feature change |
| Raw output | Ignored `.local/runs/room-tools/review-fixes-check.log` and `review-fixes-frontend-final.log` |

The coordinated command used the Visual Studio 18 Community x64 developer
environment and the approved native-build execution override. The source
manifest identifies the final code. Only frontend function comments changed
after the coordinated run; the final frontend rerun includes those comments.
Evidence and guide edits followed the runtime checks.

## Completed checks

| Command | Observed result |
| --- | --- |
| `python tools/dev.py check` | Exit 0; 758 successful Rust test executions, 178 frontend tests, 2 server-registration tests and 50 tool/controller tests |
| Included Rust checks | Format, Wasm guest and worker builds, workspace and development-feature Clippy with `-D warnings` passed |
| Included frontend and contracts | Svelte: 0 errors/warnings; production build, Node coverage, DTO drift and TypeScript contract checks passed |
| `python tools/dev.py frontend` after final comments | Exit 0; Svelte: 0 errors/warnings; build and all 178 tests passed |
| Documentation checks | JSON syntax, source hashes, local Markdown links and diff whitespace checked |

The 988 passing executions are not unique cases: the wrapper repeats core
tests with `dev-disposable-servers`. Two existing image benchmarks were ignored
and are not passes. The exporter emits existing `ts-rs` warnings about unsupported
`deny_unknown_fields` metadata; runtime deserialization enforces that rule and
the generated-binding drift check passes.

## Limits

No live Merovingian/Synapse run, native UI or accessibility walkthrough,
Linux/macOS runtime check, native containment gate, release package test or
dependency audit was run. The SDK drain and restart-required limits remain.
The new local tests do not establish remote Windows CI, SonarQube gate or
CodeRabbit approval. These require checks of the pushed revision.
