"""MA-VALUE-001 metrics: task success, efficiency, child utility.

Spawn rate is recorded, never a success criterion. KEEP / no-spawn on the
multi arm is a first-class outcome, not a fail.
"""

from __future__ import annotations

import unittest

from _path import LIB  # noqa: F401

from metrics import describe
from schema import make_run, validate_run
from spawn_metric import extract_con, fixture_db
from value import (
    CHILD_CONTRIBUTIONS,
    SUCCESS_METRICS,
    classify_child_contributions,
    child_result_used,
    compare_value_arms,
    decide_value,
    profile_effectiveness,
    value_eval_result,
    value_summary,
)


def _child(cid: str, role: str = "explorer", **kw):
    return ("sub_agent_started", {"id": cid, "role": role, **kw})


def _run(
    *,
    task: str,
    mode: str,
    success: bool | None,
    turns: int,
    tokens: int | None,
    duration: int | None,
    spawn: bool,
    child_used: bool,
    tool_calls: int = 10,
    tests_passed: int | None = None,
    regressions: int | None = None,
) -> dict:
    timeline = {
        "valid": True,
        "engaged": True,
        "spawn": spawn,
        "disposition": "delegated" if spawn else "kept",
        "rounds": turns,
        "parent_tool_calls": tool_calls,
        "input_tokens": tokens,
        "output_tokens": 0 if tokens is not None else None,
        "total_tokens": tokens,
        "wall_time_ms": duration,
        "child_result_used": child_used,
        "spawn_metric": {
            "natural_spawn_count": int(spawn),
            "useful_child_count": int(child_used and spawn),
            "parent_tool_calls": tool_calls,
            "children": [{"id": "c1", "role": "explorer"}] if spawn else [],
        },
    }
    run = make_run(
        run_id=f"{task}-{mode}-1",
        started_at=None,
        git_sha=None,
        binary=None,
        leveler_home=None,
        session_db=None,
        task_id=task,
        suite="multi_agent",
        max_rounds=None,
        expected_disposition=None,
        arm_name=mode,
        arm_factor="mode",
        arm_value="single_agent" if mode == "single" else "multi_agent",
        model_ref="m",
        timeline=timeline,
        verifier_ran=success is not None,
        verifier_passed=success,
        experiment="MA-VALUE-001",
        mode=mode,
    )
    run["task_success"] = success
    run["quality"] = {
        "tests_passed": tests_passed,
        "regressions": regressions,
        "review_findings": None,
        "missed_issues": None,
    }
    return run


class ContributionTests(unittest.TestCase):
    def test_contribution_vocabulary_is_fixed(self):
        self.assertEqual(
            CHILD_CONTRIBUTIONS,
            (
                "exploration_reduction",
                "bug_found",
                "plan_improvement",
                "verification_improvement",
                "context_reduction",
            ),
        )

    def test_explorer_consumed_without_mutation_is_used(self):
        con = fixture_db(
            [
                _child("e1", role="explorer"),
                ("sub_agent_finished", {"id": "e1", "ok": True, "summary": "found src/lib.rs"}),
                ("tool_call_started", {"name": "read_file"}),
                ("tool_call_started", {"name": "apply_patch"}),
            ]
        )
        spawn = extract_con(con)
        self.assertEqual(spawn["useful_child_count"], 0)
        self.assertTrue(child_result_used(spawn))

    def test_finished_child_with_no_parent_followup_is_not_used(self):
        con = fixture_db(
            [
                _child("e1", role="explorer"),
                ("sub_agent_finished", {"id": "e1", "ok": True, "summary": "looked around"}),
            ]
        )
        spawn = extract_con(con)
        self.assertFalse(child_result_used(spawn))

    def test_explorer_then_parent_edit_is_exploration_reduction(self):
        con = fixture_db(
            [
                _child("e1", role="explorer"),
                ("sub_agent_finished", {"id": "e1", "ok": True, "summary": "bug in parse.rs"}),
                ("tool_call_started", {"name": "apply_patch"}),
            ]
        )
        labels = classify_child_contributions(extract_con(con))
        self.assertIn("exploration_reduction", labels)

    def test_reviewer_findings_are_verification_improvement(self):
        con = fixture_db(
            [
                _child("r1", role="reviewer"),
                ("sub_agent_finished", {"id": "r1", "ok": True, "summary": "off-by-one"}),
                ("tool_call_started", {"name": "resolve_finding"}),
            ]
        )
        labels = classify_child_contributions(extract_con(con))
        self.assertIn("verification_improvement", labels)

    def test_accepted_finding_is_bug_found(self):
        con = fixture_db(
            [
                _child("w1", role="worker"),
                (
                    "tool_call_started",
                    {"name": "report_finding", "agent_id": "w1"},
                ),
                ("sub_agent_finished", {"id": "w1", "ok": True, "summary": "race"}),
                ("tool_call_started", {"name": "resolve_finding"}),
            ]
        )
        labels = classify_child_contributions(extract_con(con))
        self.assertIn("bug_found", labels)


