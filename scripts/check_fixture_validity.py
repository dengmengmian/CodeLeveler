#!/usr/bin/env python3
"""Prove a benchmark case measures something, before letting a model near it.

Three separate defects have shipped in this suite, each invisible to the check
that was in place at the time:

  N6  the described bug did not exist, so the hidden acceptance passed on an
      untouched workspace and scored the model on repairing nothing.
  N3  the semantically correct fix breaks a maintained test that feeds
      Valid-unset records and expects them counted, while the task forbids
      touching tests. The case cannot be passed at all.
  N4  the hidden acceptance turns on whether a null sink reports what it
      accepted or what it wrote, and the task says both.

The first was caught by checking the broken direction. The second and third
were not caught by anything, because a reference fix that satisfies its own
hidden acceptance can still violate the task's stated constraints, break the
repository's own suite, or encode one of several defensible readings. Proving
an oracle is not proving a case.

So validity is a conjunction of six gates, and a case is usable only if all
six hold:

  G1  broken state demonstrably fails
  G2  reference solution satisfies the full task   (derived: G3 ∧ G4 ∧ G5 ∧ G6)
  G3  maintained repository checks pass
  G4  hidden acceptance passes
  G5  explicit task constraints are respected
  G6  requirement semantics are unambiguous

G6 cannot be inferred mechanically and is not guessed here: it is a
benchmark-author attestation carried in the case's `validity.semantics` block.
A case whose reference is green in every executable direction is still not
VALID if the task text admits a second reasonable implementation the oracle
would reject.

Statuses are distinct on purpose. UNVERIFIED (nothing proved it either way)
and UNDER_SPECIFIED (semantics admit more than one reading) are NOT VALID, and
neither is silently promoted.

Every case is checked even after one fails, so a repair round sees the whole
matrix instead of discovering the next defect only after fixing this one.

Exit status is 0 only when every case is VALID.

Usage:
    python3 scripts/check_fixture_validity.py [--cases evals/navigation]
                                              [--only n6-large-file-region]
                                              [--json out.json]
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import shutil
import subprocess
import sys
import tempfile

import yaml

# Reference patches live beside the cases they prove: <cases-dir>/reference/.
# Kept as a module global so the checker's own tests can point it elsewhere.
REFERENCE_DIR = "evals/navigation/reference"


def reference_dir_for(cases_dir: str) -> str:
    return os.path.join(cases_dir, "reference")

VALID = "VALID"
INVALID = "INVALID"
UNVERIFIED = "UNVERIFIED"
UNDER_SPECIFIED = "UNDER_SPECIFIED"

PASS = "PASS"
FAIL = "FAIL"
SKIP = "n/a"


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


def _env() -> dict:
    return {**os.environ, "TMPDIR": os.environ.get("TMPDIR") or "/tmp"}


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
            ["bash", ".fixture_check.sh"], cwd=workspace, env=_env(),
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        os.remove(script)
        return completed.returncode
    completed = subprocess.run(
        [program, *args], cwd=workspace, env=_env(),
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    return completed.returncode


def run_maintained(case: dict, workspace: str) -> tuple[bool, str]:
    """The repository's own checks, which the hidden acceptance may not cover.

    N3's acceptance ran only `go test ./internal/report/`; the conflict it
    missed lived in the same package's pre-existing test.
    """
    checks = (case.get("validity") or {}).get("maintained_checks") or []
    if not checks:
        return False, "no maintained_checks declared"
    for command in checks:
        done = subprocess.run(list(command), cwd=workspace, env=_env(),
                              capture_output=True)
        if done.returncode != 0:
            detail = (done.stdout.decode() + done.stderr.decode()).strip()
            return False, f"{' '.join(command)} → exit {done.returncode}: {detail[-300:]}"
    return True, ""


def test_fingerprints(workspace: str) -> dict[str, str]:
    """Path → content for every test file already in the workspace."""
    out = {}
    for root, dirs, files in os.walk(workspace):
        dirs[:] = [d for d in dirs if d != ".git"]
        for name in files:
            if not name.endswith("_test.go"):
                continue
            path = os.path.join(root, name)
            with open(path, "rb") as handle:
                out[os.path.relpath(path, workspace)] = handle.read().hex()
    return out


def check_constraints(case: dict, baseline: str, fixed: str) -> tuple[bool, list[str]]:
    """Explicit task constraints the reference must not buy its green with.

    Checked against the baseline→reference diff, so a fixture repair by the
    benchmark author is never mistaken for the solution editing tests.
    """
    reasons = []
    declared = (case.get("validity") or {}).get("constraints") or {}

    forbidden = case.get("forbidden_edit_paths") or []
    if forbidden:
        done = subprocess.run(["git", "diff", "--name-only", "--", *forbidden],
                              cwd=fixed, capture_output=True)
        touched = [p for p in done.stdout.decode().split("\n") if p.strip()]
        if touched:
            reasons.append(f"FORBIDDEN_PATH_EDITED: {', '.join(touched)}")

    if declared.get("existing_tests_unmodified"):
        before, after = test_fingerprints(baseline), test_fingerprints(fixed)
        changed = sorted(p for p in before if after.get(p) != before[p])
        removed = sorted(p for p in before if p not in after)
        if changed or removed:
            reasons.append(
                "EXISTING_TEST_MODIFIED: " + ", ".join(changed + removed))

    return not reasons, reasons


def check_semantics(case: dict) -> tuple[str, str]:
    """Benchmark-author attestation. Deliberately not inferred from the text."""
    semantics = (case.get("validity") or {}).get("semantics") or {}
    status = semantics.get("status")
    if status not in ("UNAMBIGUOUS", "UNDER_SPECIFIED", "UNVERIFIED"):
        return "UNVERIFIED", "no semantics attestation"
    return status, (semantics.get("rationale") or "").strip()


def apply_reference(case_id: str, workspace: str) -> tuple[bool, str]:
    patch = os.path.join(REFERENCE_DIR, f"{case_id}.patch")
    if not os.path.exists(patch):
        return False, "REFERENCE_MISSING"
    done = subprocess.run(["git", "apply", os.path.abspath(patch)],
                          cwd=workspace, capture_output=True)
    if done.returncode != 0:
        return False, f"REFERENCE_DOES_NOT_APPLY: {done.stderr.decode().strip()[:160]}"
    return True, ""


def check(path: str) -> dict:
    case = yaml.safe_load(open(path))
    case_id = case["id"]
    gates = dict.fromkeys(("G1", "G2", "G3", "G4", "G5", "G6"), SKIP)
    reasons: list[str] = []

    with tempfile.TemporaryDirectory(prefix="fixture-validity-") as tmp:
        # G1 — the broken state must actually fail.
        broken = os.path.join(tmp, "broken")
        materialize(case, broken)
        if run_acceptance(case, broken) == 0:
            gates["G1"] = FAIL
            reasons.append("BROKEN_FIXTURE_PASSES")
            gates["G2"] = FAIL
            return dict(case=case_id, gates=gates, status=INVALID, reasons=reasons)
        gates["G1"] = PASS

        # Recovery ground truth — a recovery case must start from the failure
        # shape it claims, or it scores recovery from something else entirely
        # ("only baseline non-zero" is how that mistake ships). Declared as
        # validity.recovery_baseline: {build: fail|pass, tests: fail|pass}.
        shape = (case.get("validity") or {}).get("recovery_baseline") or {}
        for check_name, command in (("build", ["go", "build", "./..."]),
                                    ("tests", ["go", "test", "./..."])):
            want = shape.get(check_name)
            if want is None:
                continue
            done = subprocess.run(command, cwd=broken, env=_env(), capture_output=True)
            got = "pass" if done.returncode == 0 else "fail"
            if got != want:
                gates["G1"] = FAIL
                reasons.append(
                    f"RECOVERY_BASELINE_SHAPE: {check_name} is {got}, case claims {want}")
                return dict(case=case_id, gates=gates, status=INVALID, reasons=reasons)

        # G6 — attestation, independent of any reference.
        semantics, _ = check_semantics(case)
        gates["G6"] = PASS if semantics == "UNAMBIGUOUS" else FAIL
        if semantics == "UNDER_SPECIFIED":
            reasons.append("SEMANTICS_UNDER_SPECIFIED")
        elif semantics == "UNVERIFIED":
            reasons.append("SEMANTICS_UNVERIFIED")

        # G3/G4/G5 need a reference solution.
        fixed = os.path.join(tmp, "fixed")
        materialize(case, fixed)
        baseline = os.path.join(tmp, "baseline")
        shutil.copytree(fixed, baseline)
        applied, why = apply_reference(case_id, fixed)
        if not applied:
            reasons.append(why)
            gates["G2"] = FAIL
            status = INVALID if why.startswith("REFERENCE_DOES_NOT_APPLY") else UNVERIFIED
            if semantics == "UNDER_SPECIFIED":
                status = UNDER_SPECIFIED if status == UNVERIFIED else status
            return dict(case=case_id, gates=gates, status=status, reasons=reasons)

        maintained_ok, maintained_why = run_maintained(case, fixed)
        gates["G3"] = PASS if maintained_ok else FAIL
        if not maintained_ok:
            reasons.append("MAINTAINED_CHECKS_FAIL")
            reasons.append(maintained_why)

        gates["G4"] = PASS if run_acceptance(case, fixed) == 0 else FAIL
        if gates["G4"] == FAIL:
            reasons.append("REFERENCE_HIDDEN_ACCEPTANCE_FAIL")

        constraints_ok, constraint_reasons = check_constraints(case, baseline, fixed)
        gates["G5"] = PASS if constraints_ok else FAIL
        reasons.extend(constraint_reasons)

    # G2 is derived: the reference satisfies the whole task only if every
    # executable direction and the semantics attestation hold.
    gates["G2"] = PASS if all(gates[g] == PASS for g in ("G3", "G4", "G5", "G6")) else FAIL

    if all(gates[g] == PASS for g in ("G1", "G2", "G3", "G4", "G5", "G6")):
        status = VALID
    elif gates["G6"] == FAIL and all(gates[g] == PASS for g in ("G1", "G3", "G4", "G5")):
        status = UNDER_SPECIFIED
    else:
        status = INVALID
    return dict(case=case_id, gates=gates, status=status, reasons=reasons)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--cases", default="evals/navigation")
    ap.add_argument("--only")
    ap.add_argument("--json")
    args = ap.parse_args()

    if not shutil.which("git"):
        sys.exit("git is required")

    print("case validity — six gates, all must hold\n")
    print(f"{'case':32}" + "".join(f"{g:>6}" for g in
                                   ("G1", "G2", "G3", "G4", "G5", "G6")) + "   status")

    results = []
    global REFERENCE_DIR
    REFERENCE_DIR = reference_dir_for(args.cases)
    for path in sorted(glob.glob(f"{args.cases}/*.yaml")):
        case_id = yaml.safe_load(open(path))["id"]
        if args.only and args.only != case_id:
            continue
        # Every case runs: a repair round must see the whole matrix at once.
        result = check(path)
        results.append(result)
        g = result["gates"]
        row = "".join(f"{g[k]:>6}" for k in ("G1", "G2", "G3", "G4", "G5", "G6"))
        print(f"{result['case']:32}{row}   {result['status']}")
        for reason in result["reasons"]:
            print(f"      {reason[:150]}")

    if args.json:
        with open(args.json, "w") as handle:
            json.dump(results, handle, indent=2)

    by_status = {s: [r["case"] for r in results if r["status"] == s]
                 for s in (VALID, INVALID, UNDER_SPECIFIED, UNVERIFIED)}
    print(f"\nVALID {len(by_status[VALID])}/{len(results)}")
    for status in (INVALID, UNDER_SPECIFIED, UNVERIFIED):
        if by_status[status]:
            print(f"{status}: {', '.join(by_status[status])}")

    if len(by_status[VALID]) != len(results):
        print("\nbenchmark NOT ready — do not treat model results as capability evidence")
        sys.exit(1)
    print("\nbenchmark ready — every case is VALID")


if __name__ == "__main__":
    main()
