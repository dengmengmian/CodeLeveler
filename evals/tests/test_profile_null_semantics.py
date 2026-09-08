"""Profile effectiveness must not count an unmeasured child as a zero.

Same defect class as the reviewer observer: a child whose terminal event
carried `contribution: null` was never measured. Summing it as 0 into
`findings_generated` while still counting it in `spawned` dilutes every
per-spawn rate with runs that were never observed.

Ghost children are real: `turn.rs` emits `contribution: None` for a child
that never reported before the turn ended.
"""

from __future__ import annotations

import unittest

from _path import LIB  # noqa: F401

from value import profile_effectiveness


def _spawn(children, outcomes):
    return {"children": children, "sub_agent_outcomes": outcomes}


class ProfileNullSemantics(unittest.TestCase):
    def test_unmeasured_child_is_not_a_measured_zero(self):
        spawn = _spawn(
            [{"id": "a1", "role": "explorer", "profile_id": "explorer"}],
            {"a1": {"ok": True, "contribution": None}},
        )
        b = profile_effectiveness(spawn)["explorer"]
        self.assertEqual(b["spawned"], 1)
        self.assertEqual(
            b["unmeasured"], 1, "an unmeasured child must be counted as such"
        )
        self.assertEqual(
            b["measured"], 0, "nothing about its contribution was observed"
        )

    def test_measured_empty_projection_is_a_real_zero(self):
        spawn = _spawn(
            [{"id": "a1", "role": "explorer", "profile_id": "explorer"}],
            {"a1": {"ok": True, "contribution": {"findings_total": 0}}},
        )
        b = profile_effectiveness(spawn)["explorer"]
        self.assertEqual(b["measured"], 1)
        self.assertEqual(b["unmeasured"], 0)
        self.assertEqual(b["findings_generated"], 0)

    def test_counts_are_unchanged_for_measured_children(self):
        spawn = _spawn(
            [{"id": "a1", "role": "explorer", "profile_id": "explorer"}],
            {
                "a1": {
                    "ok": True,
                    "contribution": {
                        "findings_total": 7,
                        "findings_accepted": 5,
                        "findings_verified": 2,
                    },
                }
            },
        )
        b = profile_effectiveness(spawn)["explorer"]
        self.assertEqual(b["findings_generated"], 7)
        self.assertEqual(b["findings_accepted"], 5)
        self.assertEqual(b["findings_verified"], 2)
        self.assertEqual(b["measured"], 1)

    def test_a_ghost_child_does_not_dilute_a_real_one(self):
        spawn = _spawn(
            [
                {"id": "a1", "role": "explorer", "profile_id": "explorer"},
                {"id": "a2", "role": "explorer", "profile_id": "explorer"},
            ],
            {
                "a1": {"ok": True, "contribution": {"findings_total": 4}},
                "a2": {"ok": False, "contribution": None},
            },
        )
        b = profile_effectiveness(spawn)["explorer"]
        self.assertEqual(b["spawned"], 2)
        self.assertEqual(b["measured"], 1)
        self.assertEqual(b["unmeasured"], 1)
        self.assertEqual(b["findings_generated"], 4)


if __name__ == "__main__":
    unittest.main()


class UnmeasuredDoesNotEraseOtherMetrics(unittest.TestCase):
    """`changes_accepted` and `verification_passed` do not read the projection.

    They come from `useful_child_ids` and the terminal `ok`. An absent
    contribution must not erase facts that were measured by other means.
    """

    def test_worker_without_a_projection_still_counts_its_accepted_change(self):
        spawn = {
            "children": [{"id": "w1", "role": "worker", "profile_id": "worker"}],
            "sub_agent_outcomes": {"w1": {"ok": True, "contribution": None}},
            "useful_child_ids": ["w1"],
        }
        b = profile_effectiveness(spawn)["worker"]
        self.assertEqual(b["unmeasured"], 1)
        self.assertEqual(
            b["changes_accepted"], 1, "the parent used this child's change"
        )
        self.assertEqual(b["verification_passed"], 1)

    def test_completed_is_counted_without_a_projection(self):
        spawn = {
            "children": [{"id": "a1", "role": "explorer", "profile_id": "explorer"}],
            "sub_agent_outcomes": {"a1": {"ok": True, "contribution": None}},
        }
        b = profile_effectiveness(spawn)["explorer"]
        self.assertEqual(b["completed"], 1, "the terminal ok is a measured fact")
