"""`contribution: null` means NOT MEASURED. It must never read as zero.

The independent-review stage runs outside the executor ledger, so the runtime
emits `contribution: None` on purpose (leveler-engine/src/turn.rs). Collapsing
that to zeros makes the report claim "zero-finding reviewers", which is a
fabricated measurement: those reviewers did report findings.
"""

from __future__ import annotations

import unittest

from _path import LIB  # noqa: F401

from reviewer import extract_reviewer, reviewer_summary


def _spawn(contribution, ok=True):
    return {
        "children": [{"id": "r1", "role": "reviewer"}],
        "sub_agent_outcomes": {"r1": {"ok": ok, "contribution": contribution}},
    }


class ContributionUnmeasured(unittest.TestCase):
    def test_null_contribution_is_unmeasured_not_zero_findings(self):
        r = extract_reviewer(_spawn(None))
        self.assertEqual(r["reviewer_spawned"], 1)
        self.assertFalse(
            r["zero_findings"],
            "an unmeasured reviewer is not a zero-finding reviewer",
        )
        self.assertTrue(r["contribution_unmeasured"])
        self.assertIsNone(r["findings_generated"])

    def test_present_but_empty_projection_is_a_real_zero(self):
        r = extract_reviewer(
            _spawn({"child_id": "r1", "role": "reviewer", "findings_total": 0})
        )
        self.assertTrue(r["zero_findings"])
        self.assertFalse(r["contribution_unmeasured"])
        self.assertEqual(r["findings_generated"], 0)

    def test_unmeasured_is_not_noise_either(self):
        r = extract_reviewer(_spawn(None))
        self.assertFalse(r["noise"], "unmeasured is not a noise regression")

    def test_summary_reports_unmeasured_count(self):
        runs = [{"reviewer": extract_reviewer(_spawn(None)), "task_success": True}] * 3
        s = reviewer_summary(runs)
        self.assertEqual(s["contribution_unmeasured_n"], 3)
        self.assertEqual(s["zero_findings_n"], 0)


if __name__ == "__main__":
    unittest.main()


class ContributionSourceTrace(unittest.TestCase):
    """Phase 1 stamps which mechanism produced a contribution."""

    def test_independent_reviewer_source_is_reported(self):
        r = extract_reviewer(
            _spawn(
                {
                    "child_id": "reviewer-7f8c",
                    "role": "reviewer",
                    "findings_total": 2,
                    "findings_accepted": 1,
                    "source": {"kind": "independent_reviewer", "review_id": "reviewer-7f8c"},
                }
            )
        )
        self.assertEqual(r["contribution_sources"], ["independent_reviewer"])
        self.assertEqual(r["findings_generated"], 2)

    def test_missing_source_is_unknown_not_guessed(self):
        r = extract_reviewer(
            _spawn({"child_id": "r1", "role": "reviewer", "findings_total": 1})
        )
        self.assertEqual(r["contribution_sources"], ["unknown"])


class NullVersusEmptySemantics(unittest.TestCase):
    """`None` is "not measured". `{}` is "measured, contributed nothing".

    Collapsing the two is the defect that made the pilot report five
    zero-finding reviewers that had all reported. Locked literally.
    """

    def test_none_is_not_measured(self):
        r = extract_reviewer(_spawn(None))
        self.assertTrue(r["contribution_unmeasured"])
        self.assertIsNone(r["findings_generated"])
        self.assertFalse(r["zero_findings"])

    def test_empty_dict_is_measured_with_no_contribution(self):
        r = extract_reviewer(_spawn({}))
        self.assertFalse(r["contribution_unmeasured"])
        self.assertEqual(r["findings_generated"], 0)
        self.assertTrue(r["zero_findings"])

    def test_missing_key_is_not_measured(self):
        spawn = {
            "children": [{"id": "r1", "role": "reviewer"}],
            "sub_agent_outcomes": {"r1": {"ok": True}},
        }
        self.assertTrue(extract_reviewer(spawn)["contribution_unmeasured"])

    def test_non_dict_junk_is_not_measured_not_a_crash(self):
        for junk in (0, "", [], False, "null"):
            with self.subTest(junk=junk):
                r = extract_reviewer(_spawn(junk))
                self.assertTrue(r["contribution_unmeasured"])

    def test_mixed_children_count_only_the_measured_one(self):
        spawn = {
            "children": [
                {"id": "r1", "role": "reviewer"},
                {"id": "r2", "role": "reviewer"},
            ],
            "sub_agent_outcomes": {
                "r1": {"ok": True, "contribution": None},
                "r2": {"ok": True, "contribution": {"findings_total": 2, "findings_accepted": 1}},
            },
        }
        r = extract_reviewer(spawn)
        self.assertFalse(
            r["contribution_unmeasured"],
            "one measured child means the run is not wholly unmeasured",
        )
        self.assertEqual(r["findings_generated"], 2, "the unmeasured child adds nothing")
        self.assertEqual(r["useful_findings"], 1)
