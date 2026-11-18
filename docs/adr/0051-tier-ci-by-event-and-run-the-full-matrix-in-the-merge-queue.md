# Tier CI by event and run the full matrix in the merge queue

* Status: proposed
* Deciders: project owner
* Date: 2026-11-18

Technical Story: on 18 November 2026 the owner asked for a faster pull
request signal without a weaker gate. After the measurements below, the
owner chose: Windows and ubuntu-latest as the pull request gate; the full
matrix in the merge queue and on pushes to `main`; `workflow_dispatch` only
for branches without a pull request; the eight required check names kept;
the `latest` pre-release still built on every merge; a 10 GB cache budget
with compiled dependencies cached for the core gate only. The account is on
the GitHub Free plan (20 concurrent jobs, 5 of them macOS).

## Context and Problem Statement

Measured from `gh run list` and the jobs API, 245 runs from 29 October to 18
November 2026, with full logs of runs 44296007062 (Check, `main`),
44319781592 (SonarQube) and 44296007133 (Release, `main`):

* A pull request's Check run took 22 minutes at the median and 67 at the
  90th percentile. Its jobs ran for 14 to 24 minutes, but waited 4 to 14
  minutes for a runner at the median and 43 to 64 minutes at the 90th
  percentile. Each pull request push started 9 jobs; each merge about 17,
  including about 133 runner-minutes of Release (45 on macOS).
* In each Check job, cargo compiled for 10 (Ubuntu) to 21 (Windows) minutes
  and ran tests for 1.1 to 3.6 minutes. The caches held downloaded sources
  only, and every one hit on its primary key.
* The repository's caches used 10.75 GB of the 10 GB budget, in 103
  entries. 35 of them were saved from pull request refs.
* No workflow ran twice for one commit: `push` covered `main` only.
* Release's `gate` job ran `python tools/dev.py check` again on each push to
  `main`, on the commit Check had just tested (11.4 minutes). Each of four
  Release jobs built the Tauri CLI from source (4 to 9 minutes), and `gate`
  built cargo-about (2.6 minutes).
* The SonarQube job ran cargo build scripts, npm and the instrumented tests
  in the job that held `SONAR_TOKEN`. Any step can read a job's secrets from
  the runner's memory, so dependency code could read the token.

How can a pull request get a fast signal while every platform still passes
before a merge?

## Decision Drivers

* Every one of the eight required checks must pass before code reaches
  `main`.
* Branch protection matches check names exactly; the owner keeps the eight
  names.
* Queue time, not run time, made the slow runs.
* The cache budget is 10 GB and is already full.
* No secret may share a job with dependency code (as `release.yml` already
  requires).

## Considered Options

For the tiers:

* One non-matrix job per platform, sharing steps through YAML anchors. A job
  skipped by `if` reports success under its own name.
* Matrix jobs as before, plus an aggregate job per required name.
* A dynamic matrix that drops legs on pull requests.
* A second workflow that runs only on pull requests and reports the
  queue-only names as passed.

For the compiler cache:

* Swatinem/rust-cache on the core gate, saved from `main` only.
* sccache with its GitHub Actions cache backend.
* A compiled-dependency cache on all eight jobs.

## Decision Outcome

Chosen options: per-platform jobs with YAML anchors, and rust-cache on the
core gate. The changes:

* `check.yml` runs `windows` and `ubuntu-latest` on every event. The six
  other jobs have `if: ${{ !cancelled() && github.event_name !=
  'pull_request' }}`: skipped (reported as success) on a pull request, run in
  the merge queue (`merge_group`), on `main` and by hand. No job that holds a
  required check is a matrix.
* `concurrency` cancels only an older run of the same pull request. Every
  other run has a group of its own (`github.run_id`), because a cancelled
  required check fails the merge queue. Groups use the file name as a
  literal prefix: in a reusable workflow `github.workflow` is the caller's.
* Only `main` saves caches; pull requests and the merge queue restore them.
  The core gate also restores compiled dependencies (rust-cache v2.9.3,
  `save-if` on `main`). rust-cache has no size limit, so each core job
  reports its hit and target size in the job summary and warns on `main`
  above 2 GiB.
