from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from _path import LIB  # noqa: F401

from catalog import load_catalog
from runner import prepare_home, score_home

ROOT = Path(__file__).resolve().parents[1]
CATALOG = ROOT / "micro" / "adoption" / "catalog.json"


def write_db(dest: Path, events) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    import json
    import sqlite3

    con = sqlite3.connect(str(dest), timeout=5.0)
    try:
        con.execute("create table events (sequence integer, type text, payload text)")
        for i, (etype, payload) in enumerate(events, start=1):
            con.execute(
                "insert into events values (?,?,?)",
                (i, etype, json.dumps({"type": etype, "payload": payload})),
            )
        con.commit()
    finally:
        con.close()


class RunnerTests(unittest.TestCase):
    def test_timing_arm_writes_offer_timing_only_in_isolated_home(self):
        with tempfile.TemporaryDirectory() as tmp:
            user = Path(tmp) / "user.toml"
            user.write_text('default_model = "m"\n', encoding="utf-8")
            home = Path(tmp) / "home"
            prepare_home(home, "timing.after_first_edit", user)
            text = (home / "config.toml").read_text(encoding="utf-8")
            self.assertIn('offer_timing = "after_first_edit"', text)
            self.assertNotIn("offer_timing", user.read_text(encoding="utf-8"))

    def test_control_arm_does_not_set_offer_timing(self):
        with tempfile.TemporaryDirectory() as tmp:
            user = Path(tmp) / "user.toml"
            user.write_text('default_model = "m"\n', encoding="utf-8")
            home = Path(tmp) / "home"
            prepare_home(home, "control", user)
            text = (home / "config.toml").read_text(encoding="utf-8")
            self.assertNotIn("offer_timing", text)

    def test_score_home_matches_case_id_from_path(self):
        catalog = load_catalog(CATALOG)
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp) / "home"
            db = (
                home
                / "state"
                / "projects"
                / "leveler-eval-a01-independent-modules-123-exec1-r1"
                / "sessions.db"
            )
            write_db(
                db,
                [
                    ("plan_updated", {"steps": [{"step": "a"}, {"step": "b"}]}),
                    ("delegation_stage", {"action": "offered", "detail": "plan"}),
                    ("sub_agent_started", {"id": "w1", "role": "worker", "nickname": "Ada"}),
                    ("delegation_stage", {"action": "delegated", "detail": "src/normalize.rs"}),
                ],
            )
            batch = score_home(
                home,
                catalog=catalog,
                arm="control",
                model="m",
                git="deadbeef",
                binary="leveler",
                batch_id="t1",
                started_at=None,
            )
            self.assertEqual(len(batch["runs"]), 1)
            run = batch["runs"][0]
            self.assertEqual(run["task"]["id"], "a01-independent-modules")
            self.assertEqual(run["task"]["expected_disposition"], "spawn")
            self.assertTrue(run["metrics"]["spawn"])
            self.assertEqual(run["metrics"]["disposition"], "delegated")


if __name__ == "__main__":
    unittest.main()
