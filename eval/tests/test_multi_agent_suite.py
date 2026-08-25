"""MA-VALUE-001 suite layout, catalog, and experiment config.

Cases are pointers to Real Usage R005–R010. They are not EvaluationCase
YAML and must not invent a model-visible task.
"""

from __future__ import annotations

import json
import unittest
from pathlib import Path

from _path import LIB  # noqa: F401

from catalog import FORBIDDEN_IN_TASK
from experiment import apply_overrides, load_experiment, load_yaml, resolve_experiment


EVAL = Path(__file__).resolve().parents[1]
SUITE = EVAL / "suites" / "multi_agent"
VALUE = SUITE / "multi_agent_value"
CASES = VALUE / "cases"
CATALOG = CASES / "catalog.json"
EXPERIMENT = EVAL / "configs" / "multi_agent" / "MA-VALUE-001.yaml"
TASK_IDS = ("R005", "R006", "R007", "R008", "R009", "R010")
CASE_FILES = {
    "R005": "r005-cargo.yaml",
    "R006": "r006-casdoor.yaml",
    "R007": "r007-hoppscotch.yaml",
    "R008": "r008-ripgrep.yaml",
    "R009": "r009-go-task.yaml",
    "R010": "r010-frontend.yaml",
}


class MultiAgentSuiteLayoutTests(unittest.TestCase):
    def test_suite_directories_exist(self):
        for part in ("cases", "configs", "reports", "methodology"):
            path = VALUE / part
            self.assertTrue(path.is_dir(), path)

    def test_experiment_resolves_from_framework_path(self):
        path = resolve_experiment(EVAL, "multi_agent", "MA-VALUE-001")
        self.assertEqual(path, EXPERIMENT)
        self.assertTrue(path.is_file())


class CatalogTests(unittest.TestCase):
    def test_catalog_is_exactly_r005_to_r010(self):
        catalog = json.loads(CATALOG.read_text(encoding="utf-8"))
        self.assertEqual(catalog["suite"], "multi_agent")
        self.assertEqual(catalog["experiment"], "MA-VALUE-001")
        self.assertEqual(tuple(catalog["tasks"]), TASK_IDS)
        for tid, filename in CASE_FILES.items():
            entry = catalog["tasks"][tid]
            self.assertEqual(entry["id"], tid)
            self.assertEqual(entry["case_file"], filename)
            self.assertTrue((CASES / filename).is_file())

    def test_pointer_cases_are_not_evaluation_cases(self):
        for filename in CASE_FILES.values():
            text = (CASES / filename).read_text(encoding="utf-8")
            self.assertNotIn("\ntask:", text, filename)
            self.assertNotIn("\nexpect:", text, filename)
            self.assertIn("source: real_usage_batch_01", text)
            hits = [tok for tok in FORBIDDEN_IN_TASK if tok in text.lower()]
            self.assertEqual(hits, [], filename)

    def test_scale_is_six_by_two_by_three(self):
        cfg = load_experiment(EXPERIMENT)
        self.assertEqual(tuple(cfg["tasks"]), TASK_IDS)
        self.assertEqual(cfg["runs"], 3)
        self.assertEqual(len(cfg["tasks"]) * 2 * cfg["runs"], 36)


class ExperimentConfigTests(unittest.TestCase):
    def test_ma_value_001_loads(self):
        cfg = load_experiment(EXPERIMENT)
        self.assertEqual(cfg["suite"], "multi_agent")
        self.assertEqual(cfg["experiment"], "MA-VALUE-001")
        self.assertFalse(cfg["changes_runtime"])
        self.assertFalse(cfg.get("execute"))
        self.assertIn("task_success", cfg["metrics"])
        self.assertNotIn("spawn_rate", cfg["metrics"])
        self.assertNotIn("adoption_rate", cfg["metrics"])

    def test_mode_override_does_not_mutate_file(self):
        cfg = load_experiment(EXPERIMENT)
        over = apply_overrides(cfg, mode="multi")
        self.assertEqual(over["mode"], "multi")
        self.assertNotEqual(cfg.get("mode"), "multi")

    def test_arm_overlays_use_shipped_delegation_key(self):
        single = load_yaml(VALUE / "configs" / "single.yaml")
        multi = load_yaml(VALUE / "configs" / "multi.yaml")
        self.assertEqual(single["mode"], "single_agent")
        self.assertFalse(single["agents"]["delegation"])
        self.assertEqual(multi["mode"], "multi_agent")
        self.assertNotIn("delegation", multi.get("agents") or {})


