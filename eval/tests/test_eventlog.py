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


if __name__ == "__main__":
    unittest.main()
