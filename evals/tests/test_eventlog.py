from __future__ import annotations

import unittest

from _path import LIB  # noqa: F401

from eventlog import extract_timeline
from spawn_metric import fixture_db


def _seq(events):
    return fixture_db(events)


class EventLogTests(unittest.TestCase):
    def test_offered_then_kept(self):
        con = _seq(
            [
                ("task_started", {"goal": "do work", "model": "m"}),
                ("plan_updated", {"steps": [{"step": "a", "status": "pending"}, {"step": "b", "status": "pending"}]}),
                ("context_snapshot", {"messages": []}),
                ("delegation_stage", {"action": "offered", "detail": "plan"}),
                ("context_snapshot", {"messages": []}),
                ("tool_call_started", {"name": "apply_patch"}),
                ("tool_call_finished", {"name": "apply_patch", "is_error": False}),
                ("delegation_stage", {"action": "kept", "detail": ""}),
                ("context_snapshot", {"messages": []}),
            ]
        )
        t = extract_timeline(con)
        self.assertTrue(t["offered"])
        self.assertEqual(t["offer_trigger"], "plan")
        self.assertTrue(t["kept"])
        self.assertFalse(t["spawn"])
        self.assertEqual(t["disposition"], "kept")
        self.assertTrue(t["engaged"])
        self.assertEqual(t["offer_round"], 2)
        self.assertEqual(t["first_edit_round"], 3)

    def test_offered_then_delegated(self):
        con = _seq(
            [
                ("plan_updated", {"steps": [{"step": "a"}, {"step": "b"}]}),
                ("delegation_stage", {"action": "offered", "detail": "plan"}),
                ("context_snapshot", {"messages": []}),
                ("sub_agent_started", {"id": "w1", "role": "worker", "nickname": "Ada", "task": "impl"}),
                ("delegation_stage", {"action": "delegated", "detail": "src/a.rs"}),
                ("delegation_stage", {"action": "ownership_granted", "detail": "w1: src/a.rs"}),
                ("tool_call_started", {"name": "apply_patch", "agent_id": "w1"}),
                ("context_snapshot", {"messages": []}),
            ]
        )
        t = extract_timeline(con)
        self.assertTrue(t["spawn"])
        self.assertEqual(t["disposition"], "delegated")
        self.assertEqual(t["spawn_metric"]["natural_spawn_count"], 1)
        self.assertEqual(t["ownership_granted"], 1)
        self.assertEqual(t["spawn_metric"]["useful_child_count"], 1)

    def test_kept_then_delayed_spawn(self):
        con = _seq(
            [
                ("delegation_stage", {"action": "offered", "detail": "plan"}),
                ("context_snapshot", {"messages": []}),
                ("tool_call_started", {"name": "apply_patch"}),
                ("tool_call_finished", {"name": "apply_patch", "is_error": False}),
                ("delegation_stage", {"action": "kept", "detail": ""}),
                ("context_snapshot", {"messages": []}),
                ("delegation_stage", {"action": "reoffered", "detail": "plan_progress"}),
                ("sub_agent_started", {"id": "w2", "role": "worker", "nickname": "Bea", "task": "tests"}),
                ("delegation_stage", {"action": "delegated", "detail": "tests/"}),
                ("context_snapshot", {"messages": []}),
            ]
        )
        t = extract_timeline(con)
        self.assertTrue(t["kept"])
        self.assertTrue(t["delegated"])
        self.assertTrue(t["delayed_spawn_after_keep"])
        self.assertEqual(t["disposition"], "delegated")

    def test_never_engaged_is_invalid(self):
        con = _seq(
            [
                ("tool_call_started", {"name": "read_file"}),
                ("context_snapshot", {"messages": []}),
            ]
        )
        t = extract_timeline(con)
        self.assertFalse(t["engaged"])
        self.assertFalse(t["valid"])
        self.assertEqual(t["disposition"], "none")

    def test_reviewer_does_not_count_as_spawn(self):
        con = _seq(
            [
                ("plan_updated", {"steps": [{"step": "a"}]}),
                ("sub_agent_started", {"id": "r1", "role": "reviewer", "nickname": "Rev"}),
            ]
        )
        t = extract_timeline(con)
        self.assertFalse(t["spawn"])
        self.assertEqual(t["spawn_metric"]["reviewer_children"], 1)
        self.assertEqual(t["reviewer"]["reviewer_spawned"], 1)
        self.assertFalse(t["reviewer"]["noise"])

    def test_missing_model_requests_is_null_not_zero(self):
        con = _seq([("plan_updated", {"steps": [{"step": "a"}]})])
        t = extract_timeline(con)
        self.assertIsNone(t["input_tokens"])
        self.assertIsNone(t["output_tokens"])
        self.assertIsNone(t["total_tokens"])
        self.assertIsNone(t["wall_time_ms"])

    def test_model_requests_are_summed(self):
        con = _seq([("plan_updated", {"steps": [{"step": "a"}]})])
        con.execute(
            "create table model_requests (input_tokens integer, output_tokens integer)"
        )
        con.execute("insert into model_requests values (10, 4)")
        con.execute("insert into model_requests values (6, 2)")
        t = extract_timeline(con)
        self.assertEqual(t["input_tokens"], 16)
        self.assertEqual(t["output_tokens"], 6)
        self.assertEqual(t["total_tokens"], 22)


