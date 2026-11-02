# PR 47: canonical temporary roots in checker tests

2 November 2026. Base `3af935bca89aa48715fe87adf83d97986a54a741`. This follow-up changes only the Python
test fixture and adds a regression. Production checker guards stay unchanged.

## Verified CI failure

[Check run 41373398459](https://github.com/OMG-Software/Persephone/actions/runs/41373398459)
failed on Windows and macOS at source commit `36ed9bf`. The checker fixture
patched `ROOT` with an unresolved temporary directory. Its file-path guard
resolved each child before checking containment. Windows short-name paths
and the macOS `/var` alias produced different root spellings, so valid files
failed containment. Linux jobs in that run passed.

The fixture now resolves its root, as the production module already does.
A portable regression supplies an existing `alias-parent/..` root. It checks
canonical in-root source reads and rejection of an outside file. It needs no
symlink privilege. An in-memory negative control using the old root assignment
failed the new regression with exit 1. The committed source was not reverted.

## Completed checks

| Command/control | Result |
| --- | --- |
| `python -m unittest discover -s tests/tools -p test_check_evidence.py -v` | Exit 0: 13 scheduled, 11 passed, 2 skipped |
| `python -m unittest discover -s tests/tools -v` | Exit 0: 63 scheduled, 61 passed, 2 skipped |
| In-memory old-root regression control | Expected exit 1; alias failed canonical-root assertion |

Both skipped cases require filesystem symlinks, unavailable on this Windows
host (error 1314). They are not passes. The new alias control and real Git
historical blob tests ran. Output is in ignored
`.local/runs/pr47-monitor/ci-root-tests.log`, `ci-root-tools-suite.log`,
`ci-root-negative-control.log`, and `ci-temp-root-failure-excerpts.log`.
The host is Windows x64 10.0.26300.0 with Python 3.14.4; the native execution
override was used for temp Git repositories.

[54 normalized source hashes](room-navigation-ci-root-fix-sources.json) identify
the tested tree. Manifest SHA-256: `f0fd24375dbcd9fa3f7ffb56b9c43668db22fc2b022f8719555318dd404a71b5`. Only
`tests/tools/test_check_evidence.py` changed from base within that scope.
Dependencies, lockfiles and runtime sources are unchanged. Rust, frontend and
server tests were not repeated for this fixture-only change. Their earlier
[full run](room-navigation-review-refinements.md) remains tied to its sources.

## Evidence checks

From the repository root:

```text
python tools/check_evidence.py --manifest docs/evidence/room-navigation-ci-root-fix-sources.json --markdown docs/README.md docs/evidence/room-navigation-ci-root-fix.md --json docs/evidence/verification.json docs/evidence/room-navigation-ci-root-fix-sources.json
python tools/check_evidence.py --manifest docs/evidence/room-navigation-review-refinements-sources.json --revision 3af935bca89aa48715fe87adf83d97986a54a741 --json docs/evidence/verification.json
```

Both completed with exit 0: 54 current and 54 historical source hashes;
2 current Markdown files and 2 current JSON files. Raw output is in ignored
`.local/runs/pr47-monitor/ci-root-docs-current.log` and
`ci-root-docs-prior.log`. Staged whitespace is checked separately before commit.

The replacement CI run is not yet accepted as passing. No macOS/Windows
runner rerun result is inferred from this local test. Native UI, mounted
RoomTools interactions, live homeservers, containment, release and audit gates
remain open, as do the previously recorded SDK buffering/drain limitations
and Sonar owner-triage requirement.
