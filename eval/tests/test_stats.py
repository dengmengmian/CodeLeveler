from __future__ import annotations

import unittest

from _path import LIB  # noqa: F401

from metrics import describe, mean, median, variance, wilson


class StatsTests(unittest.TestCase):
    def test_mean_median_variance(self):
        xs = [1.0, 2.0, 3.0, 4.0]
        self.assertEqual(mean(xs), 2.5)
        self.assertEqual(median(xs), 2.5)
        self.assertEqual(median([1.0, 2.0, 3.0]), 2.0)
        self.assertAlmostEqual(variance(xs), 5 / 3)

    def test_empty_is_none_not_zero(self):
        self.assertIsNone(mean([]))
        self.assertIsNone(median([]))
        self.assertIsNone(variance([]))
        d = describe([])
        self.assertEqual(d["n"], 0)
        self.assertIsNone(d["mean"])

    def test_describe_sample_size(self):
        d = describe([10.0, 20.0, 30.0])
        self.assertEqual(d["n"], 3)
        self.assertEqual(d["mean"], 20.0)
        self.assertEqual(d["median"], 20.0)
        self.assertEqual(d["min"], 10.0)
        self.assertEqual(d["max"], 30.0)

    def test_wilson_still_present(self):
        lo, hi = wilson(3, 10)
        self.assertGreaterEqual(lo, 0.0)
        self.assertLessEqual(hi, 1.0)


if __name__ == "__main__":
    unittest.main()
