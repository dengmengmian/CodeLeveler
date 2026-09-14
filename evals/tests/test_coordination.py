from __future__ import annotations

import json
import sqlite3
import unittest

from _path import LIB  # noqa: F401

from coordination import coordination, reasoning_effort_by_lane


def _db(events, requests=()):
    """events: (seconds, type, payload); requests: (end_seconds, latency_s, agent_id[, input, output])."""
    con = sqlite3.connect(":memory:")
    con.execute("create table events (sequence integer, type text, payload text, created_at text)")
    con.execute("create table model_requests (created_at text, latency_ms integer, agent_id text, "
                "input_tokens integer default 0, output_tokens integer default 0, provider_request_id text)")
    for i, (t, etype, payload) in enumerate(events, start=1):
        con.execute("insert into events values (?,?,?,?)",
                    (i, etype, json.dumps({"type": etype, "payload": payload}), f"2026-09-14T00:{int(t)//60:02d}:{t%60:06.3f}+00:00"))
    for end, latency, agent, *tokens in requests:
        tin, tout = tokens or (0, 0)
        con.execute("insert into model_requests values (?,?,?,?,?,null)",
                    (f"2026-09-14T00:{int(end)//60:02d}:{end%60:06.3f}+00:00", int(latency * 1000), agent,
                     tin, tout))
    return con


def _tool(t, agent, name, path, finished=True, is_error=False):
    call = f"{agent}-{name}-{t}"
    out = [(t, "tool_call_started", {"agent_id": agent, "call_id": call, "name": name,
                                     "arguments": json.dumps({"path": path})})]
    if finished:
        out.append((t + 0.5, "tool_call_finished", {"agent_id": agent, "call_id": call, "name": name,
                                                   "is_error": is_error}))
    return out


class NoDelegationTests(unittest.TestCase):
    def test_a_run_without_children_reports_no_coordination(self):
        con = _db([
            (0, "turn_started", {}),
            *_tool(5, None, "write_file", "a.go"),
            (20, "task_finished", {"outcome": "completed"}),
        ])
        c = coordination(con)
        self.assertEqual(c["wall_s"], 20.0)
        self.assertEqual(c["time_to_first_useful_action_s"], 5.0)
        self.assertIsNone(c["time_to_first_spawn_s"])
        self.assertIsNone(c["coordination_overhead_s"])
        self.assertEqual(c["children"], [])
        self.assertIsNone(c["time_to_first_read_s"])
        self.assertIsNone(c["parent_pre_spawn"])


class DelegatedRunTests(unittest.TestCase):
    def setUp(self):
        self.con = _db(
            [
                (0, "turn_started", {}),
                *_tool(2, None, "read_file", "a.go"),
                (70, "sub_agent_started", {"id": "c1", "role": "worker", "task": "implement p/one"}),
                (70, "sub_agent_started", {"id": "c2", "role": "worker"}),
                *_tool(75, None, "write_file", "own.go"),
                *_tool(80, "c1", "write_file", "p/one.go"),
                *_tool(95, "c2", "write_file", "p/two.go"),
                (98, "sub_agent_finished", {"id": "c1", "outcome": "completed_with_findings", "stop": "completed"}),
                (99, "sub_agent_finished", {"id": "c2", "outcome": "completed_with_findings", "stop": "completed"}),
                *_tool(105, None, "write_file", "p/one.go"),
                (120, "task_finished", {"outcome": "completed"}),
            ],
            # Both children begin work the moment they start (no queue).
            requests=[(3, 1, None, 100, 10), (69, 65, None, 300, 40), (79, 9, "c1"), (94, 24, "c2"),
                      (110, 4, None, 500, 5)],
        )
        self.c = coordination(self.con)

    def test_spawn_timing_and_parent_planning(self):
        self.assertEqual(self.c["time_to_first_spawn_s"], 70.0)
        # The parent's request that ended before the spawn took 65 s.
        self.assertEqual(self.c["parent_planning_s"], 65.0)
        self.assertEqual(self.c["time_to_first_useful_action_s"], 70.0)

    def test_first_read_and_parent_work_before_the_first_spawn(self):
        self.assertEqual(self.c["time_to_first_read_s"], 2.0)
        self.assertEqual(self.c["parent_pre_spawn"], {
            "requests": 2, "input_tokens": 400, "output_tokens": 50, "model_wait_s": 66.0})
        self.assertEqual(self.c["parent_requests"], 3)

    def test_each_child_brief_length_is_reported(self):
        briefs = {c["id"]: c["brief_chars"] for c in self.c["children"]}
        self.assertEqual(briefs, {"c1": len("implement p/one"), "c2": None})

    def test_child_runtime_critical_path_and_parallel_saving(self):
        runtimes = {c["id"]: c["runtime_s"] for c in self.c["children"]}
        self.assertEqual(runtimes, {"c1": 10.5, "c2": 25.5})
        self.assertEqual(self.c["child_critical_path_s"], 25.5)
        self.assertEqual(self.c["serial_child_work_s"], 36.0)
        self.assertEqual(self.c["parallel_time_saved_s"], 10.5)

    def test_settlement_wait_integration_and_overhead(self):
        # Last child work ended at 95.5, last settlement at 99.
        self.assertEqual(self.c["settlement_wait_s"], 3.5)
        self.assertEqual(self.c["parent_integration_s"], 21.0)
        self.assertEqual(self.c["coordination_overhead_s"], 65.0 + 3.5 + 21.0)

    def test_parent_rewrites_of_child_files_after_settlement_are_reported(self):
        self.assertEqual(self.c["parent_rewrites"], {"c1": ["p/one.go"], "c2": []})


