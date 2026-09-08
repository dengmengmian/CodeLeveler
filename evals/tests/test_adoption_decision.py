"""Decision-benchmark metrics: adoption, latency, shape, value. KEEP is not failure."""

from __future__ import annotations

import unittest

from _path import LIB  # noqa: F401

from metrics import adoption_summary, decision_latency_mean, shape_correlation, value_by_disposition
from schema import compact_record, make_run


def _run(
    *,
    task: str,
    shape: str,
    spawn: bool,
    offered: bool = True,
    offer_round: int | None = 4,
    kept_round: int | None = None,
    delegated_round: int | None = None,
    rounds: int = 12,
    edits: int = 3,
    valid: bool = True,
    expected: str = "spawn",
) -> dict:
    if spawn:
        delegated_round = delegated_round if delegated_round is not None else 7
        kept_round = None
        disposition = "delegated"
    else:
        kept_round = kept_round if kept_round is not None else 5
        disposition = "kept" if offered else "none"
    return make_run(
        run_id=f"{task}-{'s' if spawn else 'k'}",
        started_at=None,
        git_sha=None,
        binary=None,
        leveler_home=None,
        session_db=None,
        task_id=task,
        suite="adoption",
        max_rounds=16,
        expected_disposition=expected,
        shape=shape,
        arm_name="control",
        arm_factor="task_shape",
        arm_value=shape,
        model_ref="m",
        timeline={
            "valid": valid,
            "engaged": valid,
            "spawn": spawn,
            "offered": offered,
            "offer_round": offer_round if offered else None,
            "kept": not spawn and offered,
            "kept_round": kept_round,
            "delegated": spawn,
            "delegated_round": delegated_round,
            "disposition": disposition,
            "parent_edit_count": edits,
            "rounds": rounds,
            "spawn_metric": {"natural_spawn_count": int(spawn), "parent_tool_calls": 8},
        },
    )


class AdoptionDecisionTests(unittest.TestCase):
    def test_adoption_rate_is_spawn_given_offer_not_raw_spawn(self):
        runs = [
            _run(task="p", shape="parallel", spawn=True, offered=True),
            _run(task="p", shape="parallel", spawn=False, offered=True),
            _run(task="p", shape="parallel", spawn=False, offered=False, expected="spawn"),
        ]
        s = adoption_summary(runs)
        self.assertEqual(s["n_offer_seen"], 2)
        self.assertEqual(s["spawn_given_offer"], 1)
        self.assertEqual(s["adoption_rate"], 0.5)
        self.assertEqual(s["keep_given_offer"], 1)
        # the never-offered run is not a KEEP failure
        self.assertNotIn("failure", s)

    def test_keep_after_offer_is_not_a_failure(self):
        runs = [_run(task="s", shape="single", spawn=False, expected="keep") for _ in range(6)]
        s = adoption_summary(runs)
        self.assertEqual(s["adoption_rate"], 0.0)
        self.assertEqual(s["keep_given_offer"], 6)
        self.assertTrue(s["keep_is_first_class"])

    def test_decision_latency_is_decision_minus_offer(self):
        spawn = _run(task="p", shape="parallel", spawn=True, offer_round=4, delegated_round=10)
        keep = _run(task="s", shape="single", spawn=False, offer_round=4, kept_round=5)
        self.assertEqual(spawn["metrics"]["decision_latency_rounds"], 6)
        self.assertEqual(keep["metrics"]["decision_latency_rounds"], 1)
        self.assertEqual(decision_latency_mean([spawn, keep]), 3.5)

    def test_shape_correlation_splits_parallel_boundary_single(self):
        runs = (
            [_run(task="p", shape="parallel", spawn=True) for _ in range(4)]
            + [_run(task="b", shape="boundary", spawn=True, expected="either") for _ in range(2)]
            + [_run(task="b", shape="boundary", spawn=False, expected="either") for _ in range(2)]
            + [_run(task="s", shape="single", spawn=False, expected="keep") for _ in range(4)]
        )
        table = shape_correlation(runs)
        self.assertEqual(table["parallel"]["spawn_n"], 4)
        self.assertEqual(table["parallel"]["adoption_rate"], 1.0)
        self.assertEqual(table["boundary"]["adoption_rate"], 0.5)
        self.assertEqual(table["single"]["adoption_rate"], 0.0)
        self.assertEqual(table["single"]["keep_given_offer"], 4)

    def test_value_compares_turns_spawn_vs_keep_within_shape(self):
        runs = [
            _run(task="p", shape="parallel", spawn=True, rounds=10, edits=2),
            _run(task="p", shape="parallel", spawn=True, rounds=12, edits=2),
            _run(task="p", shape="parallel", spawn=False, rounds=18, edits=6),
            _run(task="p", shape="parallel", spawn=False, rounds=20, edits=8),
        ]
        v = value_by_disposition(runs, shape="parallel")
        self.assertEqual(v["spawn"]["mean_turns"], 11.0)
        self.assertEqual(v["keep"]["mean_turns"], 19.0)
        self.assertEqual(v["spawn"]["mean_edits"], 2.0)
        self.assertLess(v["spawn"]["mean_turns"], v["keep"]["mean_turns"])

    def test_compact_record_matches_decision_schema(self):
        run = _run(task="a01-independent-modules", shape="parallel", spawn=True)
        rec = compact_record(run)
        self.assertEqual(
            set(rec),
            {"run_id", "task", "shape", "model", "offer_seen", "delegation", "execution", "safety"},
        )
        self.assertTrue(rec["offer_seen"])
        self.assertTrue(rec["delegation"]["spawn"])
        self.assertEqual(rec["delegation"]["worker_count"], 1)
        self.assertEqual(rec["execution"]["turns"], 12)
        self.assertIn("violations", rec["safety"])


if __name__ == "__main__":
    unittest.main()
