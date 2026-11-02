# PR 47 CodeRabbit inline fixes

Updated 2 November 2026. This report covers two inline findings from the
CodeRabbit review: `4440762692` and `4440762715`. It records a focused core
fix and Markdown-checker fix, with regression tests. It does not claim a full
project check or remote CodeRabbit approval.

## Findings and fix

Invalid pinned edit relations must hide the affected preview without causing
an unnecessary fetch of the original event's edit parent. The earlier
`project_pin` fallback used the full projection path. That path resolved the
edit parent. A focused test caught the request to `/$parent` and failed before
the fix.

The rejected-relation branch now calls
`project_chunk_inner(..., false)`. This keeps original event identity, room,
and ignore-state checks. It skips edit-parent resolution because the preview
will be hidden. It does not weaken the checks for original-event identity,
ignore storage, or valid relations.

The new whole-panel regression pins an original with an invalid cross-room
replacement and a second healthy event. It checks that no request fetches the
invalid edit's parent, that the invalid preview stays hidden, and that the
healthy pin still appears. The regression passed in both default and
development-feature runs.

## Run identity

| Field | Value |
| --- | --- |
| Branch | `codex/room-navigation-media-health` |
| Baseline | `5a2545772faacbb4c50aae5f1b397aa73e6237d1` plus the final source manifest |
| Source | [Normalized source hashes](room-navigation-coderabbit-inline-fixes-sources.json) |
| Manifest SHA-256 | a99d5b2b53f73136b075c0719ad57e10a712ed087cc8c3899f4c672725532ec4 |
| Host | Windows x64, OS `10.0.26300.0` |
| Tools | Rust/Cargo 1.99.1; Python 3.14.4 |
| Dependencies | No dependency or lockfile changes |
| Raw output | Ignored `.local/runs/pr47-monitor/` |

## Completed checks

| Check | Observed result | Raw output |
| --- | --- | --- |
| Focused pin-parent regression before the fix | Exit 101. The test observed a GET for the `$parent` event, as forbidden by the requirement. | `.local/runs/pr47-monitor/pin-parent-before.log` |
| Core tests, default features | 234 passed, 0 failed; exit 0. | `.local/runs/pr47-monitor/pin-parent-core-tests.log` |
| Core tests, development feature | 238 passed, 0 failed; exit 0. | `.local/runs/pr47-monitor/pin-parent-core-dev-tests.log` |
| Clippy, all targets and development feature, with `-D warnings` | Exit 0. | `.local/runs/pr47-monitor/pin-parent-clippy.log` |
| Rust format | `cargo fmt --all -- --check`; exit 0. | `.local/runs/pr47-monitor/pin-parent-fmt.log` |

Both core runs include the earlier regression for invalid pin relations and a
malformed ignore store.

## Python checker fix and checks

The checker treated a backtick inside an HTML comment as a code opener. It
could pair that backtick with a later code span and hide a real missing link.
The parser now handles comment and code openers in order. It removes actual
comments, keeps comment markers inside code literal, and respects escaped
openers. Two new tests cover comment backticks, adjacent comments, paragraph
boundaries, escaped markers and code spans that cross comment markers.

The focused suite completed with exit 0: 15 scheduled, 13 passed, two symlink
skips. The full tools suite completed with exit 0: 65 scheduled, 63 passed,
two symlink skips. Windows denied symlink creation (error 1314); these cases
are not passes. The earlier baseline negative control failed with exit 1
against the new tests, including missing-link assertions. Git revision guards
and dependency locks are unchanged.

Logs: `.local/runs/pr47-monitor/html-comment-escape-tests.log`,
`html-comment-tools-suite.log` and `html-comment-negative-control.log`.

## Reproduction commands

Rust commands used the established Visual Studio 18 x64 environment and
`.local/cache/cargo`. From the repository root:

```text
cargo test --locked -p persephone-core --lib
cargo test --locked -p persephone-core --features dev-disposable-servers --lib
cargo clippy --locked -p persephone-core --all-targets --features dev-disposable-servers -- -D warnings
python -m unittest discover -s tests/tools -p test_check_evidence.py -v
python -m unittest discover -s tests/tools -v
```

## Limits

No full development wrapper, frontend run, mounted UI check, native interaction
or accessibility check, live homeserver run, other-platform runtime check,
containment check, release test or dependency audit was run for these fixes.
The previous 196-test frontend result is historical; frontend files did not
change in this batch and that result was not rerun.

The SDK drain and restart limits remain open. Core response buffering still
has broader limits beyond this pin-relation path. These results do not close a
milestone or production security gate.

## Evidence checks

```text
python tools/check_evidence.py --manifest docs/evidence/room-navigation-coderabbit-inline-fixes-sources.json --markdown docs/README.md docs/evidence/room-navigation-coderabbit-inline-fixes.md --json docs/evidence/verification.json docs/evidence/room-navigation-coderabbit-inline-fixes-sources.json
python tools/check_evidence.py --manifest docs/evidence/room-navigation-coderabbit-merge-fixes-sources.json --revision 5a2545772faacbb4c50aae5f1b397aa73e6237d1
```

Both completed with exit 0: 54 current and 54 historical hashes, two current
Markdown files and two JSON files. Staged whitespace was checked before commit.
