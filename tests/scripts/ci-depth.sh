#!/usr/bin/env bash
# Exercise the workflow's actual planner and aggregate, without a VM or fixtures.
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
python3 - "$repo/.github/workflows/ci.yml" <<'PY'
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

import yaml

doc = yaml.safe_load(Path(sys.argv[1]).read_text())
events = doc.get("on", doc.get(True))
assert {"pull_request", "merge_group", "workflow_dispatch"} <= set(events)
assert not (events.get("pull_request") or {}).get("branches"), "all stack bases need PR checks"
jobs = doc["jobs"]
planner_step = next(s for s in jobs["changes"]["steps"] if s.get("id") == "c")
planner = planner_step["run"]
# GitHub sets every declared step variable, empty when its expression is, so
# the planner runs here with exactly the declared names and nothing inherited.
planner_env = {k: "" for k in planner_step.get("env", {})}
assert {"EVENT", "REF", "MQ_BASE", "MQ_HEAD", "PUSH_BEFORE", "PUSH_AFTER"} <= set(planner_env), planner_env
aggregate = next(s for s in jobs["ci-ok"]["steps"] if "NEEDS" in s.get("env", {}))
full = set(json.loads(aggregate["env"]["FULL_JOBS"]))
assert full <= set(jobs), "the aggregate must not exempt nonexistent jobs"
# The merge queue's full validation: fixtures, the x86_64 suite, the newer
# kernel, the reference-oracle VM tier, arm64, macOS and the CLI.
assert {"fixtures", "test", "test-kernel-new", "suite-in-vm",
        "test-arm64", "test-darwin", "cli"} <= full, full
assert set(jobs["ci-ok"]["needs"]) == set(jobs) - {"ci-ok"}

# Derive every intentionally skipped job from the dependency graph. A new
# full job must be declared, and a quick job must never receive an exemption.
expected = {n for n, j in jobs.items()
            if j.get("if") == "needs.changes.outputs.code == 'true'"}
while True:
    expanded = set(expected)
    for name, job in jobs.items():
        needs = job.get("needs", [])
        needs = [needs] if isinstance(needs, str) else needs
        if name != "ci-ok" and set(needs) & expected:
            expanded.add(name)
    if expanded == expected:
        break
    expected = expanded
assert full == expected, (full, expected)

core = Path(sys.argv[1]).parents[2].parent / "rust-fs-core" / "scripts" / "code-changed.sh"
assert core.is_file(), f"{core} missing: run `chore siblings` first"

def plan(cwd, event, ref, **declared):
    # Only declared step variables may be set; the rest stay empty as on GitHub.
    assert set(declared) <= set(planner_env), declared
    output = Path(cwd) / "output"
    output.unlink(missing_ok=True)
    env = {k: v for k, v in os.environ.items() if k not in planner_env}
    env.update(planner_env, EVENT=event, REF=ref, GITHUB_OUTPUT=str(output), **declared)
    result = subprocess.run(["bash", "-c", planner], cwd=cwd, env=env,
                            capture_output=True, text=True)
    assert result.returncode == 0, (event, declared, result.stderr)
    return output.read_text().splitlines()

with tempfile.TemporaryDirectory() as tmp:
    for event, ref, decision in [
        ("pull_request", "refs/pull/1/merge", "false"),
        ("merge_group", "refs/heads/gh-readonly-queue/main/pr-1", "true"),
        ("workflow_call", "refs/tags/v1.0.0", "true"),
        ("workflow_dispatch", "refs/heads/ci/oracle-red", "true"),
        ("push", "refs/heads/main", "true"),
        ("push", "refs/tags/v1.0.0", "true"),
        ("unexpected", "", "true"),
    ]:
        assert plan(tmp, event, ref) == ["code=" + decision], (event, ref)

