# PR 47: SonarQube follow-up

2 November 2026. This records alerts reported for commit `6249689`. It does
not replace the [full monitored verification](room-navigation-monitor-fixes.md).

## Local URL parser

SonarQube `typescript:S5332` flagged an HTTP prefix in `roomTools.ts`. The
expression constructs and discards a `URL` to validate a bracketed IPv6 host.
It does not open a connection or disable TLS checks. The unused parser prefix
now uses HTTPS to make that intent consistent with the product policy.

`python tools/dev.py frontend` completed with exit 0: Svelte reported zero
errors/warnings, the build passed and all 196 tests passed. Existing sender
and IPv6 cases check the unchanged validation behavior. Raw output is in
ignored `.local/runs/pr47-monitor/sonar-parser-frontend.log`.

## Git revision alert: no source change

SonarQube `pythonsecurity:S8705`, alert 24 / issue `AaGZ3CetsSb8N7GDkmh-`,
flags the existing `resolve_revision` call. Independent source review and
execution found no surviving argument-injection path:

- Input enters a local `argparse` CLI, not an HTTP handler.
- Empty, option-shaped and NUL-containing revisions are rejected.
- Subprocess execution uses an argument list without a shell. The revision
  remains one argument after `--end-of-options`, with `^{commit}` appended.
- Git must resolve one commit. The result must be a complete hexadecimal
  object ID before it reaches `git show`, which also uses `--end-of-options`
  and disables external diff and text conversion.

`python -m unittest discover -s tests/tools -p test_check_evidence.py -v`
completed with exit 0: all five regressions passed with native temporary
directory access. Tests retain branches, tags, ancestry and abbreviated IDs;
they reject options before Git runs and refuse tree and missing revisions.
Raw output is in `.local/runs/pr47-monitor/sonar-revision-recheck.log`.

Additional direct calls rejected six option, whitespace, newline, semicolon,
tree and blob-path inputs. Four valid revision controls resolved correctly;
no output file was created. Exit 0 and output are recorded in
`.local/runs/pr47-monitor/sonar-revision-probes.log`. A separate read-only
review reached the same conclusion. This is evidence for local classification
as a false positive; it does not establish a changed remote issue status.

No code was changed merely to bypass the scanner. SonarQube still needs
project-owner triage of [this issue](https://sonarcloud.io/project/issues?id=OMG-Software_Persephone&issues=AaGZ3CetsSb8N7GDkmh-&open=AaGZ3CetsSb8N7GDkmh-&pullRequest=47).
The available connection cannot update SonarQube issue classification.

## Documentation check

From the repository root:

```text
python tools/check_evidence.py --manifest docs/evidence/room-navigation-sonar-parser-sources.json --markdown docs/evidence/room-navigation-sonar-follow-up.md docs/README.md --json docs/evidence/verification.json docs/evidence/room-navigation-sonar-parser-sources.json
```

Exit 0: 54 source hashes, 2 JSON files, 2 Markdown files and unstaged tracked
diff whitespace passed. Raw output is in ignored
`.local/runs/pr47-monitor/sonar-follow-up-docs.log`. Staged whitespace is
checked separately before commit.

## Source and limits

The base is `624968920683afeeb91d780164055b9d40f1812a`. The only runtime
change is the URL-parser prefix. [Normalized source hashes](room-navigation-sonar-parser-sources.json)
record all 54 files from the preceding verification scope. Dependencies,
features, lockfiles, Rust code and security guards are unchanged. The host
remains Windows x64, OS 10.0.26300.0, with Node 24.15.0 and Python 3.14.4.

Rust tests were not repeated for this parser-only change. Native UI, mounted
RoomTools interactions, live homeserver acceptance, other-platform runtime
checks, containment, release-package checks and dependency audit remain open.
The existing SDK buffering/drain limits remain. Remote checks and quality-gate
results apply only to their recorded revision.
