"""The rules the GitHub Actions workflows must keep (ADR-0051): required
checks, triggers, concurrency, cache saves and secret isolation.

Each rule is a function that returns a list of problems for a parsed
workflow. The tests run each rule on the repository's workflows, which must
have no problem, and on a small workflow that breaks the rule, which must have
one. PyYAML parses the workflows. Where it is missing these tests are skipped,
except where PERSEPHONE_WORKFLOW_TESTS=required (check.yml's ubuntu-latest
job, which installs it): there the import fails."""

import json
import os
from pathlib import Path
import re
import unittest

try:
    import yaml
except ImportError:
    if os.environ.get("PERSEPHONE_WORKFLOW_TESTS") == "required":
        raise
    yaml = None


ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github/workflows"

# Each required check of the "Protect main" ruleset, and the check.yml job
# that reports it. Branch protection matches the names exactly.
REQUIRED_CHECKS = {
    "windows": "windows",
    "ubuntu-latest": "ubuntu-latest",
    "macos-latest": "macos",
    "ubuntu-24.04-arm": "ubuntu-arm",
    "debian-13": "debian",
    "arch": "arch",
    "fedora-44": "fedora",
    "opensuse-leap-16.0": "opensuse",
}
# The jobs that run on every pull request.
CORE_GATE = {"windows", "ubuntu-latest"}
REQUIRED_WORKFLOW = "check.yml"

QUEUE_ONLY = "${{ !cancelled() && github.event_name != 'pull_request' }}"
PR_CANCEL = "${{ github.event_name == 'pull_request' }}"
# The group suffix of a workflow that cancels: one group per pull request,
# and one per run for every other event.
PR_GROUP = "${{ github.event_name == 'pull_request' && format('pr-{0}', github.event.pull_request.number) || github.run_id }}"
ON_MAIN = "github.ref == 'refs/heads/main'"
# The whole `if` of a cache save: main, and only when the restore missed.
SAVE_GUARD = re.compile(r"github\.ref == 'refs/heads/main' && steps\.[A-Za-z0-9_-]+\.outputs\.cache-hit != 'true'")
NOT_TAG = "${{ !(github.event_name == 'push' && github.ref_type == 'tag') }}"
# The whole `if` of a SONAR_TOKEN job: every event but a fork pull request.
FORK_GUARD = "github.event_name != 'pull_request' || github.event.pull_request.head.repo.full_name == github.repository"
PINNED = re.compile(r"[^@\s]+@[0-9a-f]{40}")
# Commands that run dependency code (build scripts, package scripts) or fetch
# packages. None may run in a job that holds a secret.
BUILD_COMMAND = re.compile(r"\b(cargo|npm|npx|pip|rustup)\b|tools/dev\.py")
CACHE_ACTIONS = ("actions/cache@", "actions/cache/restore@", "Swatinem/rust-cache@")


def load(path):
    workflow = yaml.safe_load(path.read_text(encoding="utf-8"))
    # YAML 1.1 reads the key `on` as true.
    if True in workflow:
        workflow["on"] = workflow.pop(True)
    return workflow


def triggers(workflow):
    on = workflow.get("on") or {}
    if isinstance(on, str):
        return {on: None}
    if isinstance(on, list):
        return dict.fromkeys(on)
    return on


def jobs(workflow):
    return (workflow.get("jobs") or {}).items()


def steps(job):
    return job.get("steps") or []


def check_name(job_id, job):
    return job.get("name", job_id)


def required_check_problems(workflow):
    """Every required check is reported by a plain job of that exact name.
    A matrix job is not allowed: skipped, it reports under its unexpanded
    name. The core gate has no `if`; every other required job is skipped
    only on pull requests."""
    problems = []
    found = dict(jobs(workflow))
    for name, job_id in REQUIRED_CHECKS.items():
        job = found.get(job_id)
        if job is None:
            problems.append(f"{name}: no job {job_id}")
            continue
        if check_name(job_id, job) != name:
            problems.append(f"{name}: job {job_id} reports {check_name(job_id, job)!r}")
        if "strategy" in job:
            problems.append(f"{name}: job {job_id} is a matrix")
        expected = None if job_id in CORE_GATE else QUEUE_ONLY
        if job.get("if") != expected:
            problems.append(f"{name}: job {job_id} has if {job.get('if')!r}, not {expected!r}")
    return problems