class QueuedChildTests(unittest.TestCase):
    def test_time_waiting_for_a_concurrency_slot_is_queue_not_runtime(self):
        con = _db(
            [
                (0, "turn_started", {}),
                (10, "sub_agent_started", {"id": "q", "role": "worker"}),
                (40, "sub_agent_finished", {"id": "q", "outcome": "completed_no_findings", "stop": "completed"}),
                (41, "task_finished", {}),
            ],
            # The child's first request starts at 25 (ended 30 after 5 s), last ends at 38.
            requests=[(9, 2, None), (30, 5, "q"), (38, 6, "q")],
        )
        child = coordination(con)["children"][0]
        self.assertEqual(child["queue_s"], 15.0)
        self.assertEqual(child["runtime_s"], 13.0)


class SafetyTests(unittest.TestCase):
    def test_a_child_write_after_its_terminal_is_counted(self):
        con = _db([
            (0, "turn_started", {}),
            (1, "sub_agent_started", {"id": "c1", "role": "worker"}),
            (5, "sub_agent_finished", {"id": "c1", "outcome": "incomplete_no_result", "stop": "cancelled"}),
            *_tool(6, "c1", "write_file", "late.go"),
            (9, "task_finished", {}),
        ])
        self.assertEqual(coordination(con)["child_write_after_terminal"], 1)

    def test_a_child_started_twice_is_recovery_duplication(self):
        con = _db([
            (0, "turn_started", {}),
            (1, "sub_agent_started", {"id": "c1", "role": "worker"}),
            (2, "sub_agent_interrupted", {"id": "c1"}),
            (3, "sub_agent_resumed", {"id": "c1", "attempt": 1}),
            (4, "sub_agent_started", {"id": "c1", "role": "worker"}),
            (6, "sub_agent_finished", {"id": "c1", "outcome": "completed_no_findings", "stop": "completed"}),
            (7, "task_finished", {}),
        ])
        self.assertEqual(coordination(con)["recovery_duplication"], 1)

    def test_reviewers_are_not_counted_as_delegation_timing(self):
        con = _db([
            (0, "turn_started", {}),
            *_tool(3, None, "write_file", "a.go"),
            (10, "sub_agent_started", {"id": "r", "role": "reviewer"}),
            (15, "sub_agent_finished", {"id": "r", "outcome": "completed_no_findings", "stop": "completed"}),
            (16, "task_finished", {}),
        ])
        c = coordination(con)
        self.assertIsNone(c["time_to_first_spawn_s"])
        self.assertEqual(c["children"], [])


class ReasoningEffortByLaneTests(unittest.TestCase):
    def test_trace_lines_are_joined_to_their_lane_by_request_id(self):
        con = _db([(0, "turn_started", {})])
        con.executemany("insert into model_requests (created_at, latency_ms, agent_id, provider_request_id) "
                        "values ('2026-09-14T00:00:01+00:00', 1, ?, ?)",
                        [(None, "req_p1"), (None, "req_p2"), ("c1", "req_c1")])
        err = (
            "\x1b[2m2026-09-14T00:00:00Z\x1b[0m \x1b[32m INFO\x1b[0m leveler_agent_core::model_round: "
            "model round started request_id=req_p1 messages=3 reasoning_effort=Some(High)\n"
            "2026-09-14T00:00:01Z  INFO leveler_agent_core::model_round: model round started "
            "request_id=req_p2 messages=5 reasoning_effort=Some(High)\n"
            "2026-09-14T00:00:02Z  INFO leveler_agent_core::model_round: model round started "
            "request_id=req_c1 messages=2 reasoning_effort=Some(Max)\n"
            "2026-09-14T00:00:03Z  INFO leveler_agent_core::model_round: model round started "
            "request_id=req_lost messages=2 reasoning_effort=None\n"
            "2026-09-14T00:00:04Z  INFO leveler_agent_core::model_round: model round finished request_id=req_p1\n"
        )
        self.assertEqual(reasoning_effort_by_lane(err, con), {
            "parent": {"High": 2}, "child": {"Max": 1}, "unmatched": {"None": 1}})


if __name__ == "__main__":
    unittest.main()
