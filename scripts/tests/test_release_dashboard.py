"""Behaviour pins for the HTML release dashboard.

These exist because the dashboard once reported a green "TUI / PTY" card while
the release was BLOCKED on a PTY contract failure: `L2.pty` is a single gate
verdict holding a list of critical gates, not a dict of suites keyed by name, so
iterating it produced one row per metadata field (binary, notes, stages, ...)
and the gate's own status was never read. The feature-coverage card had the same
shape of defect: it read the L3 layer status instead of the coverage verdict.

Run with: python3 -m unittest discover -s scripts/tests
"""

from __future__ import annotations

import importlib.util
import os
import sys
import unittest

_HERE = os.path.dirname(os.path.abspath(__file__))
_MODULE_PATH = os.path.join(os.path.dirname(_HERE), "release_dashboard.py")

_spec = importlib.util.spec_from_file_location("release_dashboard", _MODULE_PATH)
dashboard = importlib.util.module_from_spec(_spec)
sys.modules["release_dashboard"] = dashboard
_spec.loader.exec_module(dashboard)


def record_with(*, pty_status="PASS", pty_gates=None, coverage_status="FAIL", l1_checks=None):
    gates = pty_gates
    if gates is None:
        gates = [
            {"gate": "PTY launch", "verdict": "PASS", "reason": "6/6 launches reached an idle screen"},
            {"gate": "Cancel", "verdict": pty_status, "reason": "cancel settled"},
        ]
    return {
        "state": {"status": "FAIL", "stages": []},
        "dogfood": {
            "gate": {
                "decision": {"decision": "FAIL"},
                "feature_coverage": {
                    "status": coverage_status,
                    "summary": {
                        "core_covered": 99,
                        "core_total": 106,
                        "coverage_core_percent": 93.4,
                    },
                },
                "layers": {
                    "L1": {
                        "status": "FAIL",
                        "checks": l1_checks
                        if l1_checks is not None
                        else [
                            {"id": "workspace_tests", "status": "PASS"},
                            {
                                "id": "runtime_lifecycle",
                                "name": "runtime lifecycle",
                                "status": "FAIL",
                                "failure_label": "RUNTIME_LIFECYCLE_REGRESSION",
                            },
                            {
                                "id": "conversation_presentation_contract",
                                "name": "ConversationPresentationContract",
                                "status": "FAIL",
                                "failure_label": "CONVERSATION_PRESENTATION_INCOMPLETE",
                            },
                        ],
                    },
                    "L2": {
                        "status": "PASS",
                        "pty": {
                            "status": pty_status,
                            "notes": ["stages=T,S,R,C; 9/9 critical gates PASS"],
                            "stages": ["T", "S", "R", "C"],
                            "wall_seconds": 56.7,
                            "binary": {"path": "/tmp/leveler", "sha256": "deadbeef"},
                            "gates": gates,
                        },
                    },
                    "L3": {"status": "NOT_VERIFIED", "cases": []},
                },
            }
        },
    }


def card(record, label):
    return next(row for row in dashboard.summary_cards(record) if row[0] == label)


class PtyRowsTest(unittest.TestCase):
    def test_reads_the_critical_gates_instead_of_the_gate_metadata(self):
        rows = dashboard.pty_rows(record_with())
        self.assertEqual([r["name"] for r in rows], ["PTY launch", "Cancel"])
        for metadata_field in ("binary", "notes", "stages", "wall_seconds", "status"):
            self.assertNotIn(metadata_field, [r["name"] for r in rows])

    def test_a_failing_critical_gate_is_visible_as_fail(self):
        rows = dashboard.pty_rows(record_with(pty_status="FAIL"))
        self.assertEqual([r["status"] for r in rows], ["PASS", "FAIL"])


class SummaryCardsTest(unittest.TestCase):
    def test_tui_card_reports_the_pty_gate_status(self):
        _, status, _ = card(record_with(pty_status="FAIL"), "TUI / PTY")
        self.assertEqual(status, "FAIL")

    def test_tui_card_reports_a_pty_contract_failure(self):
        # The 13-metadata-field bug made this card PASS while the release was
        # BLOCKED on conversation_presentation_contract.
        _, status, _ = card(record_with(pty_status="PASS"), "TUI / PTY")
        self.assertEqual(status, "FAIL")

    def test_feature_coverage_card_reads_the_coverage_verdict(self):
        label, status, detail = card(record_with(), "Feature coverage")
        self.assertEqual(label, "Feature coverage")
        self.assertEqual(status, "FAIL")
        self.assertIn("93.4", detail)


class ContractFailuresTest(unittest.TestCase):
    def test_lists_every_non_passing_contract(self):
        rows = dashboard.contract_failure_rows(record_with())
        self.assertEqual(
            [r["id"] for r in rows],
            ["runtime_lifecycle", "conversation_presentation_contract"],
        )

    def test_a_passing_layer_reports_nothing(self):
        record = record_with(l1_checks=[{"id": "workspace_tests", "status": "PASS"}])
        self.assertEqual(dashboard.contract_failure_rows(record), [])


class RewrittenHistoryTest(unittest.TestCase):
    def test_the_page_states_why_the_change_list_is_a_tree_difference(self):
        # v1.0.21's tag belongs to a history this repository no longer shares.
        # The report must not show that line's commits as this release's work.
        record = record_with()
        record["state"].update(
            {
                "version": "1.0.22",
                "release_id": "v1.0.22",
                "candidate_sha": "a" * 40,
            }
        )
        record["changes"] = {
            "commit_count": 0,
            "changed_files": 12,
            "features": [],
            "fixes": [],
            "other": [],
            "note": "v1.0.21 is not an ancestor of this candidate",
        }
        page = dashboard.render_version(record)
        self.assertIn("not an ancestor", page)
        self.assertEqual(dashboard.change_rows(record)["Features"]["total"], 0)


if __name__ == "__main__":
    unittest.main()
