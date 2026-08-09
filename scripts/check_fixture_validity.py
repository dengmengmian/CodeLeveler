#!/usr/bin/env python3
"""Prove a benchmark case can fail before letting a model near it.

A bug-fix case is only a measurement if the bug exists. N6 shipped pointing at
a fixture whose jsonl decoder already handled `depth` correctly, so its hidden
acceptance passed on an untouched workspace — the case scored a model on
repairing something that was never broken, and the run that "failed" it was in
fact behaving correctly.

So every such case must satisfy a ground-truth contract:

    untouched fixture + overlay   →  hidden acceptance MUST FAIL
    same, plus a reference fix    →  hidden acceptance MUST PASS

The first half catches a case that cannot fail. The second catches one that
cannot pass — an acceptance broken in the other direction, which would blame
the agent for a defect in the harness.

The acceptance is executed **verbatim**, the way the eval executes it: the
script text from the YAML is written to a file and run with bash, cwd set to
the workspace. It is never re-quoted, re-escaped or passed through `bash -c`.
An earlier validity check did exactly that, mangled a `<<'GO'` heredoc, and
reported a false FAIL for N6 — which is how the broken case survived review.

Reference fixes live in `evals/navigation/reference/<case-id>.patch` as unified
diffs. A case with no reference patch is reported as UNVERIFIED rather than
silently assumed good.

Usage:
    python3 scripts/check_fixture_validity.py [--cases evals/navigation]
                                              [--only n6-large-file-region]
"""

from __future__ import annotations

import argparse
import glob
import os
import shutil
import subprocess
import sys
import tempfile

import yaml

REFERENCE_DIR = "evals/navigation/reference"


def materialize(case: dict, dest: str) -> None:
    """Build the workspace exactly as the eval does: clone, then overlay."""
    repo = case.get("repo")
    if not repo:
        os.makedirs(dest, exist_ok=True)
    else:
        subprocess.run(
            ["git", "clone", "--local", "--quiet", os.path.abspath(repo), dest],
            check=True,
        )
        subprocess.run(["git", "remote", "remove", "origin"], cwd=dest, check=False)
    for rel, content in (case.get("files") or {}).items():
        path = os.path.join(dest, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as handle:
            handle.write(content)
    for args in (
        ["init", "-q"],
        ["config", "user.email", "fixture@leveler"],
        ["config", "user.name", "fixture"],
        ["add", "-A"],
        ["commit", "-qm", "fixture baseline"],
    ):
        subprocess.run(["git", *args], cwd=dest, check=False,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def run_acceptance(case: dict, workspace: str) -> int:
    """Run the case's `expect` command verbatim, cwd = workspace."""
    expect = case["expect"]
    program, args = expect["program"], list(expect["args"])
    # A bash `-c` script is written out and executed as a file rather than
    # re-passed as an argument: quoting it again is how heredocs get mangled.
    if program == "bash" and args and args[0] == "-c":
        script = os.path.join(workspace, ".fixture_check.sh")
        with open(script, "w") as handle:
            handle.write(args[-1])
        completed = subprocess.run(
            ["bash", ".fixture_check.sh"],
            cwd=workspace,
            env={**os.environ, "TMPDIR": os.environ.get("TMPDIR") or "/tmp"},
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        os.remove(script)
        return completed.returncode
    completed = subprocess.run(
        [program, *args], cwd=workspace,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    return completed.returncode


def apply_reference(case_id: str, workspace: str) -> bool:
    patch = os.path.join(REFERENCE_DIR, f"{case_id}.patch")
    if not os.path.exists(patch):
        return False
    completed = subprocess.run(
        ["git", "apply", os.path.abspath(patch)],
        cwd=workspace,
        capture_output=True,
    )
    if completed.returncode != 0:
        print(f"    reference patch failed to apply: "
              f"{completed.stderr.decode().strip()[:200]}")
        return False
    return True


def check(path: str) -> str:
    case = yaml.safe_load(open(path))
    case_id = case["id"]
    verdicts = []

    with tempfile.TemporaryDirectory(prefix="fixture-validity-") as tmp:
        broken = os.path.join(tmp, "broken")
        materialize(case, broken)
        rc = run_acceptance(case, broken)
        if rc == 0:
            print(f"  {case_id:32} BROKEN-FIXTURE-PASSES  ← the case cannot fail")
            return "INVALID"
        verdicts.append("fails-untouched")

        fixed = os.path.join(tmp, "fixed")
        materialize(case, fixed)
        if not apply_reference(case_id, fixed):
            print(f"  {case_id:32} UNVERIFIED (no reference fix; untouched rc={rc})")
            return "UNVERIFIED"
        rc_fixed = run_acceptance(case, fixed)
        if rc_fixed != 0:
            print(f"  {case_id:32} REFERENCE-FIX-FAILS    ← the case cannot pass (rc={rc_fixed})")
            return "INVALID"
        verdicts.append("passes-with-fix")

    print(f"  {case_id:32} VALID ({', '.join(verdicts)})")
    return "VALID"


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--cases", default="evals/navigation")
    ap.add_argument("--only")
    args = ap.parse_args()

    if not shutil.which("git"):
        sys.exit("git is required")

    print("fixture validity — untouched must FAIL, reference fix must PASS\n")
    results = {}
    for path in sorted(glob.glob(f"{args.cases}/*.yaml")):
        case_id = yaml.safe_load(open(path))["id"]
        if args.only and args.only != case_id:
            continue
        results[case_id] = check(path)

    invalid = [c for c, v in results.items() if v == "INVALID"]
    print()
    for case_id, verdict in results.items():
        print(f"{case_id:34}{verdict}")
    if invalid:
        print(f"\nINVALID: {', '.join(invalid)} — do not run a model against these")
        sys.exit(1)
    print("\nno invalid fixture")


if __name__ == "__main__":
    main()
