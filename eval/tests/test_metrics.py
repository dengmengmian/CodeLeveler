from __future__ import annotations

import unittest

from _path import LIB  # noqa: F401

from metrics import compare_batches, fisher_two_sided, over_delegation, summarize_runs, wilson


def _run(task: str, spawn: bool, expected: str, valid: bool = True) -> dict:
    return {
        "schema_version": "1",
        "run": {"id": f"{task}-{'s' if spawn else 'k'}"},
        "task": {"id": task, "suite": "adoption", "expected_disposition": expected},
        "arm": {"name": "x", "factor": "baseline", "value": "x"},
        "model": {"ref": "m"},
        "delegation": {
            "offered": True,
            "natural_spawn_count": int(spawn),
            "useful_child_count": int(spawn),
            "kept": not spawn,
            "delegated": spawn,
        },
        "edits": {"rounds": 8},
        "verifier": {"ran": False, "passed": None, "command": None},
        "safety": {"ownership_granted": 0, "ownership_denied": 0, "claim_count": 0},
        "metrics": {"valid": valid, "engaged": valid, "spawn": spawn, "disposition": "delegated" if spawn else "kept"},
    }


class StatsTests(unittest.TestCase):
    def test_wilson_bounds(self):
        lo, hi = wilson(0, 10)
        self.assertGreaterEqual(lo, 0.0)
        self.assertLess(hi, 0.3)
        lo, hi = wilson(10, 10)
        self.assertGreater(lo, 0.7)
        self.assertLessEqual(hi, 1.0)

    def test_fisher_identical_is_one(self):
        self.assertAlmostEqual(fisher_two_sided(5, 5, 5, 5), 1.0)

    def test_fisher_extreme_is_small(self):
        p = fisher_two_sided(8, 2, 2, 8)
        self.assertLess(p, 0.05)

    def test_keep_controls_excluded_from_spawn_likely_rate(self):
        runs = [
            _run("a01", True, "spawn"),
            _run("a01", False, "spawn"),
            _run("a03", True, "keep"),
        ]
        s = summarize_runs(runs, spawn_likely_only=True)
        self.assertEqual(s["n_valid"], 2)
        self.assertEqual(s["spawn_n"], 1)
        over = over_delegation(runs)
        self.assertEqual(over["n_valid"], 1)
        self.assertEqual(over["spawn_n"], 1)

    def test_invalid_runs_dropped(self):
        runs = [_run("a01", False, "spawn", valid=False), _run("a01", True, "spawn")]
        s = summarize_runs(runs, spawn_likely_only=True)
        self.assertEqual(s["n_valid"], 1)
        self.assertEqual(s["spawn_rate"], 1.0)

    def test_compare_insufficient_n(self):
        control = [_run("a01", False, "spawn") for _ in range(3)]
        treatment = [_run("a01", True, "spawn") for _ in range(3)]
        cmp_ = compare_batches(control, treatment)
        self.assertEqual(cmp_["verdict"], "insufficient_n")

    def test_compare_treatment_higher(self):
        control = [_run("a01", False, "spawn") for _ in range(10)]
        treatment = [_run("a01", True, "spawn") for _ in range(10)]
        cmp_ = compare_batches(control, treatment)
        self.assertEqual(cmp_["verdict"], "treatment_higher")
        self.assertLess(cmp_["fisher_p"], 0.05)


if __name__ == "__main__":
    unittest.main()
