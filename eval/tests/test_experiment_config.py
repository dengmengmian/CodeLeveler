"""Experiment YAML is the source of run parameters — not hardcoded in the runner."""

from __future__ import annotations

import unittest
from pathlib import Path

from _path import LIB  # noqa: F401

from experiment import apply_overrides, load_experiment

EVAL = Path(__file__).resolve().parents[1]
M3 = EVAL / "configs" / "adoption" / "m3-baseline.yaml"
M2 = EVAL / "configs" / "adoption" / "m2-budget.yaml"


class ExperimentConfigTests(unittest.TestCase):
    def test_m3_baseline_loads(self):
        cfg = load_experiment(M3)
        self.assertEqual(cfg["suite"], "adoption")
        self.assertEqual(cfg["experiment"], "m3-baseline")
        self.assertEqual(cfg["provider"], "deepseek")
        self.assertEqual(cfg["model"], "deepseek-v4-flash")
        self.assertEqual(cfg["binary"], "leveler")
        self.assertEqual(cfg["runs"], 1)
        self.assertIsInstance(cfg["timeout_seconds"], int)
        self.assertIn("adoption_rate", cfg["metrics"])
        self.assertEqual(cfg["population"], "model_initiated_only")
        self.assertIn("safety_probe", cfg["exclude"])
        self.assertEqual(cfg["output"], "eval/reports/adoption/m3-baseline")

    def test_m2_budget_is_metadata_not_runtime(self):
        cfg = load_experiment(M2)
        self.assertEqual(cfg["experiment"], "m2-budget")
        self.assertEqual(cfg["suite"], "adoption")
        self.assertFalse(cfg.get("changes_runtime"))

    def test_cli_overrides_do_not_mutate_file(self):
        cfg = load_experiment(M3)
        over = apply_overrides(
            cfg,
            model="other-model",
            provider="other",
            runs=3,
            output="eval/reports/tmp",
        )
        self.assertEqual(over["model"], "other-model")
        self.assertEqual(over["runs"], 3)
        self.assertEqual(cfg["model"], "deepseek-v4-flash")
        self.assertEqual(cfg["runs"], 1)

    def test_missing_required_keys_fail(self):
        raw = EVAL / "tests" / "_bad_experiment.yaml"
        raw.write_text("suite: adoption\n", encoding="utf-8")
        self.addCleanup(raw.unlink)
        with self.assertRaises(ValueError):
            load_experiment(raw)


if __name__ == "__main__":
    unittest.main()