class ValueCompareTests(unittest.TestCase):
    def test_spawn_rate_is_not_a_success_metric(self):
        self.assertNotIn("spawn_rate", SUCCESS_METRICS)
        self.assertIn("task_success", SUCCESS_METRICS)

    def test_compare_does_not_verdict_on_spawn_rate(self):
        control = [
            _run(
                task="R005",
                mode="single",
                success=True,
                turns=80,
                tokens=100_000,
                duration=3_600_000,
                spawn=False,
                child_used=False,
            )
            for _ in range(6)
        ]
        treatment = [
            _run(
                task="R005",
                mode="multi",
                success=True,
                turns=40,
                tokens=60_000,
                duration=2_000_000,
                spawn=True,
                child_used=True,
            )
            for _ in range(6)
        ]
        cmp_ = compare_value_arms(control, treatment)
        self.assertNotIn("delta_spawn_rate", cmp_)
        self.assertNotEqual(cmp_.get("verdict"), "treatment_higher")
        self.assertIn(cmp_["verdict"], ("pass", "fail", "insufficient_n"))

    def test_pass_requires_success_hold_plus_one_gain_plus_child_used(self):
        control = [
            _run(
                task="R008",
                mode="single",
                success=True,
                turns=60,
                tokens=80_000,
                duration=2_000_000,
                spawn=False,
                child_used=False,
            )
            for _ in range(6)
        ]
        treatment = [
            _run(
                task="R008",
                mode="multi",
                success=True,
                turns=40,
                tokens=80_000,
                duration=2_000_000,
                spawn=True,
                child_used=True,
            )
            for _ in range(6)
        ]
        decision = decide_value(compare_value_arms(control, treatment))
        self.assertEqual(decision["verdict"], "pass")
        self.assertTrue(decision["success_held"])
        self.assertTrue(decision["child_output_used"])
        self.assertTrue(any(decision["improvements"].values()))

    def test_success_drop_is_fail_even_if_cheaper(self):
        control = [
            _run(
                task="R008",
                mode="single",
                success=True,
                turns=60,
                tokens=80_000,
                duration=2_000_000,
                spawn=False,
                child_used=False,
            )
            for _ in range(6)
        ]
        treatment = [
            _run(
                task="R008",
                mode="multi",
                success=False,
                turns=20,
                tokens=10_000,
                duration=100_000,
                spawn=True,
                child_used=True,
            )
            for _ in range(6)
        ]
        decision = decide_value(compare_value_arms(control, treatment))
        self.assertEqual(decision["verdict"], "fail")
        self.assertFalse(decision["success_held"])

    def test_spawn_without_parent_consumption_cannot_pass(self):
        control = [
            _run(
                task="R006",
                mode="single",
                success=True,
                turns=50,
                tokens=70_000,
                duration=1_800_000,
                spawn=False,
                child_used=False,
            )
            for _ in range(6)
        ]
        treatment = [
            _run(
                task="R006",
                mode="multi",
                success=True,
                turns=30,
                tokens=50_000,
                duration=1_200_000,
                spawn=True,
                child_used=False,
            )
            for _ in range(6)
        ]
        decision = decide_value(compare_value_arms(control, treatment))
        self.assertEqual(decision["verdict"], "fail")
        self.assertFalse(decision["child_output_used"])

    def test_n_below_six_is_insufficient_not_a_product_verdict(self):
        control = [
            _run(
                task="R005",
                mode="single",
                success=True,
                turns=10,
                tokens=1_000,
                duration=1_000,
                spawn=False,
                child_used=False,
            )
            for _ in range(3)
        ]
        treatment = [
            _run(
                task="R005",
                mode="multi",
                success=True,
                turns=8,
                tokens=900,
                duration=900,
                spawn=True,
                child_used=True,
            )
            for _ in range(3)
        ]
        decision = decide_value(compare_value_arms(control, treatment))
        self.assertEqual(decision["verdict"], "insufficient_n")
        self.assertNotEqual(decision["interpretation"], "multi_agent_has_no_value")

    def test_value_eval_result_is_backward_compatible_projection(self):
        run = _run(
            task="R005",
            mode="multi",
            success=True,
            turns=12,
            tokens=4_000,
            duration=90_000,
            spawn=True,
            child_used=True,
        )
        self.assertEqual(validate_run(run), [])
        doc = value_eval_result(run)
        self.assertEqual(doc["experiment"], "MA-VALUE-001")
        self.assertEqual(doc["mode"], "multi")
        self.assertTrue(doc["task_success"])
        self.assertEqual(doc["metrics"]["turns"], 12)
        self.assertEqual(doc["metrics"]["tokens"], 4_000)
        self.assertEqual(doc["metrics"]["duration"], 90_000)
        self.assertGreaterEqual(doc["multi_agent"]["spawn_count"], 1)
        self.assertTrue(doc["multi_agent"]["child_result_used"])

    def test_missing_tokens_stay_null_not_zero(self):
        run = _run(
            task="R005",
            mode="single",
            success=True,
            turns=5,
            tokens=None,
            duration=None,
            spawn=False,
            child_used=False,
        )
        summary = value_summary([run])
        self.assertIsNone(summary["tokens"]["mean"])
        self.assertIsNone(summary["wall_time_ms"]["mean"])
        self.assertEqual(describe([]), {"n": 0, "mean": None, "median": None, "variance": None, "min": None, "max": None})


