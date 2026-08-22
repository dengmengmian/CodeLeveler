from __future__ import annotations

import json
import unittest
from pathlib import Path

from _path import LIB  # noqa: F401

from catalog import FORBIDDEN_IN_TASK, load_catalog

ROOT = Path(__file__).resolve().parents[1]
TASKS = ROOT / "micro" / "adoption" / "tasks"
CATALOG = ROOT / "micro" / "adoption" / "catalog.json"
REQUIRED = ("id:", "name:", "task:", "expect:", "max_rounds:")


def top_id(text: str) -> str:
    for line in text.splitlines():
        if line.startswith("id:"):
            return line.split(":", 1)[1].strip()
    raise AssertionError("missing id")


class TaskSuiteTests(unittest.TestCase):
    def yaml_files(self) -> list[Path]:
        files = sorted(TASKS.glob("*.yaml"))
        self.assertEqual(len(files), 15)
        return files

    def test_count_in_range(self):
        self.yaml_files()

    def test_required_keys_and_bounded_rounds(self):
        for path in self.yaml_files():
            text = path.read_text(encoding="utf-8")
            for key in REQUIRED:
                self.assertIn(key, text, path.name)
            self.assertRegex(text, r"max_rounds:\s*(1[2-9]|20)\b", path.name)
            self.assertIn('program: "true"', text)

    def test_prompt_has_no_delegation_vocabulary(self):
        for path in self.yaml_files():
            text = path.read_text(encoding="utf-8").lower()
            hits = [tok for tok in FORBIDDEN_IN_TASK if tok in text]
            self.assertEqual(hits, [], path.name)

    def test_catalog_covers_every_task_and_stays_out_of_tasks_dir(self):
        catalog = load_catalog(CATALOG)
        yaml_ids = {top_id(p.read_text(encoding="utf-8")) for p in self.yaml_files()}
        cat_ids = set(catalog["tasks"])
        self.assertEqual(yaml_ids, cat_ids)
        shapes = {"parallel": 0, "boundary": 0, "single": 0}
        for entry in catalog["tasks"].values():
            self.assertIn(entry["expected_disposition"], ("spawn", "keep", "either"))
            self.assertIn(entry["shape"], shapes)
            shapes[entry["shape"]] += 1
        self.assertEqual(shapes, {"parallel": 5, "boundary": 5, "single": 5})
        self.assertFalse((TASKS / "catalog.json").exists())
        # catalog is JSON, not loaded by EvaluationCase::load_dir
        self.assertEqual(CATALOG.suffix, ".json")

    def test_catalog_is_valid_json(self):
        json.loads(CATALOG.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
