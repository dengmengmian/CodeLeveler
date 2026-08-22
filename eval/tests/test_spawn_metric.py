"""Frozen Gate V2 extractor tests (EX1–EX7)."""

from __future__ import annotations

import unittest

from _path import LIB  # noqa: F401

from spawn_metric import extract_con, fixture_db


def _child(cid: str, role: str = "default", **kw):
    return ("sub_agent_started", {"id": cid, "role": role, **kw})


class SpawnMetricTests(unittest.TestCase):
    def test_ex1_natural_spawn_counted(self):
        r = extract_con(fixture_db([_child("c1"), _child("c2")]))
        self.assertEqual(r["natural_spawn_count"], 2)

    def test_ex2_reviewer_only_is_zero(self):
        r = extract_con(fixture_db([_child("reviewer-1", role="reviewer")]))
        self.assertEqual(r["natural_spawn_count"], 0)
        self.assertEqual(r["reviewer_children"], 1)

    def test_ex3_inline_spawn_without_tool_call(self):
        r = extract_con(
            fixture_db(
                [
                    ("tool_call_started", {"name": "read_file"}),
                    _child("c1"),
                    ("tool_call_started", {"name": "apply_patch", "agent_id": "c1"}),
                ]
            )
        )
        self.assertEqual(r["natural_spawn_count"], 1)
        self.assertEqual(r["old_toolcall_spawn_count"], 0)

    def test_ex4_duplicate_projection_counted_once(self):
        r = extract_con(fixture_db([_child("c1"), _child("c1")]))
        self.assertEqual(r["natural_spawn_count"], 1)
        self.assertEqual(r["duplicate_child_projections"], 1)

    def test_ex5_replay_does_not_double_count(self):
        replay = [_child("c1"), _child("c2"), _child("c1"), _child("c2"), _child("c3")]
        r = extract_con(fixture_db(replay))
        self.assertEqual(r["natural_spawn_count"], 3)

    def test_ex6_child_originated_spawn_separated(self):
        r = extract_con(fixture_db([_child("c1"), _child("g1", agent_id="c1")]))
        self.assertEqual(r["natural_spawn_count"], 1)
        self.assertEqual(r["child_originated_spawn_count"], 1)

    def test_ex7_useful_child_needs_mutation(self):
        r = extract_con(
            fixture_db(
                [
                    _child("c1"),
                    _child("c2"),
                    ("delegation_stage", {"action": "ownership_granted", "detail": "c1: src/"}),
                    ("delegation_stage", {"action": "ownership_granted", "detail": "c2: lib/"}),
                    ("tool_call_started", {"name": "apply_patch", "agent_id": "c1"}),
                    ("tool_call_started", {"name": "read_file", "agent_id": "c2"}),
                ]
            )
        )
        self.assertEqual(r["useful_child_count"], 1)


if __name__ == "__main__":
    unittest.main()