class VerificationTruthTests(unittest.TestCase):
    """The ruler's first honesty rule: only a recorded pass counts as one.

    `verification_finished.passed` is the completion gate. It is true for a run
    that owed no check — a pull, a switch, a repository nothing had to prove —
    so reading it as "verification passed" is how an unverified run came to be
    counted as a passing one.
    """

    def _timeline(self, events):
        return extract_timeline(_seq(events))

    def test_a_verified_run_is_a_pass(self):
        t = self._timeline(
            [("verification_finished", {"passed": True, "verification": "passed"})]
        )
        self.assertTrue(t["verification_passed"])
        self.assertEqual(t["verification_status"], "passed")
        self.assertEqual(t["verification_truth_source"], "verification_finished")

    def test_a_failed_run_is_not_a_pass(self):
        t = self._timeline(
            [("verification_finished", {"passed": False, "verification": "failed"})]
        )
        self.assertFalse(t["verification_passed"])
        self.assertEqual(t["verification_status"], "failed")

    def test_an_unrun_verification_is_never_a_pass(self):
        t = self._timeline(
            [("verification_finished", {"passed": True, "verification": "not_run"})]
        )
        self.assertIsNone(t["verification_passed"])
        self.assertEqual(t["verification_status"], "not_run")

    def test_an_unavailable_verification_is_never_a_pass(self):
        t = self._timeline(
            [("verification_finished", {"passed": True, "verification": "unavailable"})]
        )
        self.assertIsNone(t["verification_passed"])
        self.assertEqual(t["verification_status"], "unavailable")

    def test_the_terminal_row_carries_the_truth_when_the_event_does_not(self):
        t = self._timeline(
            [
                ("verification_finished", {"passed": True}),
                ("task_finished", {"outcome": "completed", "verification": "passed"}),
            ]
        )
        self.assertTrue(t["verification_passed"])
        self.assertEqual(t["verification_truth_source"], "task_finished")

    def test_the_row_that_ran_the_checks_outranks_the_terminal_row(self):
        t = self._timeline(
            [
                ("verification_finished", {"passed": False, "verification": "failed"}),
                ("task_finished", {"outcome": "completed", "verification": "passed"}),
            ]
        )
        self.assertFalse(t["verification_passed"])
        self.assertEqual(t["verification_truth_source"], "verification_finished")

    def test_a_legacy_open_gate_is_unknown_and_never_a_pass(self):
        """An open gate with no verdict beside it. It cannot say whether the
        checks passed or simply had nothing to run, so it says neither."""
        t = self._timeline([("verification_finished", {"passed": True})])
        self.assertIsNone(t["verification_passed"])
        self.assertEqual(t["verification_status"], "legacy_unknown")
        self.assertEqual(t["verification_truth_source"], "legacy_completion_gate")

    def test_a_legacy_closed_gate_derives_a_failure(self):
        t = self._timeline([("verification_finished", {"passed": False})])
        self.assertFalse(t["verification_passed"])
        self.assertEqual(t["verification_status"], "failed")
        self.assertEqual(t["verification_truth_source"], "legacy_completion_gate")

    def test_a_run_that_recorded_nothing_says_nothing(self):
        t = self._timeline([("task_started", {"goal": "do work", "model": "m"})])
        self.assertIsNone(t["verification_passed"])
        self.assertIsNone(t["verification_status"])
        self.assertIsNone(t["verification_truth_source"])


