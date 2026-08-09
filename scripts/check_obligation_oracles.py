#!/usr/bin/env python3
"""Prove each obligation oracle can tell broken from fixed, before it scores anything.

A whole-acceptance verdict says a run failed. It does not say which part of the
requirement it failed, and N3 showed why that matters: two consumers carry the
same semantics, do not call each other, and only one of them regressed. Scoring
that needs one oracle per obligation.

An oracle is only usable if it discriminates in both directions:

    untouched fixture (+ overlay)   →  oracle MUST FAIL
    same, plus the reference fix    →  oracle MUST PASS

An oracle that cannot fail measures nothing — the mistake that made N6 vacuous.
One that cannot pass blames the agent for a defect in the harness. Anything that
does not prove both is reported ORACLE_UNPROVEN and must not carry a causal
conclusion.

Oracles come in two shapes, matching what the behaviour actually lives on:

  `oracle`        a Go test file dropped into `oracle_package`, run with
                  `-run oracle_run`. For behaviour observable in-process.
  `oracle_shell`  a script run at the workspace root. For behaviour that is a
                  property of the process — exit status, stderr, file output.

Usage:
    python3 scripts/check_obligation_oracles.py [--case n3-caller-propagation]
"""

from __future__ import annotations

import argparse
import glob
import os
import subprocess
import sys
import tempfile

import yaml

OBLIGATIONS_DIR = "evals/navigation/obligations"
CASES_DIR = "evals/navigation"
REFERENCE_DIR = "evals/navigation/reference"


def materialize(case: dict, dest: str) -> None:
    """The workspace as the eval builds it: clone, overlay, commit."""
    repo = case.get("repo")
    if repo:
        subprocess.run(
            ["git", "clone", "--local", "--quiet", os.path.abspath(repo), dest],
            check=True,
        )
        subprocess.run(["git", "remote", "remove", "origin"], cwd=dest, check=False)
    else:
        os.makedirs(dest, exist_ok=True)
    for rel, content in (case.get("files") or {}).items():
        path = os.path.join(dest, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as handle:
            handle.write(content)
    for args in (
        ["init", "-q"], ["config", "user.email", "o@leveler"],
        ["config", "user.name", "oracle"], ["add", "-A"],
        ["commit", "-qm", "obligation baseline"],
    ):
        subprocess.run(["git", *args], cwd=dest, check=False,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def run_oracle(obligation: dict, workspace: str) -> tuple[bool, str]:
    """(passed, detail). Never raises — a broken oracle is a result, not a crash."""
    env = {**os.environ, "TMPDIR": os.environ.get("TMPDIR") or "/tmp"}
    if "oracle_shell" in obligation:
        script = os.path.join(workspace, ".obligation_oracle.sh")
        with open(script, "w") as handle:
            handle.write(obligation["oracle_shell"])
        done = subprocess.run(["bash", ".obligation_oracle.sh"], cwd=workspace,
                              env=env, capture_output=True)
        os.remove(script)
        return done.returncode == 0, done.stderr.decode()[-200:]

    package = obligation["oracle_package"]
    target = os.path.join(workspace, package, "zz_obligation_oracle_test.go")
    os.makedirs(os.path.dirname(target), exist_ok=True)
    with open(target, "w") as handle:
        handle.write(obligation["oracle"])
    done = subprocess.run(
        ["go", "test", f"./{package}/", "-run", obligation["oracle_run"], "-count=1"],
        cwd=workspace, env=env, capture_output=True,
    )
    os.remove(target)
    return done.returncode == 0, done.stdout.decode()[-200:]


def apply_reference(case_id: str, workspace: str) -> bool:
    patch = os.path.join(REFERENCE_DIR, f"{case_id}.patch")
    if not os.path.exists(patch):
        return False
    done = subprocess.run(["git", "apply", os.path.abspath(patch)],
                          cwd=workspace, capture_output=True)
    return done.returncode == 0


def check_case(case_id: str) -> dict[str, str]:
    spec = yaml.safe_load(open(f"{OBLIGATIONS_DIR}/{case_id}.yaml"))
    case = yaml.safe_load(open(f"{CASES_DIR}/{case_id}.yaml"))
    verdicts: dict[str, str] = {}

    with tempfile.TemporaryDirectory(prefix="obligation-oracle-") as tmp:
        broken = os.path.join(tmp, "broken")
        materialize(case, broken)
        fixed = os.path.join(tmp, "fixed")
        materialize(case, fixed)
        has_reference = apply_reference(case_id, fixed)

        for obligation in spec["obligations"]:
            oid = obligation["id"]
            broke_pass, broke_detail = run_oracle(obligation, broken)
            if broke_pass:
                verdicts[oid] = "ORACLE_UNPROVEN (passes on the broken fixture)"
                print(f"  {oid:34} UNPROVEN — passes untouched")
                continue
            if not has_reference:
                verdicts[oid] = "ORACLE_UNPROVEN (no reference fix)"
                print(f"  {oid:34} UNPROVEN — fails untouched, no reference fix")
                continue
            fixed_pass, fixed_detail = run_oracle(obligation, fixed)
            if not fixed_pass:
                verdicts[oid] = "ORACLE_UNPROVEN (fails with the reference fix)"
                print(f"  {oid:34} UNPROVEN — reference fix does not satisfy it")
                print(f"      {fixed_detail.strip()[:150]}")
                continue
            verdicts[oid] = "PROVEN"
            print(f"  {oid:34} PROVEN (fails broken, passes fixed)")
    return verdicts


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--case")
    args = ap.parse_args()

    cases = sorted(
        os.path.basename(p)[:-5] for p in glob.glob(f"{OBLIGATIONS_DIR}/*.yaml")
    )
    if args.case:
        cases = [c for c in cases if c == args.case]

    print("obligation oracle validity — must fail broken, must pass fixed\n")
    all_verdicts: dict[str, dict[str, str]] = {}
    for case_id in cases:
        print(f"{case_id}:")
        all_verdicts[case_id] = check_case(case_id)

    unproven = [
        f"{case}/{oid}"
        for case, v in all_verdicts.items()
        for oid, verdict in v.items()
        if verdict != "PROVEN"
    ]
    print()
    if unproven:
        print(f"ORACLE_UNPROVEN: {', '.join(unproven)}")
        print("these obligations must not carry a causal conclusion")
        sys.exit(1)
    total = sum(len(v) for v in all_verdicts.values())
    print(f"all {total} obligation oracles proven in both directions")


if __name__ == "__main__":
    main()