* Release's `gate` skips the check on a push to `main`; tag and manual runs
  still run it. A non-tag run reuses the Tauri CLI and cargo-about that a
  `main` run built. A tag run builds them from source, because a cached
  binary, unlike a downloaded crate, is not checked against a lock file.
* SonarQube has two jobs. `coverage` has no secret and uploads the LCOV
  reports as an artifact. `sonarqube` holds `SONAR_TOKEN` and runs only the
  checkout, the artifact download and the scan action. It no longer restores
  `~/.sonar/cache`: the scanner runs the plugin JARs in it, and any job on
  `main` could have written that cache.
* `tests/tools/test_workflows.py` checks these rules. `check.yml`'s
  `ubuntu-latest` job installs PyYAML pinned by hash and sets
  `PERSEPHONE_WORKFLOW_TESTS=required`, so there the tests cannot be
  skipped.

| Required check | `check.yml` job | Pull request | Merge queue, `main`, manual |
| --- | --- | --- | --- |
| `windows` | `windows` | runs | runs |
| `ubuntu-latest` | `ubuntu-latest` | runs | runs |
| `macos-latest` | `macos` | skipped (success) | runs |
| `ubuntu-24.04-arm` | `ubuntu-arm` | skipped (success) | runs |
| `debian-13` | `debian` | skipped (success) | runs |
| `arch` | `arch` | skipped (success) | runs |
| `fedora-44` | `fedora` | skipped (success) | runs |
| `opensuse-leap-16.0` | `opensuse` | skipped (success) | runs |

Rejected:

* **Matrix plus aggregate jobs.** An aggregate job sees only the result of
  the whole matrix. A failure in one container would also fail the other
  containers' required checks. The legs would need new names so that they do
  not collide with the aggregates.
* **Dynamic matrix.** The dropped legs report nothing. Their required checks
  stay pending, and the pull request cannot enter the queue.
* **Placeholder workflow.** It adds a second source of the required names.
  In the merge queue, it must not run, or its passes would sit beside the
  real jobs under the same names.
* **sccache.** Its GitHub Actions backend stores many small entries in the
  same 10 GB budget and has no switch to save from `main` only.
* **Compiled dependencies on all eight jobs.** The owner kept the 10 GB
  budget. The queue-only jobs do not delay a pull request.
* **Sharding the tests.** Tests run for 1 to 4 minutes per job.
* **Fetching container packages on the host.** Each container installs its
  prerequisites in under a minute.
* **Folding SonarQube coverage into `ubuntu-latest`.** The coverage build is
  instrumented and leaves out the media worker's tests, so the required check
  would no longer show what it shows now.
* **`merge_group` on every workflow.** Only `check.yml` holds required
  checks. SonarQube in the queue would analyse each temporary
  `gh-readonly-queue` branch. Release and Pages do not run on pull requests.

### Positive Consequences

* A pull request push starts 3 jobs instead of 9: Windows, ubuntu-latest,
  and SonarQube (now two short jobs). Its runner-minutes fall from about 148
  to about 54, which leaves runners free and should shorten queue waits.
* Each merge saves about 50 Release runner-minutes on `main`, including
  about 7 minutes on the macOS path.
* `SONAR_TOKEN` no longer shares a job with dependency code.
* Pull requests no longer add caches, which frees budget for the compiled
  dependencies.

### Negative Consequences

* A failure that only macOS, arm64 or a container shows now appears in the
  merge queue, not on the pull request. The queue removes the entry, and the
  author must run `workflow_dispatch` or wait for the next queue run to see
  it again.
* Each merge waits for a full matrix run in the queue (about 25 minutes
  without queueing), and `main` runs it once more.
* Until the owner turns on the merge queue, pull requests merge after only
  the core gate. The other six checks then run first on `main`.
* The `latest` pre-release on `main` no longer waits for its own check. It
  depends on Check, which ran on the same commit in the merge queue.
* The scan downloads SonarQube's plugins on every run.
* rust-cache has no hard size limit; the 2 GiB warning is the control.
* The savings are estimates from the measurements above. Cache hit rates,
  cache sizes and queue times must be measured on GitHub after the change.

## Links

* Implements the request in the 18 November 2026 owner session.
* Keeps the secret rule of [ADR-0012](0012-sign-and-publish-releases-as-drafts.md)
  and `release.yml`'s header: no secret in a job that runs dependency code.
