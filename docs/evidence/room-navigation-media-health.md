# Room navigation, attachments and connection health

1 November 2026. The `python tools/dev.py check` run on the final source completed
with exit 0. Native UI and live homeserver acceptance remain open.

## Behavior and regression coverage

- Date navigation uses the standard timestamp endpoint, then bounded event
  context. Only HTTP 404 permits a backward lookup. Unsupported requests fail
  without a history scan. Tests check fallback, target identity, cursor scope
  and the complete serialized response budget.
- Pins use fresh shared room state and current state permissions. Tests cover
  denied writes, preserving existing pins, duplicate references and the limit
  of 100 references. Pin state is unencrypted metadata. Concurrent state
  writers have no compare-and-swap guarantee.
- Attachment browsing requests at most 50 raw history events per page and
  returns metadata only. Tests check empty filtered pages with continuation,
  opaque cursors and moderator redactions outside the sender/date filters.
  The UI deduplicates overlapping pages and retains at most 500 attachments.
- Local search filters exact sender and inclusive/exclusive original-event
  timestamps after edit and redaction reconstruction. An HTTP recorder checks
  that filtered cache search sends no request. Existing scan/byte limits remain.
- Connection health records existing sync outcomes and adds no probe or poll.
  Tests cover failure/recovery, saturation, lock/reset and stale-generation
  refusal. Local-clock observations do not prove current connectivity.
- Contract, protocol and host tests check strict fields, safe timestamps,
  explicit nullable values, input bounds and allowlisted IPC. Frontend tests
  cover bridge arguments, filter validation, metadata bounds and inert rendering.
  Focused date/pin context suppresses read markers and is cleared on room
  invalidation. These tests do not replace a native interaction walkthrough.

## Run identity

| Field | Value |
| --- | --- |
| Branch | `codex/room-navigation-media-health` |
| Base | `ba09f42a97ec7fa981c2279fb313cc31a1544ac1` plus the source manifest |
| Source | [Normalized source hashes](room-navigation-media-health-sources.json) |
| Manifest SHA-256 | `0d5c8c234306f90f5f151774e3dc5658569796ca72113111672f9e7fbf9a7a22` |
| Host | Windows x64, OS 10.0.26300.0; debug/test builds and release Wasm guest |
| Tools | Rust/Cargo 1.99.1, Node 24.15.0, npm 11.13.0, Python 3.14.4 |
| Dependencies | No dependency version, feature or lockfile change |
| Raw output | Ignored `.local/runs/room-navigation-media-health/check-verified.log` |

The run used the Visual Studio 18 Community x64 developer environment and the
approved native execution override. The source manifest records the final
runtime/test files. Guide and evidence edits followed the checks.

## Completed checks

| Check | Observed result |
| --- | --- |
| `python tools/dev.py check` | Exit 0; 798 successful Rust test executions, 189 frontend tests, 2 server-registration tests and 50 tool/controller tests |
| Rust checks included | Format, guest/worker builds, workspace and development-feature Clippy with `-D warnings` passed |
| Frontend and contracts included | Svelte 0 errors/warnings; production build, Node coverage, generated-binding drift and TypeScript contract checks passed |
| Documentation checks | JSON, source hashes, local Markdown links and diff hygiene checked separately |

The 1039 passing executions are not unique cases. The wrapper repeats core
tests with `dev-disposable-servers`. The 2 ignored existing image benchmarks
are not passes. Existing `ts-rs` warnings concern unsupported Serde metadata;
runtime rejection tests and generated-binding drift checks passed.

## Earlier attempts and corrections

The first focused compile caught a Wiremock builder ordering error and a
missing search-filter test argument. Both were corrected. A sandboxed focused
run failed 15 cases with store/file-access failures; the native override run
completed all 207 core library tests. Raw logs are retained as
`core-focused-first-compile.log`, `core-focused-sandbox.log` and
`core-focused.log` in the ignored run directory.

The first full check stopped at four Clippy errors. The corrected run
`check-final.log` completed with exit 0. During that run, review found that
an attachment reply accepted a missing cursor field. Explicit required-nullable
decoding and missing/null/wrong/duplicate-field tests corrected the contract.
The next passing run, `check-cursor.log`, includes that correction. Final review
also found duplicate pin IDs could break keyed UI rows. The core now removes
duplicates before fetching previews, and a regression checks one reference
and one fetch. `check-verified.log` is the complete rerun with both corrections
fixed before the command started. Earlier outcomes are not substituted for
this result.

## Limits

No native UI/accessibility walkthrough, live Merovingian/Synapse run,
Linux/macOS runtime check, separate native containment gate, release package
test or dependency audit was run. Mock loopback homeservers are synthetic;
Python harness output is not a live homeserver result. History and cache
coverage remain partial. The existing SDK drain and `restart_required` limits
remain unresolved. These checks do not establish production security readiness
or remote CI/reviewer approval.
