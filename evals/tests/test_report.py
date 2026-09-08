from __future__ import annotations

import unittest

from _path import LIB  # noqa: F401

from report import adoption_micro_report, csv_summary, markdown_report
from schema import make_run


def _run(task: str, spawn: bool) -> dict:
    return make_run(
        run_id=f"{task}-1",
        started_at=None,
        git_sha=None,
        binary=None,
        leveler_home=None,
        session_db=None,
        task_id=task,
        suite="adoption",
        max_rounds=20,
        expected_disposition="spawn" if task != "a03-single-file-keep" else "keep",
        arm_name="control",
        arm_factor="baseline",
        arm_value="product_default",
        model_ref="m",
        timeline={
            "valid": True,
            "engaged": True,
            "spawn": spawn,
            "offered": True,
            "disposition": "delegated" if spawn else "kept",
            "spawn_metric": {"natural_spawn_count": int(spawn), "useful_child_count": 0},
        },
    )


class ReportTests(unittest.TestCase):
    def test_markdown_contains_spawn_rate(self):
        runs = [_run("a01-independent-modules", True), _run("a03-single-file-keep", False)]
        md = markdown_report(title="Adoption micro", batches={"control": runs})
        self.assertIn("spawn rate", md)
        self.assertIn("P_over", md)
        self.assertIn("`a01-independent-modules`", md)

    def test_csv_has_header_and_row(self):
        runs = [_run("a01-independent-modules", True)]
        text = csv_summary(runs)
        self.assertIn("natural_spawn_count", text.splitlines()[0])
        self.assertIn("a01-independent-modules", text)

    def test_adoption_micro_report_has_six_sections(self):
        runs = [
            make_run(
                run_id="p-1",
                started_at=None,
                git_sha=None,
                binary=None,
                leveler_home=None,
                session_db=None,
                task_id="a01-independent-modules",
                suite="adoption",
                max_rounds=16,
                expected_disposition="spawn",
                shape="parallel",
                arm_name="control",
                arm_factor="task_shape",
                arm_value="parallel",
                model_ref="m",
                timeline={
                    "valid": True,
                    "engaged": True,
                    "spawn": True,
                    "offered": True,
                    "offer_round": 4,
                    "delegated_round": 8,
                    "disposition": "delegated",
                    "parent_edit_count": 2,
                    "rounds": 10,
                    "spawn_metric": {"natural_spawn_count": 1},
                },
            )
        ]
        md = adoption_micro_report({"batch_id": "demo", "model": "m", "arm": {"name": "control", "factor": "task_shape", "value": "parallel"}, "runs": runs, "notes": "synthetic"})
        for heading in (
            "## 1. Dataset",
            "## 2. Experiment setup",
            "## 3. Metrics",
            "## 4. Results",
            "## 5. Findings",
            "## 6. Next hypothesis",
        ):
            self.assertIn(heading, md)
        self.assertIn("KEEP is a first-class outcome", md)
        self.assertIn("Task shape correlation", md)

    def test_experiment_report_has_required_headings(self):
        from report import experiment_report

        runs = [
            make_run(
                run_id="p-1",
                started_at=None,
                git_sha=None,
                binary=None,
                leveler_home=None,
                session_db=None,
                task_id="a01-independent-modules",
                suite="adoption",
                max_rounds=16,
                expected_disposition="spawn",
                shape="parallel",
                arm_name="control",
                arm_factor="task_shape",
                arm_value="parallel",
                model_ref="m",
                timeline={
                    "valid": True,
                    "engaged": True,
                    "spawn": True,
                    "offered": True,
                    "offer_round": 4,
                    "delegated_round": 8,
                    "disposition": "delegated",
                    "parent_edit_count": 2,
                    "rounds": 10,
                    "spawn_metric": {"natural_spawn_count": 1},
                },
            )
        ]
        md = experiment_report(
            {"batch_id": "demo", "model": "m", "runs": runs},
            experiment={
                "suite": "adoption",
                "experiment": "m3-baseline",
                "model": "m",
                "provider": "p",
                "binary": "leveler",
                "runs": 1,
                "timeout_seconds": 1200,
                "population": "model_initiated_only",
                "exclude": ["safety_probe"],
                "changes_runtime": False,
            },
        )
        for heading in (
            "## Experiment",
            "## Dataset",
            "## Spawn statistics",
            "## Confidence interval",
            "## Verifier results",
            "## Findings",
        ):
            self.assertIn(heading, md)
        self.assertIn("Wilson 90%", md)


if __name__ == "__main__":
    unittest.main()
