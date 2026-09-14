from __future__ import annotations

import json
import sqlite3
import unittest

from _path import LIB  # noqa: F401

from child_lifecycle import child_lifecycle, request_usage
from spawn_metric import fixture_db


def _started(cid, role="explorer", files=None, read_only=None):
    body = {"id": cid, "role": role, "nickname": cid}
    if read_only is not None:
        body["read_only"] = read_only
    if files is not None:
        body["spec"] = {"files": files, "background": True}
    return ("sub_agent_started", body)


def _finished(cid, outcome="completed_with_findings", stop="completed", ok=True):
    return ("sub_agent_finished", {"id": cid, "ok": ok, "outcome": outcome, "stop": stop})


def _tool(agent, call, name, args, is_error=False):
    return [
        ("tool_call_started", {"agent_id": agent, "call_id": call, "name": name,
                               "arguments": json.dumps(args)}),
        ("tool_call_finished", {"agent_id": agent, "call_id": call, "name": name,
                                "is_error": is_error}),
    ]


class TerminalGroupingTests(unittest.TestCase):
    def test_groups_children_by_typed_outcome_and_stop(self):
        con = fixture_db([
            _started("a"), _finished("a"),
            _started("b"), _finished("b", "incomplete_partial", "budget", ok=False),
            _started("c"), _finished("c", "incomplete_no_result", "cancelled", ok=False),
            _started("d"), _finished("d", "incomplete_no_result", "lost", ok=False),
        ])
        lc = child_lifecycle(con)
        self.assertEqual(lc["children"], 4)
        self.assertEqual(lc["by_outcome"], {
            "completed_with_findings": 1, "incomplete_partial": 1, "incomplete_no_result": 2,
        })
        self.assertEqual(lc["by_stop"], {"completed": 1, "budget": 1, "cancelled": 1, "lost": 1})
        self.assertEqual(lc["child_success"], 1)
        self.assertEqual(lc["child_partial"], 1)
        self.assertEqual(lc["lost_accepted_child"], 1)

    def test_an_untyped_terminal_is_counted_as_untyped_not_as_success(self):
        con = fixture_db([_started("a"), ("sub_agent_finished", {"id": "a", "ok": True})])
        lc = child_lifecycle(con)
        self.assertEqual(lc["by_outcome"], {"untyped": 1})
        self.assertEqual(lc["child_success"], 0)
        self.assertEqual(lc["untyped_terminals"], 1)


class LifecycleCounterTests(unittest.TestCase):
    def test_a_second_terminal_is_a_duplicate_settlement_and_only_the_first_counts(self):
        con = fixture_db([_started("a"), _finished("a"), _finished("a", "incomplete_no_result", "failed", ok=False)])
        lc = child_lifecycle(con)
        self.assertEqual(lc["duplicate_settlement"], 1)
        self.assertEqual(lc["by_stop"], {"completed": 1})

    def test_a_started_child_without_terminal_is_an_open_orphan(self):
        con = fixture_db([_started("a"), _started("b"), _finished("b")])
        self.assertEqual(child_lifecycle(con)["open_orphan"], 1)

    def test_interruptions_and_resumes_are_counted(self):
        con = fixture_db([
            _started("a"), ("sub_agent_interrupted", {"id": "a"}),
            ("sub_agent_resumed", {"id": "a", "attempt": 1}), _finished("a"),
        ])
        lc = child_lifecycle(con)
        self.assertEqual((lc["interrupted"], lc["resumed"], lc["open_orphan"]), (1, 1, 0))


