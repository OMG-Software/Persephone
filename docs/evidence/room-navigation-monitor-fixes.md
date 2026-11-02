# Room navigation: monitored PR 47 fixes

2 November 2026. Final `python tools/dev.py check` completed with exit 0.
This supplements the [earlier review fixes](room-navigation-review-fixes.md).

## Verified findings and changes

- Pin previews break equal-timestamp edit ties by event ID. A regression runs
  both response orders and requires the same latest text.
- The panel/add limit remains 100 pins. Removal from existing lists can retain
  all other IDs within a 512-ID state limit and 112 KiB replacement-body limit.
  Tests check a 101-ID removal, refused additions and oversized state/payloads.
- Sender filters validate the server part and optional port before submission.
  Pin action labels include the complete bounded event ID, so identical
  previews remain distinct. Tests cover invalid ports/IPv6 and repeated text.
- Evidence JSON rejects `NaN`, `Infinity` and `-Infinity`. Titled and
  angle-bracket Markdown links are checked. The whitespace PASS text now
  states its actual scope: unstaged tracked changes. Staged whitespace is
  checked separately before commit.

## Security outcomes

**Fixed: Git option injection in the evidence checker.** `--revision` previously
became the leading part of `git show <revision>:<path>`. List-form subprocess
execution avoided shell evaluation but did not prevent Git option parsing.
The checker now rejects option-shaped input, resolves one commit through
`rev-parse --verify --end-of-options`, validates its full object ID, and uses
only that ID in `git show` after `--end-of-options`. The path remains confined
to the repository. Tests prove option input is rejected before Git runs;
branch, tag, abbreviated ID, ancestry and historical blob controls still pass.
Tree objects and missing revisions are refused. This supports an option
injection finding, not an arbitrary shell-command execution claim.

**Fixed: unbounded buffering of pin replacement responses.** The prior SDK
relations path buffered the entire body and decrypted entries before the
core's count check. The narrow replacement uses the same SDK-configured HTTP
client and typed Ruma endpoint builder. It rejects advertised oversize bodies
and checks each streamed chunk before extending its buffer beyond 256 KiB.
Only a complete page of at most 20 raw events reaches sequential SDK decryption.
SDK trust settings, TLS/proxy policy, the existing 10-second deadline and final
session-generation reply check remain. No cryptography is implemented here.

Negative controls include an oversized response, chunked unknown-length data,
malformed JSON, excessive encrypted placeholders, incomplete pages and bounded
error responses. The chunked test verifies rejection before the server sends
its whole body. Ordinary pin edits and forged-sender rejection remain covered.

A fresh independent boundary review found that the first bounded-reader
candidate lost SDK token refresh. That confirmed regression was corrected:
only `401 M_UNKNOWN_TOKEN` permits one call to the SDK's authentication-specific
refresh and one retry with the new token. Both relation responses are bounded.
Tests check successful refresh, repeated refusal, oversized initial/retry errors
and unrelated errors. The refresh endpoint itself still uses SDK transport.

The GitHub alert-detail API returned HTTP 401 during inspection. The public PR
annotation and the actual source path supplied the finding context. Local
tests establish the repaired behavior; this record does not claim the remote
Sonar alert or review has closed.

## Run identity and completed checks

| Field | Value |
| --- | --- |
| Branch/base | `codex/room-navigation-media-health` / `e9ac7f561073583004efda26e2821cf65e1d4da7` plus final source manifest |
| Source | [Normalized hashes](room-navigation-monitor-fixes-sources.json) |
| Manifest SHA-256 | `6e090173a43afff3b5e0baa988048ca1dd4d9db9a913eb56a94668bd32810741` |
| Host/build | Windows x64, OS 10.0.26300.0; debug/test builds and release Wasm guest |
| Tools | Rust/Cargo 1.99.1; Node 24.15.0; npm 11.13.0; Python 3.14.4 |
| Dependencies | No version, feature or lockfile change |
| `python tools/dev.py check` | Exit 0: 842 Rust executions, 196 frontend tests, 57 Python tests |
| Earlier focused checks | Core library 225 passed before refresh compatibility correction; frontend wrapper 196 passed; five evidence-checker regressions passed with native temp access |
| Raw output | Ignored `.local/runs/pr47-monitor/check.log`, `core-tests.log`, `frontend.log`, `checker-tests.log` |

The full check includes format, guest/worker builds, workspace/development
tests and Clippy with `-D warnings`, Svelte/build/coverage, generated DTO drift,
TypeScript contracts and Python harnesses. Counts are executions, not unique
cases. The 2 ignored image benchmarks are not passes. The native override
and Visual Studio 18 Community x64 environment were used. Existing `ts-rs`
metadata warnings remain; runtime rejection and contract-drift checks pass.

An initial checker test run failed in the sandbox's temp directory; native
execution passed. Three refresh tests initially failed because their mock
server omitted `/versions`; the fixture was corrected, and the failing output
is retained in `core-tests-refresh-fixture-failure.log`. The first full run
failed Clippy's partial-read check in the chunked test server (exit 101,
`check-first.log`). The bounded request-header loop corrected that fixture
before the complete passing rerun. Failures are not counted as passes.

## Documentation checks

Run from the repository root:

```text
python tools/check_evidence.py --manifest docs/evidence/room-navigation-monitor-fixes-sources.json --markdown README.md PRIVACY.md SECURITY.md docs/README.md docs/development.md docs/adr/index.md docs/adr/0036-bound-room-navigation-and-inspection-in-the-core.md docs/room-tools.md docs/architecture/development-plan.md docs/planning/project-plan.md docs/evidence/room-navigation-media-health.md docs/evidence/room-navigation-review-fixes.md docs/evidence/room-navigation-monitor-fixes.md --json docs/evidence/verification.json docs/evidence/room-navigation-monitor-fixes-sources.json docs/evidence/room-navigation-review-fixes-sources.json
python tools/check_evidence.py --manifest docs/evidence/room-navigation-review-fixes-sources.json --revision e9ac7f561073583004efda26e2821cf65e1d4da7 --json docs/evidence/verification.json
```

Both commands exited 0. The first checked 54 current source hashes, 3 JSON
files, 13 Markdown files and unstaged tracked diff whitespace. The second
checked the prior record's 52 source hashes against their original commit.
Raw output is retained in ignored `.local/runs/pr47-monitor/docs-current.log`
and `docs-prior.log`. Before commit, `git diff --cached --check` also exited 0;
its output is retained in `staged-whitespace.log` in the same directory.

## Coverage and security limits

The attachment query test is now named as guard-unit coverage. A requested
mounted RoomTools interaction test was not added: the locked dependencies have
no DOM harness, and the existing component tests use server rendering. The
client effect/input-event path remains untested. No source-string surrogate
or new test framework was added. This is an explicit coverage gap, not proof
that native filter interactions passed.

No native UI/accessibility walkthrough, live homeserver acceptance,
Linux/macOS runtime, separate containment gate, release package or dependency
audit ran. Other SDK responses, including pin-state/original-event reads and
token refresh, still use SDK buffering. The new byte cap is limited to pin
replacement responses. Existing global SDK drain and `restart_required` gaps
remain. These fixes do not establish production security readiness.
