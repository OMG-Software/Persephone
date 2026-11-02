# PR 47 CodeRabbit merge fixes

Updated 2 November 2026. This report records documentation and regression-test
work for the PR 47 review. It does not claim a new remote review score or a
production security result.

The change adds or refines function documentation across 18 source and test
files. It changes no production logic. A direct core regression test checks an
invalid pin-edit room reference when the local ignore store is also malformed.

## Review coverage

The observed CodeRabbit report showed 53.25% function documentation coverage,
below its 80% threshold. It listed 77 touched functions in 21 files, with 10
functions marked unsupported. A new remote coverage result has not been
verified: **PENDING**.

## Regression-test correction

The test fetches the original pin event and its cross-room replacement through
the SDK before it corrupts the real ignore store. It then confirms that ignore
state cannot be read and calls the direct pin projection path. The projection
must return `Unavailable`; it must not treat the invalid relation as a reason
to return an empty preview and hide the store failure.

The first whole-panel fixture failed twice with an `Ok(PinnedMessages { ... })`
result containing no items and `truncated: true`. SDK construction of the room
event's push context read the corrupt ignore store before execution reached the
projection branch. The corrected fixture prepares SDK objects first. It tests
the intended guard directly. It does not mount the full panel.

## Run identity

| Field | Value |
| --- | --- |
| Branch | `codex/room-navigation-media-health` |
| Baseline | `9f2e01176624e0283e2a257a7c4b5f249bc45a69` plus the final source manifest |
| Source | [Normalized source hashes](room-navigation-coderabbit-merge-fixes-sources.json) |
| Manifest SHA-256 | `0b4f19cd50bc351a3ca01b5c14b9e43f72072fb3c80d60736d7b050f81968f89` (54 normalized source hashes) |
| Host and tool versions | Windows x64 10.0.26300.0; Rust/Cargo 1.99.1; Python 3.14.4; Node 24.15.0 |
| Raw output | Ignored `.local/runs/pr47-monitor/` |

## Completed checks

| Check | Observed result | Raw output |
| --- | --- | --- |
| `python tools/dev.py frontend` | Exit 0; 196 Node tests passed; Svelte reported 0 errors and 0 warnings. | `.local/runs/pr47-monitor/docstring-frontend.log` |
| Evidence-checker unit tests | 13 scheduled; 11 passed; 2 symlink tests skipped because Windows denied symlink creation. | `.local/runs/pr47-monitor/docstring-checker.log` |
| Rust doctests | Four targets completed with 0 cases and exit 0. | `.local/runs/pr47-monitor/docstring-rustdoc.log` |
| Rust documentation build | Exit 0. Three pre-existing private-item link warnings remain at `account.rs:567`, `oauth_login.rs:78` and `outbox.rs:40`. | `.local/runs/pr47-monitor/docstring-rustdoc-build.log` |
| Room-tools default-feature tests | 28 passed; exit 0. | `.local/runs/pr47-monitor/ignore-guard-fixed-tests.log` |
| Room-tools development-feature tests | 28 passed; exit 0. | `.local/runs/pr47-monitor/ignore-guard-fixed-dev-tests.log` |
| Final Rust format check | `cargo fmt --all -- --check`; exit 0. | `.local/runs/pr47-monitor/ignore-guard-final-fmt.log` |
| Source audit | 18 changed source/test files contained no executable source changes, except the new regression test; Python AST was unchanged after removing docstrings. | `.local/runs/pr47-monitor/coderabbit-final-source-audit.log` |

Native Rust commands used the established Visual Studio 18 x64 environment,
locked dependencies and `.local/cache/cargo`. Commands, from the repository root:

```text
python -m unittest discover -s tests/tools -p test_check_evidence.py -v
cargo test --locked -p persephone-core --lib room_tools::tests
cargo test --locked -p persephone-core --features dev-disposable-servers --lib room_tools::tests
cargo test --locked --doc -p persephone-core -p persephone-core-protocol -p persephone-contracts -p persephone-desktop
cargo doc --locked --no-deps -p persephone-core -p persephone-core-protocol -p persephone-contracts -p persephone-desktop
```

## Checks not repeated or pending

* The full `python tools/dev.py check` was not repeated because production
  statements did not change. No full-check result is claimed for this batch.
* New remote CodeRabbit documentation-coverage result: **PENDING**.

## Negative control

The test's negative control temporarily removed the three-line projection guard.
The regression then failed with exit 101 and returned `Ok([])` where it expected
`Unavailable`. The harness restored the source and reran the regression; one
test passed and none failed. This shows that the test detects removal of the
guard. It is not a product failure in the restored source.

Raw output: `.local/runs/pr47-monitor/ignore-guard-negative-control.log` and
`.local/runs/pr47-monitor/ignore-guard-restored.log`.

## Limits

These checks do not cover a mounted room-tools panel, native accessibility or
interaction, live homeserver behavior, Linux or macOS runtime behavior, or the
full SDK drain and restart requirements. Other SDK response buffering, native
containment, release packaging and dependency-audit acceptance remain open. They do not close a milestone or
production security gate. Do not infer those results from unit tests, a
documentation build, or a passing frontend check.

## Evidence checks

```text
python tools/check_evidence.py --manifest docs/evidence/room-navigation-coderabbit-merge-fixes-sources.json --markdown docs/README.md docs/evidence/room-navigation-coderabbit-merge-fixes.md --json docs/evidence/verification.json docs/evidence/room-navigation-coderabbit-merge-fixes-sources.json
```

This completed with exit 0: 54 source hashes, two Markdown files and two JSON
files. Staged whitespace was also checked before commit.