def duplicate_check_problems(name, workflow):
    """No job other than the mapped one reports a required check's name."""
    problems = []
    for job_id, job in jobs(workflow):
        reported = check_name(job_id, job)
        if reported in REQUIRED_CHECKS and (name != REQUIRED_WORKFLOW or REQUIRED_CHECKS[reported] != job_id):
            problems.append(f"{name}: job {job_id} also reports required check {reported!r}")
    return problems


def trigger_problems(name, workflow):
    """Every workflow can be run by hand. The workflow that holds required
    checks runs on pull requests, in the merge queue and on main. A push
    trigger covers main only, so that a branch with a pull request is not
    run twice for one commit, and never the merge queue's branches."""
    problems = []
    on = triggers(workflow)
    if "workflow_dispatch" not in on:
        problems.append(f"{name}: no workflow_dispatch")
    if name == REQUIRED_WORKFLOW:
        for event in ("pull_request", "merge_group", "push"):
            if event not in on:
                problems.append(f"{name}: holds required checks but has no {event}")
    push = on.get("push") or {}
    # The whole filter, not a pattern match: GitHub's filter syntax (`*`,
    # `**`, `+`, `?`, `[]`, `!`) differs from fnmatch, and a later negative
    # pattern can exclude main again.
    if name == REQUIRED_WORKFLOW and push.get("branches") != ["main"]:
        problems.append(f"{name}: holds required checks but push branches are not exactly [main]")
    if "branches" in push and "branches-ignore" in push:
        problems.append(f"{name}: push has both branches and branches-ignore")
    branches = push.get("branches") or []
    if "push" in on and not branches and not push.get("tags"):
        problems.append(f"{name}: push runs on every branch")
    for branch in branches:
        if branch != "main" and not branch.startswith("!"):
            problems.append(f"{name}: push runs on branch {branch!r}, which its pull request also runs")
    if any("*" in branch for branch in branches) and "!gh-readonly-queue/**" not in branches:
        problems.append(f"{name}: push wildcard does not exclude gh-readonly-queue/**")
    return problems


def concurrency_problems(name, workflow):
    """The group has a literal prefix (the file's stem): github.workflow is
    the caller's name in a reusable workflow. Only pull request runs are
    cancelled, and then every other run has a group of its own."""
    problems = []
    stem = Path(name).stem
    concurrency = workflow.get("concurrency")
    if not isinstance(concurrency, dict):
        return [f"{name}: no concurrency group"]
    group = str(concurrency.get("group", ""))
    if group != stem and not group.startswith(f"{stem}-"):
        problems.append(f"{name}: group {group!r} does not start with {stem!r}")
    if "github.workflow" in group:
        problems.append(f"{name}: group uses github.workflow")
    cancel = concurrency.get("cancel-in-progress", False)
    if cancel not in (False, PR_CANCEL):
        problems.append(f"{name}: cancel-in-progress {cancel!r} may cancel a run that is not a pull request's")
    if cancel == PR_CANCEL and group != f"{stem}-{PR_GROUP}":
        problems.append(f"{name}: a cancelling group must be {stem}-{PR_GROUP}")
    return problems


def cache_problems(name, workflow):
    """Only main saves caches; pull requests and the merge queue restore.
    The compiled-dependency cache is for the core gate only. A tag run never
    restores a cached tool binary."""
    problems = []
    on = triggers(workflow)
    untrusted = "pull_request" in on or "merge_group" in on
    for job_id, job in jobs(workflow):
        compile_cache = (job.get("env") or {}).get("COMPILE_CACHE") == "true"
        if compile_cache and (name != REQUIRED_WORKFLOW or job_id not in CORE_GATE):
            problems.append(f"{name}: job {job_id} caches compiled dependencies outside the core gate")
        for step in steps(job):
            uses = step.get("uses", "")
            label = f"{name}: job {job_id} step {step.get('name', uses)!r}"
            if uses.startswith("actions/cache@") and untrusted:
                problems.append(f"{label} saves a cache from pull requests; use restore and save")
            if uses.startswith("actions/cache/save@") and not SAVE_GUARD.fullmatch(str(step.get("if", ""))):
                problems.append(f"{label} saves a cache outside main")
            if uses.startswith("Swatinem/rust-cache@"):
                if (step.get("with") or {}).get("save-if") != "${{ " + ON_MAIN + " }}":
                    problems.append(f"{label} saves compiled dependencies outside main")
                if step.get("if") != "env.COMPILE_CACHE == 'true'":
                    problems.append(f"{label} is not limited to the core gate")
            path = str((step.get("with") or {}).get("path", ""))
            if uses.startswith("actions/cache/restore@") and "runner.temp" in path and step.get("if") != NOT_TAG:
                problems.append(f"{label} restores a cached tool binary in a tag run")
    return problems