class CheckAndReviewCountsTests(unittest.TestCase):
    """The ruler names what it counted.

    The durable log holds one row per verification CHECK and one per closure
    review STAGE. A check is not a test, and a finished stage says nothing
    about what a review found, so the record carries the counts it can
    support and no others.
    """

    def _timeline(self, events):
        return extract_timeline(_seq(events))

    def test_every_status_in_the_vocabulary_is_counted_as_a_check(self):
        t = self._timeline(
            [
                ("verification_check", {"name": "a", "status": "passed"}),
                ("verification_check", {"name": "b", "status": "failed"}),
                ("verification_check", {"name": "c", "status": "skipped"}),
                ("verification_check", {"name": "d", "status": "tool_missing"}),
                (
                    "verification_check",
                    {"name": "e", "status": "environment_unavailable"},
                ),
            ]
        )
        self.assertEqual(t["checks_total"], 5)
        self.assertEqual(t["checks_passed"], 1)

    def test_the_older_spellings_are_counted_and_are_not_passes(self):
        t = self._timeline(
            [
                ("verification_check", {"name": "a", "status": "toolmissing"}),
                (
                    "verification_check",
                    {"name": "b", "status": "environmentunavailable"},
                ),
                ("verification_check", {"name": "c", "status": "passed"}),
            ]
        )
        self.assertEqual(t["checks_total"], 3)
        self.assertEqual(t["checks_passed"], 1)

    def test_a_status_this_build_cannot_read_is_a_check_but_never_a_pass(self):
        t = self._timeline(
            [
                ("verification_check", {"name": "a", "status": "something-else"}),
                ("verification_check", {"name": "b", "status": None}),
            ]
        )
        self.assertEqual(t["checks_total"], 2)
        self.assertIsNone(t["checks_passed"])

    def test_checks_are_counted_as_checks_with_their_total(self):
        t = self._timeline(
            [
                ("verification_check", {"name": "cargo fmt", "status": "passed"}),
                ("verification_check", {"name": "cargo test", "status": "passed"}),
                ("verification_check", {"name": "cargo clippy", "status": "failed"}),
            ]
        )
        self.assertEqual(t["checks_passed"], 2)
        self.assertEqual(t["checks_total"], 3)
        self.assertNotIn("tests_passed", t)

    def test_a_check_that_did_not_run_is_not_counted_as_passed(self):
        t = self._timeline(
            [
                ("verification_check", {"name": "cargo test", "status": "skipped"}),
                ("verification_check", {"name": "cargo build", "status": "toolmissing"}),
            ]
        )
        self.assertIsNone(t["checks_passed"])
        self.assertEqual(t["checks_total"], 2)

    def test_no_checks_means_not_measured(self):
        t = self._timeline([("task_started", {"goal": "g", "model": "m"})])
        self.assertIsNone(t["checks_passed"])
        self.assertIsNone(t["checks_total"])

    def test_a_finished_review_is_counted_as_a_stage_not_as_findings(self):
        t = self._timeline(
            [
                ("review_stage", {"action": "launching", "detail": "required"}),
                ("review_stage", {"action": "finished_ok", "detail": "required"}),
                ("review_stage", {"action": "finished_incomplete", "detail": "required"}),
            ]
        )
        self.assertEqual(t["review_stages_ok"], 1)
        self.assertNotIn("review_findings", t)

    def test_no_review_means_not_measured(self):
        t = self._timeline([("task_started", {"goal": "g", "model": "m"})])
        self.assertIsNone(t["review_stages_ok"])


if __name__ == "__main__":
    unittest.main()
