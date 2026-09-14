from __future__ import annotations

import unittest

from _path import LIB  # noqa: F401

from ab import aggregate, arm_order, judge_run


def _lifecycle(**over):
    base = {
        "children": 0, "by_outcome": {}, "by_stop": {}, "child_success": 0, "child_partial": 0,
        "untyped_terminals": 0, "lost_accepted_child": 0, "duplicate_settlement": 0, "open_orphan": 0,
        "interrupted": 0, "resumed": 0, "ownership_violation": 0, "ownership_violations": [],
        "unattributed_child_mutations": 0, "child_mutations": {}, "child_read_paths": 0,
        "parent_rereads_after_child": 0,
    }
    base.update(over)
    return base


def _run(**over):
    run = {
        "case": "c", "category": "PARALLELIZABLE_IMPLEMENTATION", "arm": "treatment", "rep": 0,
        "expect_pass": True, "visible_checks_pass": True, "task_outcome": "completed",
        "verification_status": "passed", "rounds": 10, "wall_s": 100.0, "children": [],
        "child_terminals": {}, "changed_files": [], "lifecycle": _lifecycle(),
        "usage": {"requests": 5, "input_tokens": 1000, "output_tokens": 100,
                  "cached_input_tokens": 800, "cost_usd_micros": 50, "unpriced_requests": 0},
    }
    run.update(over)
    return run


class ArmOrderTests(unittest.TestCase):
    def test_every_arm_leads_equally_often_across_slots(self):
        arms = ["a", "b", "c"]
        leads = [arm_order(arms, slot)[0] for slot in range(6)]
        self.assertEqual(sorted(leads), ["a", "a", "b", "b", "c", "c"])
        self.assertEqual(sorted(arm_order(arms, 4)), arms)


class TruthTests(unittest.TestCase):
    def test_verified_while_its_own_checks_fail_is_false_verified(self):
        j = judge_run(_run(verification_status="passed", visible_checks_pass=False, expect_pass=False))
        self.assertTrue(j["false_verified"])
        self.assertTrue(j["incorrect_and_verified"])

    def test_verified_but_failing_the_independent_oracle_is_incorrect_and_verified_only(self):
        j = judge_run(_run(verification_status="passed", visible_checks_pass=True, expect_pass=False))
        self.assertFalse(j["false_verified"])
        self.assertTrue(j["incorrect_and_verified"])

    def test_completed_but_incorrect_is_a_completion_truth_miss_not_false_verified(self):
        j = judge_run(_run(verification_status="not_run", expect_pass=False, visible_checks_pass=False))
        self.assertFalse(j["false_verified"])
        self.assertTrue(j["completed_but_incorrect"])

    def test_a_legacy_completed_alias_counts_as_completed(self):
        j = judge_run(_run(task_outcome="completed_unverified", verification_status="not_run", expect_pass=False))
        self.assertTrue(j["completed_but_incorrect"])


class UsefulDelegationTests(unittest.TestCase):
    def test_a_worker_whose_retained_change_passes_is_an_independent_subtask(self):
        j = judge_run(_run(
            children=[{"id": "w", "role": "worker", "read_only": False}],
            child_terminals={"w": {"outcome": "completed_with_findings", "stop": "completed"}},
            lifecycle=_lifecycle(children=1, child_success=1, child_mutations={"w": ["slug/slug.go"]}),
            changed_files=["slug/slug.go"],
        ))
        self.assertTrue(j["delegated"])
        self.assertEqual(j["useful_children"], {"w": "independent_subtask"})
        self.assertTrue(j["useful_delegation"])
        self.assertFalse(j["unnecessary_delegation"])

    def test_a_worker_whose_change_did_not_survive_is_not_useful(self):
        j = judge_run(_run(
            children=[{"id": "w", "role": "worker", "read_only": False}],
            child_terminals={"w": {"outcome": "completed_with_findings", "stop": "completed"}},
            lifecycle=_lifecycle(children=1, child_success=1, child_mutations={"w": ["slug/slug.go"]}),
            changed_files=[],
        ))
        self.assertEqual(j["useful_children"], {})
        self.assertTrue(j["unnecessary_delegation"])

    def test_an_explorer_with_findings_the_parent_did_not_redo_is_evidence_used(self):
        j = judge_run(_run(
            children=[{"id": "e", "role": "explorer", "read_only": True}],
            child_terminals={"e": {"outcome": "completed_with_findings", "stop": "completed"}},
            lifecycle=_lifecycle(children=1, child_success=1, child_read_paths=4, parent_rereads_after_child=1),
        ))
        self.assertEqual(j["useful_children"], {"e": "evidence_not_redone"})

    def test_an_explorer_whose_reads_the_parent_all_redid_is_not_useful(self):
        j = judge_run(_run(
            children=[{"id": "e", "role": "explorer", "read_only": True}],
            child_terminals={"e": {"outcome": "completed_with_findings", "stop": "completed"}},
            lifecycle=_lifecycle(children=1, child_success=1, child_read_paths=2, parent_rereads_after_child=2),
        ))
        self.assertEqual(j["useful_children"], {})

    def test_a_reviewer_is_an_independent_review_but_not_model_delegation(self):
        j = judge_run(_run(
            children=[{"id": "r", "role": "reviewer", "read_only": True}],
            child_terminals={"r": {"outcome": "completed_no_findings", "stop": "completed"}},
        ))
        self.assertFalse(j["delegated"])
        self.assertTrue(j["independent_review"])
        self.assertFalse(j["unnecessary_delegation"])

    def test_any_spawn_on_a_simple_task_is_unnecessary(self):
        j = judge_run(_run(
            category="SIMPLE",
            children=[{"id": "w", "role": "worker", "read_only": False}],
            child_terminals={"w": {"outcome": "completed_with_findings", "stop": "completed"}},
            lifecycle=_lifecycle(children=1, child_success=1, child_mutations={"w": ["clamp.go"]}),
            changed_files=["clamp.go"],
        ))
        self.assertTrue(j["unnecessary_delegation"])


class AggregateTests(unittest.TestCase):
    def test_rates_totals_and_unknown_cost(self):
        runs = [judge_run(_run()), judge_run(_run(expect_pass=False, wall_s=300.0,
                                                  usage={"requests": 7, "input_tokens": 2000, "output_tokens": 50,
                                                         "cached_input_tokens": 0, "cost_usd_micros": None,
                                                         "unpriced_requests": 2}))]
        agg = aggregate(runs)
        self.assertEqual(agg["n"], 2)
        self.assertEqual(agg["task_success_rate"], 0.5)
        self.assertEqual(agg["wall_s_median"], 200.0)
        self.assertEqual(agg["requests_mean"], 6.0)
        self.assertIsNone(agg["cost_usd_micros_total"])
        self.assertEqual(agg["unpriced_requests"], 2)
        self.assertEqual(agg["false_verified_total"], 0)
        self.assertIsNone(agg["useful_delegation_rate"])

    def test_child_rates_use_children_as_the_denominator(self):
        run = judge_run(_run(lifecycle=_lifecycle(children=4, child_success=2, child_partial=1, open_orphan=1)))
        agg = aggregate([run])
        self.assertEqual(agg["child_success_rate"], 0.5)
        self.assertEqual(agg["child_partial_rate"], 0.25)
        self.assertEqual(agg["open_orphan_total"], 1)


if __name__ == "__main__":
    unittest.main()