def secret_problems(name, workflow):
    """A job that holds a secret runs no dependency code and restores no
    cache, because any step can read the job's secrets from the runner's
    memory. A SONAR_TOKEN job skips fork pull requests. Every action is
    pinned to a full commit SHA."""
    problems = []
    # A job inherits the workflow's env, and any secret in it.
    inherited = json.dumps(workflow.get("env") or {})
    for job_id, job in jobs(workflow):
        text = json.dumps(job) + inherited
        for step in steps(job):
            uses = step.get("uses", "")
            if uses and not uses.startswith("./") and not PINNED.fullmatch(uses):
                problems.append(f"{name}: job {job_id} uses unpinned {uses!r}")
        secrets = set(re.findall(r"secrets\.([A-Za-z0-9_]+)", text)) - {"GITHUB_TOKEN"}
        if not secrets:
            continue
        for step in steps(job):
            label = f"{name}: job {job_id} holds {sorted(secrets)} and step {step.get('name', step.get('uses'))!r}"
            if BUILD_COMMAND.search(str(step.get("run", ""))):
                problems.append(f"{label} runs a build or package command")
            if step.get("uses", "").startswith(CACHE_ACTIONS):
                problems.append(f"{label} restores a cache")
        if "SONAR_TOKEN" in secrets and job.get("if") != FORK_GUARD:
            problems.append(f"{name}: job {job_id} holds SONAR_TOKEN without the fork guard")
    return problems


def all_problems(name, workflow):
    problems = trigger_problems(name, workflow)
    problems += concurrency_problems(name, workflow)
    problems += cache_problems(name, workflow)
    problems += secret_problems(name, workflow)
    problems += duplicate_check_problems(name, workflow)
    if name == REQUIRED_WORKFLOW:
        problems += required_check_problems(workflow)
    return problems


def parse(text):
    workflow = yaml.safe_load(text)
    if True in workflow:
        workflow["on"] = workflow.pop(True)
    return workflow


@unittest.skipIf(yaml is None, "PyYAML is not installed (see tools/ci-requirements.txt)")
class RepositoryWorkflows(unittest.TestCase):
    def test_every_workflow_keeps_the_rules(self):
        """GIVEN each workflow in .github/workflows
        WHEN every rule checks it
        THEN none reports a problem."""
        paths = sorted(WORKFLOWS.glob("*.yml"))
        self.assertIn(REQUIRED_WORKFLOW, [path.name for path in paths])
        for path in paths:
            with self.subTest(workflow=path.name):
                self.assertEqual(all_problems(path.name, load(path)), [])

    def test_the_workflow_tests_run_in_the_core_gate(self):
        """GIVEN check.yml
        WHEN a core-gate job sets PERSEPHONE_WORKFLOW_TESTS=required
        THEN that job installs the hash-pinned PyYAML before the check."""
        workflow = load(WORKFLOWS / REQUIRED_WORKFLOW)
        job = workflow["jobs"]["ubuntu-latest"]
        self.assertEqual(job["env"].get("PERSEPHONE_WORKFLOW_TESTS"), "required")
        runs = [str(step.get("run", "")) for step in job["steps"]]
        install = next(i for i, run in enumerate(runs) if "tools/ci-requirements.txt" in run)
        check = next(i for i, run in enumerate(runs) if run.endswith("tools/dev.py check"))
        self.assertLess(install, check)
        self.assertIn("--require-hashes", runs[install])
        requirements = (ROOT / "tools/ci-requirements.txt").read_text(encoding="utf-8")
        self.assertRegex(requirements, r"--hash=sha256:[0-9a-f]{64}")