class OwnershipTests(unittest.TestCase):
    def test_a_write_inside_the_spawn_scope_is_not_a_violation(self):
        con = fixture_db([
            _started("w", role="worker", files=["pkg/a/NOTES.md"], read_only=False),
            *_tool("w", "c1", "write_file", {"path": "pkg/a/NOTES.md", "content": "x"}),
            _finished("w"),
        ])
        lc = child_lifecycle(con)
        self.assertEqual(lc["ownership_violation"], 0)
        self.assertEqual(lc["child_mutations"], {"w": ["pkg/a/NOTES.md"]})

    def test_a_directory_scope_covers_files_below_it(self):
        con = fixture_db([
            _started("w", role="worker", files=["pkg/a"], read_only=False),
            *_tool("w", "c1", "replace", {"path": "./pkg/a/x.go", "old": "a", "new": "b"}),
        ])
        self.assertEqual(child_lifecycle(con)["ownership_violation"], 0)

    def test_a_successful_write_outside_scope_is_a_violation(self):
        con = fixture_db([
            _started("w", role="worker", files=["pkg/a"], read_only=False),
            *_tool("w", "c1", "write_file", {"path": "pkg/b/x.go", "content": "x"}),
        ])
        lc = child_lifecycle(con)
        self.assertEqual(lc["ownership_violation"], 1)
        self.assertEqual(lc["ownership_violations"], [{"child": "w", "path": "pkg/b/x.go"}])

    def test_a_refused_write_outside_scope_is_not_a_violation(self):
        con = fixture_db([
            _started("w", role="worker", files=["pkg/a"], read_only=False),
            *_tool("w", "c1", "write_file", {"path": "pkg/b/x.go", "content": "x"}, is_error=True),
        ])
        self.assertEqual(child_lifecycle(con)["ownership_violation"], 0)

    def test_a_late_claim_grant_extends_the_scope(self):
        con = fixture_db([
            _started("w", role="worker", read_only=False),
            ("delegation_stage", {"action": "ownership_granted", "detail": "w: pkg/c/y.go, pkg/d"}),
            *_tool("w", "c1", "write_file", {"path": "pkg/d/z.go", "content": "x"}),
        ])
        self.assertEqual(child_lifecycle(con)["ownership_violation"], 0)

    def test_any_successful_write_by_a_read_only_child_is_a_violation(self):
        con = fixture_db([
            _started("e", role="explorer", files=["pkg/a"], read_only=True),
            *_tool("e", "c1", "write_file", {"path": "pkg/a/x.go", "content": "x"}),
        ])
        self.assertEqual(child_lifecycle(con)["ownership_violation"], 1)

    def test_apply_patch_paths_are_read_from_the_patch_headers(self):
        patch = "*** Begin Patch\n*** Update File: pkg/a/x.go\n@@\n-a\n+b\n*** Add File: pkg/b/new.go\n+x\n*** End Patch"
        con = fixture_db([
            _started("w", role="worker", files=["pkg/a"], read_only=False),
            *_tool("w", "c1", "apply_patch", {"patch": patch}),
        ])
        lc = child_lifecycle(con)
        self.assertEqual(lc["ownership_violations"], [{"child": "w", "path": "pkg/b/new.go"}])

    def test_a_mutation_whose_path_cannot_be_read_is_reported_unattributed(self):
        con = fixture_db([
            _started("w", role="worker", files=["pkg/a"], read_only=False),
            *_tool("w", "c1", "write_file", {"content": "x"}),
        ])
        lc = child_lifecycle(con)
        self.assertEqual(lc["ownership_violation"], 0)
        self.assertEqual(lc["unattributed_child_mutations"], 1)


class TruncatedArgumentTests(unittest.TestCase):
    """The durable log caps each string argument, e.g. {"path":"semver/s…"}."""

    def test_a_truncated_path_under_a_granted_prefix_is_unattributed_not_a_violation(self):
        con = fixture_db([
            _started("w", role="worker", read_only=False),
            ("delegation_stage", {"action": "ownership_granted", "detail": "w: semver/semver.go"}),
            *_tool("w", "c1", "write_file", {"path": "semver/s\u2026", "content": "// Packa\u2026"}),
        ])
        lc = child_lifecycle(con)
        self.assertEqual(lc["ownership_violation"], 0)
        self.assertEqual(lc["unattributed_child_mutations"], 1)
        self.assertEqual(lc["suspect_truncated_writes"], [])

    def test_a_truncated_path_matching_no_scope_is_flagged_for_review(self):
        con = fixture_db([
            _started("w", role="worker", read_only=False),
            ("delegation_stage", {"action": "ownership_granted", "detail": "w: semver/semver.go"}),
            *_tool("w", "c1", "write_file", {"path": "cron/c\u2026", "content": "x"}),
        ])
        lc = child_lifecycle(con)
        self.assertEqual(lc["ownership_violation"], 0)
        self.assertEqual(lc["suspect_truncated_writes"], [{"child": "w", "path_prefix": "cron/c"}])


