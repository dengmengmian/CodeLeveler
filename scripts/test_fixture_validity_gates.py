#!/usr/bin/env python3
"""Prove the validity checker catches each defect that has actually shipped.

Every scenario below is a defect this suite already produced once, reduced to
the smallest fixture that reproduces its structure. They run on synthetic
cases rather than the real ones so the tripwire keeps working while N1-N8 are
being repaired — and so a repair cannot quietly make the tripwire vacuous.

    python3 scripts/test_fixture_validity_gates.py
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile

import yaml

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import check_fixture_validity as cfv  # noqa: E402


def write_case(cases_dir: str, case_id: str, *, files: dict, expect: str,
               maintained: list, semantics: str = "UNAMBIGUOUS",
               existing_tests_unmodified: bool = True,
               forbidden: list | None = None) -> str:
    case = {
        "id": case_id,
        "name": case_id,
        "files": files,
        "forbidden_edit_paths": forbidden or [],
        "expect": {"program": "bash", "args": ["-c", expect]},
        "validity": {
            "maintained_checks": maintained,
            "constraints": {"existing_tests_unmodified": existing_tests_unmodified},
            "semantics": {
                "status": semantics,
                "critical_observable": "synthetic",
                "oracle_interpretation": "synthetic",
                "rationale": "synthetic fixture for the checker's own tests",
            },
        },
    }
    path = os.path.join(cases_dir, f"{case_id}.yaml")
    with open(path, "w") as handle:
        yaml.safe_dump(case, handle)
    return path


def make_reference(reference_dir: str, case_id: str, case_path: str,
                   edits: dict) -> None:
    """Produce a real unified diff by editing a materialised workspace."""
    case = yaml.safe_load(open(case_path))
    with tempfile.TemporaryDirectory(prefix="mkref-") as tmp:
        work = os.path.join(tmp, "w")
        cfv.materialize(case, work)
        for rel, content in edits.items():
            with open(os.path.join(work, rel), "w") as handle:
                handle.write(content)
        diff = subprocess.run(["git", "diff"], cwd=work, capture_output=True)
        with open(os.path.join(reference_dir, f"{case_id}.patch"), "wb") as handle:
            handle.write(diff.stdout)


def scenario(name: str, *, files: dict, expect: str, maintained: list,
             reference: dict | None, semantics: str = "UNAMBIGUOUS",
             existing_tests_unmodified: bool = True,
             forbidden: list | None = None) -> dict:
    with tempfile.TemporaryDirectory(prefix="gatetest-") as tmp:
        cases_dir = os.path.join(tmp, "cases")
        reference_dir = os.path.join(tmp, "reference")
        os.makedirs(cases_dir)
        os.makedirs(reference_dir)
        path = write_case(cases_dir, name, files=files, expect=expect,
                          maintained=maintained, semantics=semantics,
                          existing_tests_unmodified=existing_tests_unmodified,
                          forbidden=forbidden)
        if reference is not None:
            make_reference(reference_dir, name, path, reference)
        previous, cfv.REFERENCE_DIR = cfv.REFERENCE_DIR, reference_dir
        try:
            return cfv.check(path)
        finally:
            cfv.REFERENCE_DIR = previous


FAILURES: list[str] = []


def expect_result(label: str, result: dict, *, status: str,
                  reason: str | None = None, gate: tuple | None = None) -> None:
    problems = []
    if result["status"] != status:
        problems.append(f"status {result['status']!r}, want {status!r}")
    if reason and not any(reason in r for r in result["reasons"]):
        problems.append(f"reasons {result['reasons']} missing {reason!r}")
    if gate and result["gates"][gate[0]] != gate[1]:
        problems.append(f"{gate[0]}={result['gates'][gate[0]]!r}, want {gate[1]!r}")
    if problems:
        FAILURES.append(f"{label}: " + "; ".join(problems))
        print(f"  FAIL  {label}\n        " + "\n        ".join(problems))
    else:
        print(f"  ok    {label}")


def main() -> None:
    print("validity gate regressions\n")

    # N6's defect: the described bug does not exist, so an untouched workspace
    # already satisfies the acceptance and the case scores nothing.
    expect_result(
        "BROKEN_FIXTURE_PASSES — a case that cannot fail is INVALID",
        scenario("broken-passes",
                 files={"marker.txt": "FIXED\n"},
                 expect="grep -q FIXED marker.txt",
                 maintained=[["true"]],
                 reference=None),
        status=cfv.INVALID, reason="BROKEN_FIXTURE_PASSES", gate=("G1", cfv.FAIL))

    # N3's defect: the reference satisfies the hidden acceptance while breaking
    # a check the repository maintains. Hidden acceptance alone cannot see it.
    expect_result(
        "MAINTAINED_TEST_CONFLICT — acceptance green, maintained suite red",
        scenario("maintained-conflict",
                 files={"marker.txt": "KEEP\n"},
                 expect="grep -q FIXED marker.txt",
                 maintained=[["bash", "-c", "grep -q KEEP marker.txt"]],
                 reference={"marker.txt": "FIXED\n"}),
        status=cfv.INVALID, reason="MAINTAINED_CHECKS_FAIL", gate=("G3", cfv.FAIL))

    # N4's defect: everything executable is green, but the task text admits a
    # second reading the oracle rejects.
    expect_result(
        "SEMANTICS_UNDER_SPECIFIED — all gates green but semantics contested",
        scenario("under-specified",
                 files={"marker.txt": "KEEP\n"},
                 expect="grep -q FIXED marker.txt",
                 maintained=[["bash", "-c", "grep -q FIXED marker.txt"]],
                 reference={"marker.txt": "FIXED\n"},
                 semantics="UNDER_SPECIFIED"),
        status=cfv.UNDER_SPECIFIED, reason="SEMANTICS_UNDER_SPECIFIED",
        gate=("G6", cfv.FAIL))

    # A case nobody has proved solvable is not a case yet.
    expect_result(
        "REFERENCE_MISSING — unproved reference direction is UNVERIFIED",
        scenario("no-reference",
                 files={"marker.txt": "KEEP\n"},
                 expect="grep -q FIXED marker.txt",
                 maintained=[["true"]],
                 reference=None),
        status=cfv.UNVERIFIED, reason="REFERENCE_MISSING", gate=("G2", cfv.FAIL))

    # The reference must not buy its green by rewriting the tests the task
    # told the solver to leave alone.
    expect_result(
        "EXISTING_TEST_MODIFIED — reference rewrites a maintained test",
        scenario("test-rewrite",
                 files={"marker.txt": "KEEP\n", "thing_test.go": "// original\n"},
                 expect="grep -q FIXED marker.txt",
                 maintained=[["true"]],
                 reference={"marker.txt": "FIXED\n", "thing_test.go": "// rewritten\n"}),
        status=cfv.INVALID, reason="EXISTING_TEST_MODIFIED", gate=("G5", cfv.FAIL))

    # …and must not edit a path the case declares off limits.
    expect_result(
        "FORBIDDEN_PATH_EDITED — reference touches a forbidden path",
        scenario("forbidden-edit",
                 files={"marker.txt": "KEEP\n", "legacy/old.go": "// dead\n"},
                 expect="grep -q FIXED marker.txt",
                 maintained=[["true"]],
                 reference={"marker.txt": "FIXED\n", "legacy/old.go": "// touched\n"},
                 forbidden=["legacy/old.go"]),
        status=cfv.INVALID, reason="FORBIDDEN_PATH_EDITED", gate=("G5", cfv.FAIL))

    # The positive control: without it, a checker that always says INVALID
    # would pass every test above.
    expect_result(
        "VALID — every gate green really does yield VALID",
        scenario("all-green",
                 files={"marker.txt": "KEEP\n"},
                 expect="grep -q FIXED marker.txt",
                 maintained=[["bash", "-c", "grep -q FIXED marker.txt"]],
                 reference={"marker.txt": "FIXED\n"}),
        status=cfv.VALID)

    print()
    if FAILURES:
        print(f"{len(FAILURES)} gate regression(s) failed")
        sys.exit(1)
    print("all gate regressions pass")


if __name__ == "__main__":
    main()