REVIEWER = SUITE / "reviewer_value"
REVIEWER_CASES = REVIEWER / "cases"
REVIEWER_CATALOG = REVIEWER_CASES / "catalog.json"
REVIEWER_EXPERIMENT = EVAL / "configs" / "multi_agent" / "MA-VALUE-REVIEWER-PILOT.yaml"
REVIEWER_TASKS = (
    "icg-2-bug-fix",
    "rust-race-counter",
    "ts-concurrency-limit",
    "ts-redact-secrets",
    "icg-3-cross-module",
)


class ReviewerPilotSuiteTests(unittest.TestCase):
    def test_layout_exists(self):
        for part in ("cases", "configs", "reports", "methodology"):
            path = REVIEWER / part
            self.assertTrue(path.is_dir(), path)

    def test_experiment_loads(self):
        cfg = load_experiment(REVIEWER_EXPERIMENT)
        self.assertEqual(cfg["experiment"], "MA-VALUE-REVIEWER-PILOT")
        self.assertFalse(cfg["changes_runtime"])
        self.assertFalse(cfg.get("execute"))
        self.assertEqual(cfg["runs"], 1)
        self.assertEqual(tuple(cfg["tasks"]), REVIEWER_TASKS)
        self.assertEqual(len(cfg["tasks"]) * 2 * cfg["runs"], 10)
        self.assertNotIn("spawn_rate", cfg["metrics"])
        self.assertIn("useful_findings", cfg["metrics"])
        self.assertIn("task_success", cfg["metrics"])

    def test_catalog_points_at_vendored_evals(self):
        catalog = json.loads(REVIEWER_CATALOG.read_text(encoding="utf-8"))
        self.assertEqual(tuple(catalog["tasks"]), REVIEWER_TASKS)
        repo = EVAL.parent
        for tid, filename in {
            "icg-2-bug-fix": "icg-2-bug-fix.yaml",
            "rust-race-counter": "rust-race-counter.yaml",
            "ts-concurrency-limit": "ts-concurrency-limit.yaml",
            "ts-redact-secrets": "ts-redact-secrets.yaml",
            "icg-3-cross-module": "icg-3-cross-module.yaml",
        }.items():
            entry = catalog["tasks"][tid]
            self.assertTrue((REVIEWER_CASES / filename).is_file())
            ev = repo / entry["evals_path"]
            self.assertTrue(ev.is_file(), ev)
            text = (REVIEWER_CASES / filename).read_text(encoding="utf-8")
            self.assertNotIn("\ntask:", text)
            self.assertNotIn("\nexpect:", text)

    def test_arm_overlays_use_shipped_independent_review_key(self):
        self_arm = load_yaml(REVIEWER / "configs" / "self.yaml")
        rev_arm = load_yaml(REVIEWER / "configs" / "reviewer.yaml")
        self.assertEqual(self_arm["agents"]["independent_review"], "off")
        self.assertEqual(rev_arm["agents"]["independent_review"], "always")

    def test_mode_self_and_reviewer_do_not_mutate_file(self):
        cfg = load_experiment(REVIEWER_EXPERIMENT)
        over = apply_overrides(cfg, mode="reviewer")
        self.assertEqual(over["mode"], "reviewer")
        self.assertNotEqual(cfg.get("mode"), "reviewer")


if __name__ == "__main__":
    unittest.main()