class LegacyLogTests(unittest.TestCase):
    """v0.2.0-beta.2 logs: no spec, scope appended to the task, no typed terminal."""

    def test_a_legacy_scope_marker_in_the_task_is_the_admitted_scope(self):
        con = fixture_db([
            ("sub_agent_started", {"id": "w", "role": "worker", "read_only": False,
                                   "task": "write notes\n[scope: pkg/a/NOTES.md, pkg/b]"}),
            *_tool("w", "c1", "write_file", {"path": "pkg/a/NOTES.md", "content": "x"}),
            *_tool("w", "c2", "write_file", {"path": "pkg/b/y.go", "content": "x"}),
        ])
        lc = child_lifecycle(con)
        self.assertEqual(lc["ownership_violation"], 0)
        self.assertEqual(lc["child_mutations"], {"w": ["pkg/a/NOTES.md", "pkg/b/y.go"]})

    def test_the_same_call_id_in_parent_and_child_is_not_confused(self):
        con = fixture_db([
            _started("w", role="worker", files=["pkg/a"], read_only=False),
            ("tool_call_started", {"agent_id": "w", "call_id": "functions.write:0", "name": "write_file",
                                   "arguments": json.dumps({"path": "pkg/a/x.go", "content": "x"})}),
            ("tool_call_started", {"call_id": "functions.write:0", "name": "write_file",
                                   "arguments": json.dumps({"path": "b.go", "content": "x"})}),
            ("tool_call_finished", {"agent_id": "w", "call_id": "functions.write:0", "name": "write_file",
                                    "is_error": False}),
        ])
        self.assertEqual(child_lifecycle(con)["ownership_violation"], 0)


class ParentUseTests(unittest.TestCase):
    def test_parent_rereads_of_what_a_child_already_read_are_counted(self):
        con = fixture_db([
            _started("e"),
            *_tool("e", "c1", "read_file", {"path": "a.go"}),
            *_tool("e", "c2", "read_file", {"path": "b.go"}),
            _finished("e"),
            ("tool_call_started", {"call_id": "p1", "name": "read_file", "arguments": json.dumps({"path": "a.go"})}),
        ])
        lc = child_lifecycle(con)
        self.assertEqual(lc["child_reads"], {"e": ["a.go", "b.go"]})
        self.assertEqual(lc["parent_rereads"], {"e": ["a.go"]})

    def test_rereads_are_distinct_paths_per_child(self):
        con = fixture_db([
            _started("e"), *_tool("e", "c1", "read_file", {"path": "a.go"}), _finished("e"),
            _started("f"), *_tool("f", "c2", "read_file", {"path": "z.go"}), _finished("f"),
            ("tool_call_started", {"call_id": "p1", "name": "read_file", "arguments": json.dumps({"path": "a.go"})}),
            ("tool_call_started", {"call_id": "p2", "name": "read_file", "arguments": json.dumps({"path": "./a.go"})}),
        ])
        lc = child_lifecycle(con)
        self.assertEqual(lc["parent_rereads"], {"e": ["a.go"], "f": []})


def _requests_db(rows, columns=True):
    con = sqlite3.connect(":memory:")
    if columns:
        con.execute("create table model_requests (input_tokens int, output_tokens int, "
                    "cached_input_tokens int, cost_usd_micros int, agent_id text)")
        con.executemany("insert into model_requests values (?,?,?,?,?)", rows)
    else:
        con.execute("create table model_requests (input_tokens int, output_tokens int)")
        con.executemany("insert into model_requests values (?,?)", rows)
    return con


class RequestUsageTests(unittest.TestCase):
    def test_splits_parent_and_child_lanes_with_cached_tokens_and_cost(self):
        con = _requests_db([(100, 10, 50, 7, None), (200, 20, 150, 9, None), (40, 4, 0, 3, "c1")])
        u = request_usage(con)
        self.assertEqual(u["total"], {"requests": 3, "input_tokens": 340, "output_tokens": 34,
                                      "cached_input_tokens": 200, "cost_usd_micros": 19,
                                      "unpriced_requests": 0})
        self.assertEqual(u["child"]["requests"], 1)
        self.assertEqual(u["parent"]["cost_usd_micros"], 16)

    def test_an_unpriced_request_makes_cost_unknown_not_lower(self):
        con = _requests_db([(100, 10, 0, 7, None), (100, 10, 0, None, None)])
        u = request_usage(con)
        self.assertIsNone(u["total"]["cost_usd_micros"])
        self.assertEqual(u["total"]["unpriced_requests"], 1)

    def test_an_old_table_without_the_columns_reports_unknown(self):
        con = _requests_db([(100, 10)], columns=False)
        u = request_usage(con)
        self.assertEqual(u["total"]["requests"], 1)
        self.assertIsNone(u["total"]["cached_input_tokens"])
        self.assertIsNone(u["total"]["cost_usd_micros"])
        self.assertIsNone(u["child"])


if __name__ == "__main__":
    unittest.main()
