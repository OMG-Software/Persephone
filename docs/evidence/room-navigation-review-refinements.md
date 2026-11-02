# PR 47: review refinements

2 November 2026. This run verifies three later CodeRabbit findings against
base `36ed9bf31697cb527675d174341ecf82a4e1f732`. Earlier evidence remains historical.

## Verified fixes

- A replacement event with a malformed or different room ID now suppresses
  only that pin preview. The response retains its pin reference and healthy
  previews. It never substitutes obsolete original text. Original-event
  identity and ignore-store validation still fail closed. Two new core tests
  cover isolation and the original-room rejection control. Existing tests
  retain normal same-room edits and deterministic selection.
- The evidence checker ignores matching-backtick code spans when it checks
  inline Markdown links. Regression controls cover escapes, unmatched ticks,
  single-newline spans, blank paragraphs and adjacent real links.
- Historical source hashes use the lexical manifest path inside the selected
  Git commit. Current checkout symlinks cannot redirect that lookup. Portable
  path checks reject absolute paths, colon/drive syntax, backslashes, NUL,
  empty segments and dot segments. Current-file real-path guards and all Git
  revision/argument guards remain unchanged. A real Git blob test also
  forbids consultation of current source filesystem resolution.

## Run identity and verification

| Field | Value |
| --- | --- |
| Branch/base | `codex/room-navigation-media-health` / `36ed9bf31697cb527675d174341ecf82a4e1f732` |
| Source | [54 normalized hashes](room-navigation-review-refinements-sources.json) |
| Manifest SHA-256 | `ed4220e596e1b3e29df7bb9cab202cf19365642f89fa4130dd8a75ce12eaf6b6` |
| Host/build | Windows x64, OS 10.0.26300.0; debug/test builds and release Wasm guest |
| Tools | Rust/Cargo 1.99.1; Node 24.15.0; npm 11.13.0; Python 3.14.4 |
| Dependencies | No version, feature or lockfile change |
| `python tools/dev.py check` | Exit 0: 846 Rust executions, 196 frontend tests, 62 Python passes; 2 Python skips |
| Focused evidence checker | Exit 0: 12 scheduled, 10 passed, 2 skipped |

The full check includes Rust format, guest/worker builds, tests and Clippy
with warnings denied, Svelte/static build/coverage, generated DTO drift,
TypeScript contracts and Python harnesses. Counts are executions, not unique
cases. Two existing Rust image benchmarks remain ignored. Python scheduled
64 tests across two suites; the two actual filesystem symlink cases skipped
with Windows error 1314. They are not passes. The independent real Git blob
regression ran and passed. Svelte reported zero errors and warnings.

The native execution override and Visual Studio 18 Community x64 environment
were used. An initial focused checker attempt could not use the sandbox temp
Git directory; the native run completed successfully. Raw output is ignored:
`.local/runs/pr47-monitor/round2-check.log` and
`.local/runs/pr47-monitor/tools-round2-tests.log`.

## Documentation verification

From the repository root:

```text
python tools/check_evidence.py --manifest docs/evidence/room-navigation-review-refinements-sources.json --markdown docs/development.md docs/room-tools.md docs/README.md docs/evidence/room-navigation-review-refinements.md --json docs/evidence/verification.json docs/evidence/room-navigation-review-refinements-sources.json
python tools/check_evidence.py --manifest docs/evidence/room-navigation-sonar-parser-sources.json --revision 36ed9bf31697cb527675d174341ecf82a4e1f732 --json docs/evidence/verification.json
```

Both completed with exit 0. The first checks 54 current source hashes,
4 Markdown files, 2 JSON files and unstaged tracked whitespace. The second
checks 54 historical source hashes against their recorded commit. Raw output
is in `.local/runs/pr47-monitor/round2-docs-current.log` and
`round2-docs-prior.log` in the same directory. Staged whitespace is checked
separately with `git diff --cached --check` before commit.

## Limits

No native UI/accessibility walkthrough, mounted RoomTools interaction,
live homeserver acceptance, Linux/macOS runtime, separate containment gate,
release package or dependency audit ran. The missing locked DOM harness
remains an explicit coverage gap. Existing SDK response buffering, complete
SDK task drain and `restart_required` limitations remain. These checks do
not establish production readiness. The previously documented Sonar Git
revision false positive still requires owner triage; these changes do not
claim that the remote gate or review has closed.
