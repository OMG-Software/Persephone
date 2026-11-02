# Room navigation: PR 47 review fixes

2 November 2026. The final `python tools/dev.py check` run completed with exit 0.
This report supplements the [original run](room-navigation-media-health.md).

## Changes and validation

- Pin previews apply validated edits from the original sender before leaving
  the core. A failed, incomplete or undecryptable edit read hides the preview
  and retains the event reference. At most 20 replacements are checked per pin.
  Tests cover latest text, a forged sender and failure paths.
- The entire pin read has a 10-second deadline, including in-flight requests.
  The core checks the session lease before each content or edit request.
  Tests cover a slow request, retained references and closure before content
  fetching. The final generation fence still rejects replies after lock.
- Date navigation selects an eligible visible event in the same bounded
  context window. Ignored and relation-only events cannot become the focus.
  Tests cover an ignored lookup target followed by visible content and a
  window with no eligible event. No unbounded history fallback was added.
- Server room reads have separate bounded admission from local cache search.
  A contention test checks that each group stays bounded and independent.
- Attachment projection skips edit-identity lookups while retaining deletion
  processing. Filter changes clear old metadata and reject stale responses.
  Tests preserve moderator redactions and verify query changes away and back.
- Focused context hides the latest-room read-receipt footer. Pin controls have
  target-specific accessible names. Sender filters reject C0/C1 controls.
  New public DTO imports use generated contracts directly. Rendering and
  validation tests cover these changes; native accessibility remains untested.
- The filtered SQLite search test now excludes cached matching text by sender
  and both date boundaries. A production `run_sync` test checks health success,
  HTTP failure, recovery and generation reset through `Session::connection_health`.
- SonarQube issue `typescript:S7737` reported an object default for search
  filters. A named frozen default preserves the existing payload.
- The P6 plan links both feature verification histories. Documentation checks
  now have a committed checker, recorded commands, exit statuses and output logs.

These fixes address the 13 inline CodeRabbit findings, its attachment follow-up,
its two test-coverage notes and the one SonarQube issue retrieved for commit
`911bcd18d3c0e928312178c8053e2d8ac8c2a481`. Reviewer requests were checked against source and regression tests;
this report does not claim a new remote review or quality-gate result.

## Run identity

| Field | Value |
| --- | --- |
| Branch | `codex/room-navigation-media-health` |
| Base | `911bcd18d3c0e928312178c8053e2d8ac8c2a481` plus the final source manifest |
| Source | [Normalized hashes](room-navigation-review-fixes-sources.json) |
| Manifest SHA-256 | `3ca75db313fa4df862bb32af8b227832eb6a4ccf5c43e28848e4fd349b027caa` |
| Host | Windows x64, OS 10.0.26300.0; debug/test builds and release Wasm guest |
| Tools | Rust/Cargo 1.99.1, Node 24.15.0, npm 11.13.0, Python 3.14.4 |
| Dependencies | No dependency version, feature or lockfile changes |

The run used the Visual Studio 18 Community x64 developer environment and the
approved native execution override. Evidence and guide edits followed checks.
The original source manifest remains unchanged.

## Completed checks

| Command | Exit and observed result | Ignored output location |
| --- | --- | --- |
| `cargo test --locked -p persephone-core --lib` | 0; 217 tests passed before the final lint/copy adjustment | `.local/runs/pr47-remediation/core-tests.log` |
| `python tools/dev.py frontend` | 0; 193 tests, Svelte 0 errors/warnings, production build before final pin warning copy | `.local/runs/pr47-remediation/frontend.log` |
| `python tools/dev.py check` on final source | 0; 816 Rust test executions, 193 frontend tests, 2 registration and 50 tool/controller tests | `.local/runs/pr47-remediation/check.log` |
| Documentation/source checks | Exact commands and exit statuses are recorded below | `.local/runs/pr47-remediation/docscheck-*.log` |

The full check includes format, guest/worker builds, workspace and development
feature tests/Clippy with `-D warnings`, frontend coverage, generated DTO drift,
TypeScript contracts and Python harnesses. The 1061 passes are
executions, not unique cases; core tests repeat with development features.
The 2 ignored existing image benchmarks are not passes. Existing `ts-rs`
Serde-metadata warnings remain; runtime rejection and binding drift tests pass.

The final documentation check ran from the repository root:

```text
python tools/check_evidence.py --manifest docs/evidence/room-navigation-review-fixes-sources.json --markdown README.md PRIVACY.md SECURITY.md docs/README.md docs/adr/index.md docs/adr/0036-bound-room-navigation-and-inspection-in-the-core.md docs/room-tools.md docs/architecture/development-plan.md docs/planning/project-plan.md docs/evidence/room-navigation-media-health.md docs/evidence/room-navigation-review-fixes.md --json docs/evidence/verification.json docs/evidence/room-navigation-review-fixes-sources.json docs/evidence/room-navigation-media-health-sources.json
```

Exit 0: 52 source hashes, 3 JSON files, 11 Markdown files and diff hygiene passed.
Raw output: ignored `.local/runs/pr47-remediation/docscheck-current.log`.
The historical source command, exit 0 and output location are recorded in the
[original report's later check](room-navigation-media-health.md#later-reproducibility-check-2-november-2026).

The first full remediation check stopped at one Clippy `useless_asref` error
(exit 101, `check-first.log`). The conversion was removed and the complete
command rerun. The initial frontend remediation run caught two stale render
assertions; the corrected wrapper run passed. Failures are not counted as passes.

## Limits

No native UI/accessibility walkthrough, live Merovingian/Synapse acceptance,
Linux/macOS runtime check, separate containment gate, release package test or
dependency audit ran. Mock loopback SDK tests are synthetic. Existing SDK drain
and `restart_required` limits remain. A pin deadline is not a global SDK drain
barrier. The pin-state replacement still has no compare-and-swap guarantee.
