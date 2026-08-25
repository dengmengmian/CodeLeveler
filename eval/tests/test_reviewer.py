"""Reviewer value metrics: useful findings, not finding count."""

from __future__ import annotations

import unittest

from _path import LIB  # noqa: F401

from reviewer import (
    REVIEWER_SUCCESS_METRICS,
    compare_reviewer_arms,
    decide_reviewer,
    extract_reviewer,
    reviewer_eval_result,
)
from spawn_metric import extract_con, fixture_db


def _started(cid: str, role: str = "reviewer", **kw):
    return ("sub_agent_started", {"id": cid, "role": role, **kw})


def _finished(cid: str, ok: bool = True, contribution: dict | None = None):
    body = {"id": cid, "ok": ok, "summary": "review"}
    if contribution is not None:
        body["contribution"] = contribution
    return ("sub_agent_finished", body)


def _contrib(*, total=0, accepted=0, verified=0, rejected=0):
    return {
        "child_id": "r1",
        "role": "reviewer",
        "findings_total": total,
        "findings_accepted": accepted,
        "findings_verified": verified,
        "findings_rejected": rejected,
    }


def _run(*, mode: str, success: bool | None, spawned: int, useful: int, noise: bool = False, n_extra: int = 0):
    rev = {
        "reviewer_spawned": spawned,
        "reviewer_completed": spawned,
        "findings_generated": useful + (3 if noise else 0),
        "findings_accepted": useful,
        "findings_verified": useful,
        "findings_rejected": 0,
        "useful_findings": useful,
        "zero_findings": spawned > 0 and useful == 0 and not noise,
        "noise": noise,
    }
    return {
        "experiment": "MA-VALUE-REVIEWER-PILOT",
        "mode": mode,
        "task_success": success,
        "efficiency": {"turns": 20, "total_tokens": 10_000, "wall_time_ms": 60_000, "tool_calls": 30},
        "reviewer": rev,
        "edits": {"rounds": 20},
    }


class ExtractTests(unittest.TestCase):
    def test_zero_findings_is_valid_not_a_fail_signal(self):
        con = fixture_db(
            [
                _started("r1"),
                _finished("r1", contribution=_contrib(total=0)),
            ]
        )
        rev = extract_reviewer(extract_con(con))
        self.assertEqual(rev["reviewer_spawned"], 1)
        self.assertEqual(rev["findings_generated"], 0)
        self.assertTrue(rev["zero_findings"])
        self.assertFalse(rev["noise"])
        self.assertEqual(rev["useful_findings"], 0)

    def test_accepted_findings_are_useful(self):
        con = fixture_db(
            [
                _started("r1"),
                _finished(
                    "r1",
                    contribution=_contrib(total=3, accepted=2, verified=1, rejected=1),
                ),
            ]
        )
        rev = extract_reviewer(extract_con(con))
        self.assertEqual(rev["findings_generated"], 3)
        self.assertEqual(rev["findings_accepted"], 2)
        self.assertEqual(rev["findings_verified"], 1)
        self.assertEqual(rev["useful_findings"], 2)
        self.assertFalse(rev["noise"])

    def test_unjudged_findings_are_noise(self):
        con = fixture_db(
            [
                _started("r1"),
                _finished("r1", contribution=_contrib(total=4, accepted=0, rejected=0)),
            ]
        )
        rev = extract_reviewer(extract_con(con))
        self.assertTrue(rev["noise"])
        self.assertEqual(rev["useful_findings"], 0)

    def test_rejected_findings_are_not_noise(self):
        con = fixture_db(
            [
                _started("r1"),
                _finished("r1", contribution=_contrib(total=2, accepted=0, rejected=2)),
            ]
        )
        rev = extract_reviewer(extract_con(con))
        self.assertFalse(rev["noise"])
        self.assertEqual(rev["findings_rejected"], 2)

    def test_explorer_is_not_a_reviewer(self):
        con = fixture_db(
            [
                _started("e1", role="explorer"),
                _finished("e1", contribution=_contrib(total=9, accepted=9)),
            ]
        )
        rev = extract_reviewer(extract_con(con))
        self.assertEqual(rev["reviewer_spawned"], 0)
        self.assertEqual(rev["useful_findings"], 0)

    def test_no_reviewer_projects_zeros(self):
        rev = extract_reviewer(extract_con(fixture_db([])))
        self.assertEqual(rev["reviewer_spawned"], 0)
        self.assertFalse(rev["zero_findings"])
        self.assertFalse(rev["noise"])