MINIMAL = """
on:
  push: {branches: [main]}
  pull_request:
  merge_group:
  workflow_dispatch:
concurrency:
  group: check-${{ github.event_name == 'pull_request' && format('pr-{0}', github.event.pull_request.number) || github.run_id }}
  cancel-in-progress: ${{ github.event_name == 'pull_request' }}
jobs: {}
"""


@unittest.skipIf(yaml is None, "PyYAML is not installed (see tools/ci-requirements.txt)")
class RulesReject(unittest.TestCase):
    def workflow(self, **changes):
        workflow = parse(MINIMAL)
        workflow.update(changes)
        return workflow

    def test_a_required_check_reported_by_a_matrix(self):
        """GIVEN check.yml where macos-latest is one leg of a matrix job
        WHEN the required-check rule runs
        THEN it reports the missing plain job."""
        workflow = load(WORKFLOWS / REQUIRED_WORKFLOW)
        workflow["jobs"].pop("macos")
        workflow["jobs"]["hosted"] = {"name": "${{ matrix.os }}", "strategy": {"matrix": {"os": ["macos-latest"]}}}
        self.assertIn("macos-latest: no job macos", required_check_problems(workflow))

    def test_a_queue_only_check_that_also_runs_on_pull_requests_or_a_skipped_core_job(self):
        """GIVEN check.yml with the arch job's `if` removed and windows skipped
        WHEN the required-check rule runs
        THEN it reports both."""
        workflow = load(WORKFLOWS / REQUIRED_WORKFLOW)
        workflow["jobs"]["arch"].pop("if")
        workflow["jobs"]["windows"]["if"] = QUEUE_ONLY
        problems = required_check_problems(workflow)
        self.assertTrue(any(p.startswith("arch: job arch has if None") for p in problems), problems)
        self.assertTrue(any(p.startswith("windows: job windows has if") for p in problems), problems)

    def test_a_second_job_with_a_required_name(self):
        """GIVEN a workflow with another job named debian-13
        WHEN the duplicate rule runs
        THEN it reports that job."""
        workflow = self.workflow(jobs={"other": {"name": "debian-13"}})
        self.assertEqual(
            duplicate_check_problems("other.yml", workflow),
            ["other.yml: job other also reports required check 'debian-13'"],
        )

    def test_missing_triggers_and_a_branch_run_twice(self):
        """GIVEN check.yml without merge_group or workflow_dispatch, pushing on every branch
        WHEN the trigger rule runs
        THEN it reports each fault."""
        workflow = self.workflow(on={"push": {"branches": ["**"]}, "pull_request": None})
        problems = trigger_problems("check.yml", workflow)
        self.assertIn("check.yml: no workflow_dispatch", problems)
        self.assertIn("check.yml: holds required checks but has no merge_group", problems)
        self.assertIn("check.yml: push runs on branch '**', which its pull request also runs", problems)
        self.assertIn("check.yml: push wildcard does not exclude gh-readonly-queue/**", problems)

    def test_a_shared_or_cancelling_group(self):
        """GIVEN a group named by github.workflow that cancels every run
        WHEN the concurrency rule runs
        THEN it reports the prefix and the cancellation."""
        workflow = self.workflow(concurrency={"group": "${{ github.workflow }}-${{ github.ref }}", "cancel-in-progress": True})
        problems = concurrency_problems("check.yml", workflow)
        self.assertEqual(len(problems), 3, problems)

    def test_a_cache_saved_from_a_pull_request(self):
        """GIVEN a pull request workflow with a combined cache, an unguarded save,
        a rust-cache that saves everywhere and a tool cache restored on tags
        WHEN the cache rule runs
        THEN it reports each."""
        job = {
            "env": {"COMPILE_CACHE": "true"},
            "steps": [
                {"name": "a", "uses": "actions/cache@" + "0" * 40},
                {"name": "b", "uses": "actions/cache/save@" + "0" * 40},
                {"name": "c", "uses": "Swatinem/rust-cache@" + "0" * 40, "with": {"save-if": "true"}},
                {"name": "d", "uses": "actions/cache/restore@" + "0" * 40, "with": {"path": "${{ runner.temp }}/tools"}},
            ],
        }
        problems = cache_problems("check.yml", self.workflow(jobs={"macos": job}))
        self.assertEqual(len(problems), 6, problems)

    def test_a_secret_next_to_dependency_code(self):
        """GIVEN a job that holds SONAR_TOKEN, builds, restores a cache, has no
        fork guard and uses an action by tag
        WHEN the secret rule runs
        THEN it reports each."""
        job = {
            "steps": [
                {"name": "build", "run": "python tools/dev.py coverage"},
                {"name": "cache", "uses": "actions/cache/restore@" + "0" * 40},
                {"name": "scan", "uses": "SonarSource/sonarqube-scan-action@v8", "env": {"SONAR_TOKEN": "${{ secrets.SONAR_TOKEN }}"}},
            ],
        }
        problems = secret_problems("sonar.yml", self.workflow(jobs={"sonarqube": job}))
        self.assertEqual(len(problems), 4, problems)

    def test_required_checks_that_no_longer_run_on_main(self):
        """GIVEN check.yml whose push runs on tags only, or lists main and then excludes it
        WHEN the trigger rule runs
        THEN it reports that main is not covered."""
        expected = "check.yml: holds required checks but push branches are not exactly [main]"
        for push in (
            {"tags": ["v*"]},
            {"branches": ["main", "!main"]},
            {"branches": ["main", "!ma*"]},
            {"branches": ["main", "!ma+in"]},
        ):
            with self.subTest(push=push):
                workflow = self.workflow(on={"push": push, "pull_request": None, "merge_group": None, "workflow_dispatch": None})
                self.assertIn(expected, trigger_problems("check.yml", workflow))

    def test_a_cancelling_group_that_differs_per_run(self):
        """GIVEN a cancelling group that names both the pull request and the run
        WHEN the concurrency rule runs
        THEN it reports that a newer push could not cancel the older run."""
        group = "check-${{ github.event.pull_request.number || github.run_id }}-${{ github.run_id }}"
        workflow = self.workflow(concurrency={"group": group, "cancel-in-progress": PR_CANCEL})
        problems = concurrency_problems("check.yml", workflow)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("a cancelling group must be", problems[0])

    def test_a_save_guard_that_also_admits_pull_requests(self):
        """GIVEN a cache save whose `if` contains the main test but also allows pull requests
        WHEN the cache rule runs
        THEN it reports a save outside main, and the approved guard still passes."""
        loose = {"name": "s", "uses": "actions/cache/save@" + "0" * 40, "if": f"{ON_MAIN} || github.event_name == 'pull_request'"}
        exact = {"name": "t", "uses": "actions/cache/save@" + "0" * 40, "if": f"{ON_MAIN} && steps.deps.outputs.cache-hit != 'true'"}
        problems = cache_problems("check.yml", self.workflow(jobs={"j": {"steps": [loose, exact]}}))
        self.assertEqual(problems, ["check.yml: job j step 's' saves a cache outside main"])

    def test_a_secret_inherited_from_the_workflow_env(self):
        """GIVEN SONAR_TOKEN in the workflow-level env and a job that builds
        WHEN the secret rule runs
        THEN it reports the build, although the job itself names no secret."""
        workflow = self.workflow(
            env={"SONAR_TOKEN": "${{ secrets.SONAR_TOKEN }}"},
            jobs={"coverage": {"if": FORK_GUARD, "steps": [{"name": "build", "run": "cargo build"}]}},
        )
        problems = secret_problems("sonar.yml", workflow)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("runs a build or package command", problems[0])

    def test_an_inverted_fork_guard(self):
        """GIVEN a SONAR_TOKEN job whose `if` contains the repository test but admits every pull request
        WHEN the secret rule runs
        THEN it reports the missing fork guard."""
        guard = "github.event_name == 'pull_request' || github.event.pull_request.head.repo.full_name == github.repository"
        job = {"if": guard, "steps": [{"name": "scan", "env": {"SONAR_TOKEN": "${{ secrets.SONAR_TOKEN }}"}}]}
        problems = secret_problems("sonar.yml", self.workflow(jobs={"sonarqube": job}))
        self.assertEqual(problems, ["sonar.yml: job sonarqube holds SONAR_TOKEN without the fork guard"])


if __name__ == "__main__":
    unittest.main()
