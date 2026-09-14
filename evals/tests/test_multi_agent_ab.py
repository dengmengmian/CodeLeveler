from __future__ import annotations

import sys
import unittest
from pathlib import Path

from _path import LIB  # noqa: F401

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))

from multi_agent_ab import registered_units  # noqa: E402


class RegisteredUnitsTests(unittest.TestCase):
    def test_units_are_the_packages_the_oracle_writes_hidden_tests_into(self):
        case = {"expect": {"program": "bash", "args": ["-c",
            "set -euo pipefail\nmkdir -p \"$(dirname 'expr/zz_hidden_test.go')\"\n"
            "cat > 'expr/zz_hidden_test.go' <<'HIDDEN_EOF'\npackage expr\nHIDDEN_EOF\n"
            "cat > 'jsonpath/zz_hidden_test.go' <<'HIDDEN_EOF'\npackage jsonpath\nHIDDEN_EOF\ngo test ./...\n"]}}
        self.assertEqual(registered_units(case), ["expr", "jsonpath"])


if __name__ == "__main__":
    unittest.main()