# A real merge group: a temporary repository with a documentation-only commit
# and a code commit on top, beside the real rust-fs-core sibling the step
# reads as ../rust-fs-core, so the planner diffs actual base and head SHAs.
with tempfile.TemporaryDirectory() as tmp:
    work = Path(tmp) / "repo"
    work.mkdir()
    (Path(tmp) / "rust-fs-core").symlink_to(core.parents[1], target_is_directory=True)
    git_env = dict(os.environ, GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1",
                   GIT_AUTHOR_NAME="ci-depth", GIT_AUTHOR_EMAIL="ci-depth@invalid",
                   GIT_COMMITTER_NAME="ci-depth", GIT_COMMITTER_EMAIL="ci-depth@invalid")

    def commit(path, text):
        (work / path).parent.mkdir(parents=True, exist_ok=True)
        (work / path).write_text(text)
        for args in (["add", path], ["commit", "-qm", path]):
            subprocess.run(["git", *args], cwd=work, env=git_env, check=True)
        return subprocess.run(["git", "rev-parse", "HEAD"], cwd=work, env=git_env,
                              check=True, capture_output=True, text=True).stdout.strip()

    subprocess.run(["git", "init", "-q", str(work)], env=git_env, check=True)
    base = commit("src/lib.rs", "// base\n")
    docs = commit("README.md", "docs\n")
    code = commit("src/lib.rs", "// changed\n")
    queue = "refs/heads/gh-readonly-queue/main/pr-1"
    for mq_base, mq_head, decision, paths in [
        (base, docs, "false", ["README.md"]),
        (docs, code, "true", ["src/lib.rs"]),
        (base, code, "true", ["README.md", "src/lib.rs"]),
    ]:
        assert plan(work, "merge_group", queue, MQ_BASE=mq_base, MQ_HEAD=mq_head) == \
            ["code=" + decision], (mq_base, mq_head)
        changed = (work / "changed.txt").read_text().splitlines()
        assert changed == paths, (mq_base, mq_head, changed)
        (work / "changed.txt").unlink()
        # A push to main diffs the same way; a pull request never does.
        assert plan(work, "push", "refs/heads/main", PUSH_BEFORE=mq_base,
                    PUSH_AFTER=mq_head) == ["code=" + decision]
        (work / "changed.txt").unlink()
        assert plan(work, "pull_request", "refs/pull/1/merge", MQ_BASE=mq_base,
                    MQ_HEAD=mq_head, PUSH_BEFORE=mq_base, PUSH_AFTER=mq_head) == ["code=false"]
        assert not (work / "changed.txt").exists(), "a pull request must not diff"
    # Half a range, an unknown before or a branch creation is never a skip.
    for declared in [dict(MQ_BASE=base), dict(MQ_HEAD=docs)]:
        assert plan(work, "merge_group", queue, **declared) == ["code=true"], declared
    for before in ["0" * 40, "f" * 40]:
        assert plan(work, "push", "refs/heads/main", PUSH_BEFORE=before,
                    PUSH_AFTER=docs) == ["code=true"], before

def verdict(code, results):
    env = dict(os.environ, CODE=code, NEEDS=json.dumps(results), FULL_JOBS=json.dumps(sorted(full)))
    return subprocess.run(["bash", "-c", aggregate["run"]], env=env,
                          capture_output=True, text=True)

success = {n: {"result": "success"} for n in jobs["ci-ok"]["needs"]}
quick = {n: {"result": "skipped" if n in full else "success"} for n in success}
for code, results in [("true", success), ("false", quick)]:
    result = verdict(code, results)
    assert result.returncode == 0, result.stdout + result.stderr

for code in ["", "invalid"]:
    assert verdict(code, success).returncode != 0, "a missing decision must fail closed"
for name in success:
    for state in ["failure", "cancelled", "skipped"]:
        bad = dict(success)
        bad[name] = {"result": state}
        assert verdict("true", bad).returncode != 0, ("full", name, state)
        if name not in full or state != "skipped":
            bad = dict(quick)
            bad[name] = {"result": state}
            assert verdict("false", bad).returncode != 0, ("quick", name, state)
print("ci-depth: event planning, stack triggers, and aggregate failure cases passed")
PY