class ProfileEffectivenessTests(unittest.TestCase):
    def test_explorer_counts_findings_from_contribution(self):
        con = fixture_db(
            [
                (
                    "sub_agent_started",
                    {
                        "id": "e1",
                        "role": "explorer",
                        "profile_id": "explorer",
                        "profile_role": "explorer",
                        "capabilities": ["repository_analysis"],
                    },
                ),
                (
                    "sub_agent_finished",
                    {
                        "id": "e1",
                        "ok": True,
                        "summary": "found",
                        "contribution": {
                            "child_id": "e1",
                            "role": "explorer",
                            "profile_id": "explorer",
                            "findings_total": 7,
                            "findings_accepted": 5,
                            "findings_verified": 2,
                        },
                    },
                ),
            ]
        )
        buckets = profile_effectiveness(extract_con(con))
        e = buckets["explorer"]
        self.assertEqual(e["spawned"], 1)
        self.assertEqual(e["completed"], 1)
        self.assertEqual(e["findings_generated"], 7)
        self.assertEqual(e["findings_accepted"], 5)
        self.assertEqual(e["findings_verified"], 2)
        self.assertEqual(e["capabilities"], ["repository_analysis"])

    def test_reviewer_bugs_found_and_confirmed(self):
        con = fixture_db(
            [
                ("sub_agent_started", {"id": "r1", "role": "reviewer", "profile_id": "reviewer"}),
                (
                    "sub_agent_finished",
                    {
                        "id": "r1",
                        "ok": True,
                        "summary": "off-by-one",
                        "contribution": {
                            "findings_total": 3,
                            "findings_accepted": 2,
                            "findings_verified": 1,
                        },
                    },
                ),
            ]
        )
        r = profile_effectiveness(extract_con(con))["reviewer"]
        self.assertEqual(r["bugs_found"], 3)
        self.assertEqual(r["bugs_confirmed"], 2)

    def test_worker_changes_and_verification(self):
        con = fixture_db(
            [
                ("sub_agent_started", {"id": "w1", "role": "worker", "profile_id": "worker"}),
                ("delegation_stage", {"action": "ownership_granted", "detail": "w1: src/a.rs"}),
                ("tool_call_started", {"name": "apply_patch", "agent_id": "w1"}),
                ("sub_agent_finished", {"id": "w1", "ok": True, "summary": "done"}),
            ]
        )
        w = profile_effectiveness(extract_con(con))["worker"]
        self.assertEqual(w["changes_accepted"], 1)
        self.assertEqual(w["verification_passed"], 1)

    def test_legacy_event_without_profile_id_falls_back_to_role(self):
        con = fixture_db(
            [
                ("sub_agent_started", {"id": "e1", "role": "explorer"}),
                ("sub_agent_finished", {"id": "e1", "ok": True, "summary": "ok"}),
            ]
        )
        buckets = profile_effectiveness(extract_con(con))
        self.assertIn("explorer", buckets)
        self.assertEqual(buckets["explorer"]["profile_id"], "explorer")
        self.assertIsNone(extract_con(con)["children"][0]["profile_id"])


if __name__ == "__main__":
    unittest.main()
