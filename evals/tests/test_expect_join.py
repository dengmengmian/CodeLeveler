"""The independent `expect` verdict must reach the run record.

`leveler eval run --json-out` already runs each case's `expect`. The observer
is the part that was missing: without this join every run scores
`task_success = None` and the experiment cannot answer its primary question.
"""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from _path import LIB  # noqa: F401

from runner import load_expect_verdicts


def _eval_result(cases: list[dict]) -> Path:
    d = Path(tempfile.mkdtemp(prefix="expect-join-"))
    p = d / "eval_result.json"
    p.write_text(json.dumps({"kind": "run", "report": {"cases": cases}}), encoding="utf-8")
    return p


class ExpectJoin(unittest.TestCase):
    def test_passed_case_is_a_scored_success(self):
        p = _eval_result([
            {"id": "rust-race-counter", "expect_passed": True, "verification_ran": True,
             "verification_evidence": {"program": "cargo", "args": ["test", "--quiet"]}},
        ])
        v = load_expect_verdicts(p)
        self.assertEqual(v["rust-race-counter"]["ran"], True)
        self.assertEqual(v["rust-race-counter"]["passed"], True)
        self.assertIn("cargo", v["rust-race-counter"]["command"])

    def test_failed_case_is_a_scored_failure_not_a_skip(self):
        p = _eval_result([
            {"id": "ts-redact-secrets", "expect_passed": False, "verification_ran": True},
        ])
        v = load_expect_verdicts(p)
        self.assertEqual(v["ts-redact-secrets"]["ran"], True)
        self.assertEqual(v["ts-redact-secrets"]["passed"], False)

    def test_verifier_that_did_not_run_is_unscored_not_a_failure(self):
        p = _eval_result([
            {"id": "icg-2-bug-fix", "expect_passed": None, "verification_ran": False},
        ])
        v = load_expect_verdicts(p)
        self.assertEqual(v["icg-2-bug-fix"]["ran"], False)
        self.assertIsNone(v["icg-2-bug-fix"]["passed"])

    def test_repetitions_collapse_to_all_must_pass(self):
        p = _eval_result([
            {"id": "t", "repetition": 1, "expect_passed": True, "verification_ran": True},
            {"id": "t", "repetition": 2, "expect_passed": False, "verification_ran": True},
        ])
        v = load_expect_verdicts(p)
        self.assertEqual(v["t"]["passed"], False)

    def test_missing_file_yields_no_verdicts_not_a_crash(self):
        self.assertEqual(load_expect_verdicts(Path("/nonexistent/eval_result.json")), {})


if __name__ == "__main__":
    unittest.main()