class DecisionTests(unittest.TestCase):
    def test_finding_count_is_not_a_success_metric(self):
        self.assertNotIn("findings_generated", REVIEWER_SUCCESS_METRICS)
        self.assertIn("useful_findings", REVIEWER_SUCCESS_METRICS)
        self.assertIn("task_success", REVIEWER_SUCCESS_METRICS)

    def test_n_five_is_insufficient_not_a_product_verdict(self):
        control = [_run(mode="self", success=True, spawned=0, useful=0) for _ in range(5)]
        treatment = [_run(mode="reviewer", success=True, spawned=1, useful=1) for _ in range(5)]
        decision = compare_reviewer_arms(control, treatment)
        self.assertEqual(decision["verdict"], "insufficient_n")
        self.assertNotEqual(decision["interpretation"], "reviewer_has_no_value")

    def test_pass_requires_success_hold_and_useful_findings(self):
        control = [_run(mode="self", success=True, spawned=0, useful=0) for _ in range(6)]
        treatment = [_run(mode="reviewer", success=True, spawned=1, useful=1) for _ in range(6)]
        decision = compare_reviewer_arms(control, treatment)
        self.assertEqual(decision["verdict"], "pass")
        self.assertTrue(decision["success_held"])
        self.assertTrue(decision["useful_findings"])
        self.assertTrue(decision["reviewer_ran"])

    def test_success_drop_is_fail(self):
        control = [_run(mode="self", success=True, spawned=0, useful=0) for _ in range(6)]
        treatment = [_run(mode="reviewer", success=False, spawned=1, useful=1) for _ in range(6)]
        decision = compare_reviewer_arms(control, treatment)
        self.assertEqual(decision["verdict"], "fail")
        self.assertFalse(decision["success_held"])

    def test_noise_without_useful_findings_is_fail(self):
        control = [_run(mode="self", success=True, spawned=0, useful=0) for _ in range(6)]
        treatment = [
            _run(mode="reviewer", success=True, spawned=1, useful=0, noise=True) for _ in range(6)
        ]
        decision = compare_reviewer_arms(control, treatment)
        self.assertEqual(decision["verdict"], "fail")
        self.assertTrue(decision["noise_regression"])

    def test_reviewer_that_never_ran_cannot_pass(self):
        control = [_run(mode="self", success=True, spawned=0, useful=0) for _ in range(6)]
        treatment = [_run(mode="reviewer", success=True, spawned=0, useful=0) for _ in range(6)]
        decision = compare_reviewer_arms(control, treatment)
        self.assertEqual(decision["verdict"], "fail")
        self.assertEqual(decision["interpretation"], "reviewer_did_not_run")

    def test_zero_findings_with_held_success_is_not_a_pass(self):
        comparison = decide_reviewer(
            {
                "success_held": True,
                "quality_improved": False,
                "useful_findings": False,
                "reviewer_ran": True,
                "noise_regression": False,
                "insufficient_n": False,
            }
        )
        self.assertEqual(comparison["verdict"], "fail")
        self.assertNotEqual(comparison["interpretation"], "reviewer_has_no_value")

    def test_compact_result_is_additive(self):
        run = _run(mode="reviewer", success=True, spawned=1, useful=2)
        doc = reviewer_eval_result(run)
        self.assertEqual(doc["experiment"], "MA-VALUE-REVIEWER-PILOT")
        self.assertEqual(doc["mode"], "reviewer")
        self.assertTrue(doc["task_success"])
        self.assertEqual(doc["reviewer"]["useful_findings"], 2)
        self.assertEqual(doc["reviewer"]["spawned"], 1)


if __name__ == "__main__":
    unittest.main()
