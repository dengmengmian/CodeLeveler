from __future__ import annotations

import sys
import unittest
from pathlib import Path

from _path import LIB  # noqa: F401

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))

from multi_agent_ab import arm_env, parse_arm, registered_units  # noqa: E402


class RegisteredUnitsTests(unittest.TestCase):
    def test_units_are_the_packages_the_oracle_writes_hidden_tests_into(self):
        case = {"expect": {"program": "bash", "args": ["-c",
            "set -euo pipefail\nmkdir -p \"$(dirname 'expr/zz_hidden_test.go')\"\n"
            "cat > 'expr/zz_hidden_test.go' <<'HIDDEN_EOF'\npackage expr\nHIDDEN_EOF\n"
            "cat > 'jsonpath/zz_hidden_test.go' <<'HIDDEN_EOF'\npackage jsonpath\nHIDDEN_EOF\ngo test ./...\n"]}}
        self.assertEqual(registered_units(case), ["expr", "jsonpath"])


class ParentEffortArmTests(unittest.TestCase):
    def test_an_arm_can_lower_only_the_parent_effort(self):
        arm = parse_arm("r1=/bin/echo:parent=high")
        self.assertFalse(arm["single"])
        self.assertEqual(arm["parent_effort"], "high")
        env = arm_env(arm, Path("/h"))
        self.assertEqual(env["LEVELER_EVAL_PARENT_REASONING_EFFORT"], "high")

    def test_a_plain_arm_sets_no_parent_effort_but_still_traces_it(self):
        arm = parse_arm("r0=/bin/echo")
        self.assertIsNone(arm["parent_effort"])
        env = arm_env(arm, Path("/h"))
        self.assertNotIn("LEVELER_EVAL_PARENT_REASONING_EFFORT", env)
        for e in (env, arm_env(parse_arm("r1=/bin/echo:parent=high"), Path("/h"))):
            self.assertIn("leveler_agent_core::model_round=info", e["RUST_LOG"])
            self.assertEqual(e["LEVELER_HOME"], "/h")

    def test_an_unknown_arm_mode_is_refused(self):
        with self.assertRaises(SystemExit):
            parse_arm("r1=/bin/echo:parent")


if __name__ == "__main__":
    unittest.main()
