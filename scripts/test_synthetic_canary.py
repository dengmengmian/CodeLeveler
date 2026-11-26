#!/usr/bin/env python3
"""Reverse tests for the synthetic-canary contract.

A security test that accepts a value from the environment, a session record or
an earlier run has no security value: it would send someone's real credential
to a provider. These tests pin the refusal, not just the happy path.
"""

import os
import unittest

from synthetic_canary import SyntheticCanaryRun, UntrustedCanary


def historical_session_value() -> str:
    # Stands in for a value read from a Session row or Memory. It is not a real
    # credential and is refused for the same reason a real one would be: no
    # provenance from this run.
    return "HISTORICAL-SESSION-VALUE-0123456789abcdef"


class SyntheticCanaryContract(unittest.TestCase):
    def test_an_issued_value_is_accepted(self):
        run = SyntheticCanaryRun("unit")
        value = run.issue("seed")
        self.assertTrue(run.accepts(value))
        self.assertEqual(run.require(value), value)
        self.assertEqual(run.label_of(value), "seed")

    def test_a_value_from_the_environment_is_refused(self):
        run = SyntheticCanaryRun("unit")
        env_value = next((v for v in os.environ.values() if v), "")
        self.assertFalse(run.accepts(env_value))
        with self.assertRaises(UntrustedCanary):
            run.require(env_value)

    def test_a_historical_session_value_is_refused(self):
        run = SyntheticCanaryRun("unit")
        value = historical_session_value()
        self.assertFalse(run.accepts(value))
        with self.assertRaises(UntrustedCanary):
            run.require(value)

    def test_another_runs_value_is_refused(self):
        mine = SyntheticCanaryRun("unit")
        theirs = SyntheticCanaryRun("unit")
        value = theirs.issue("seed")
        self.assertFalse(mine.accepts(value))
        with self.assertRaises(UntrustedCanary):
            mine.scrub(value, value, "<SYNTHETIC-SEED>")

    def test_a_forged_prefix_is_refused(self):
        run = SyntheticCanaryRun("unit")
        value = run.issue("seed")
        forged = value.replace(run.run_id, "deadbeef", 1)
        self.assertFalse(run.accepts(forged))

    def test_a_shaped_value_is_accepted_only_when_this_run_issued_it(self):
        run = SyntheticCanaryRun("unit")
        issued = run.issue("seed", shape="sk")
        looking_alike = "sk-" + "A" * 40
        self.assertTrue(issued.startswith("sk-"))
        self.assertTrue(run.accepts(issued))
        self.assertFalse(run.accepts(looking_alike))

    def test_scripts_do_not_freeze_canary_literals(self):
        import pathlib
        import re as _re
        root = pathlib.Path(__file__).resolve().parent
        literal = _re.compile(r"CANARY[A-Z_]*\s*=\s*[\"'][^\"']+[\"']")
        offenders = []
        for path in root.glob("*.py"):
            if path.name in {"synthetic_canary.py", "test_synthetic_canary.py"}:
                continue
            for number, line in enumerate(path.read_text(errors="ignore").splitlines(), 1):
                if literal.search(line):
                    offenders.append(f"{path.name}:{number}")
        self.assertEqual(offenders, [], "security tests must issue canaries per run")

    def test_reports_carry_digests_not_values(self):
        run = SyntheticCanaryRun("unit")
        value = run.issue("seed")
        manifest = run.manifest()
        serialized = repr(manifest)
        self.assertNotIn(value, serialized)
        self.assertIn(run.digest(value), serialized)
        self.assertEqual(manifest["source"], "synthetic_canary.py:secrets")

    def test_scrub_removes_the_value_from_evidence(self):
        run = SyntheticCanaryRun("unit")
        value = run.issue("steer")
        scrubbed = run.scrub(f"body: {value}", value, "<SYNTHETIC-STEER>")
        self.assertNotIn(value, scrubbed)
        self.assertIn("<SYNTHETIC-STEER>", scrubbed)


if __name__ == "__main__":
    unittest.main(verbosity=2)
