from __future__ import annotations

import unittest

from _path import LIB  # noqa: F401

from schema import compact_record, make_batch, make_run, validate_batch, validate_run


class SchemaTests(unittest.TestCase):
    def test_make_run_validates(self):
        run = make_run(
            run_id="r1",
            started_at=None,
            git_sha="abc",
            binary="leveler 0.1.4",
            leveler_home="/tmp/h",
            session_db="/tmp/h/sessions.db",
            task_id="a01",
            suite="adoption",
            max_rounds=20,
            expected_disposition="spawn",
            arm_name="control",
            arm_factor="baseline",
            arm_value="product_default",
            model_ref="deepseek/deepseek-v4-flash",
            timeline={
                "offered": True,
                "offer_trigger": "plan",
                "offer_round": 4,
                "spawn": True,
                "delegated": True,
                "valid": True,
                "engaged": True,
                "disposition": "delegated",
                "spawn_metric": {"natural_spawn_count": 1, "useful_child_count": 1, "parent_tool_calls": 6},
            },
        )
        self.assertEqual(validate_run(run), [])

    def test_missing_section_is_error(self):
        errors = validate_run({"schema_version": "1"})
        self.assertTrue(any("missing" in e for e in errors))

    def test_batch_round_trip(self):
        run = make_run(
            run_id="r1",
            started_at=None,
            git_sha=None,
            binary=None,
            leveler_home=None,
            session_db=None,
            task_id="a01",
            suite="adoption",
            max_rounds=16,
            expected_disposition="keep",
            arm_name="control",
            arm_factor="baseline",
            arm_value="product_default",
            model_ref=None,
            timeline={"valid": True, "engaged": True, "spawn": False, "disposition": "kept"},
        )
        batch = make_batch(batch_id="b1", runs=[run])
        self.assertEqual(validate_batch(batch), [])

    def test_compact_record_has_required_contract_keys(self):
        run = make_run(
            run_id="r1",
            started_at=None,
            git_sha=None,
            binary=None,
            leveler_home=None,
            session_db=None,
            task_id="a01",
            suite="adoption",
            max_rounds=16,
            expected_disposition="spawn",
            arm_name="control",
            arm_factor="baseline",
            arm_value="product_default",
            model_ref="m",
            timeline={"valid": True, "engaged": True, "spawn": False, "disposition": "kept"},
        )
        rec = compact_record(run)
        self.assertEqual(rec["run_id"], "r1")
        self.assertEqual(rec["task"], "a01")
        self.assertEqual(rec["model"], "m")
        self.assertIn("spawn", rec["delegation"])
        self.assertIn("worker_count", rec["delegation"])
        self.assertIn("decision_round", rec["delegation"])
        self.assertIn("turns", rec["execution"])
        self.assertIn("edits", rec["execution"])
        self.assertIn("verifier", rec["execution"])
        self.assertIn("violations", rec["safety"])

    def test_value_fields_are_optional_on_existing_records(self):
        run = make_run(
            run_id="r1",
            started_at=None,
            git_sha=None,
            binary=None,
            leveler_home=None,
            session_db=None,
            task_id="a01",
            suite="adoption",
            max_rounds=16,
            expected_disposition="keep",
            arm_name="control",
            arm_factor="baseline",
            arm_value="product_default",
            model_ref=None,
            timeline={"valid": True, "engaged": True, "spawn": False, "disposition": "kept"},
        )
        self.assertEqual(validate_run(run), [])
        self.assertNotIn("experiment", run)
        self.assertNotIn("mode", run)
        self.assertNotIn("task_success", run)
        self.assertNotIn("multi_agent", run)

    def test_value_fields_round_trip_when_present(self):
        run = make_run(
            run_id="r1",
            started_at=None,
            git_sha=None,
            binary=None,
            leveler_home=None,
            session_db=None,
            task_id="R005",
            suite="multi_agent",
            max_rounds=None,
            expected_disposition=None,
            arm_name="multi",
            arm_factor="mode",
            arm_value="multi_agent",
            model_ref="m",
            timeline={
                "valid": True,
                "engaged": True,
                "spawn": True,
                "disposition": "delegated",
                "rounds": 12,
                "input_tokens": 100,
                "output_tokens": 40,
                "total_tokens": 140,
                "wall_time_ms": 90000,
                "child_result_used": True,
                "spawn_metric": {
                    "natural_spawn_count": 1,
                    "useful_child_count": 0,
                    "parent_tool_calls": 8,
                    "children": [{"id": "e1", "role": "explorer"}],
                },
            },
            verifier_ran=True,
            verifier_passed=True,
            experiment="MA-VALUE-001",
            mode="multi",
        )
        self.assertEqual(validate_run(run), [])
        self.assertEqual(run["experiment"], "MA-VALUE-001")
        self.assertEqual(run["mode"], "multi")
        self.assertTrue(run["task_success"])
        self.assertEqual(run["efficiency"]["turns"], 12)
        self.assertEqual(run["efficiency"]["total_tokens"], 140)
        self.assertTrue(run["multi_agent"]["child_result_used"])


if __name__ == "__main__":
    unittest.main()
