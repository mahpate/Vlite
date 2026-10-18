"""Unit tests for the cross-database benchmark; no Docker required."""

import importlib.util
import pathlib
import sys
import types
import unittest

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "compare_rest", pathlib.Path(__file__).resolve().parents[1] / "benchmarks" / "compare_rest.py"
)
compare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(compare)


class ComparisonTests(unittest.TestCase):
    def test_generator_is_reproducible_and_continuation_is_identical(self):
        all_rows, state = compare.dataset(10, 4, 42)
        first, middle = compare.dataset(3, 4, 42)
        second, end = compare.dataset(7, 4, middle)
        self.assertEqual(all_rows, first + second)
        self.assertEqual(state, end)
        self.assertNotEqual(all_rows, compare.dataset(10, 4, 43)[0])

    def test_percentiles_are_nearest_rank(self):
        values = list(range(100, 0, -1))
        self.assertEqual(compare.percentile(values, .5), 50)
        self.assertEqual(compare.percentile(values, .95), 95)
        self.assertEqual(compare.percentile(values, .99), 99)

    def test_oracle_accepts_ties_and_rejects_invalid_results(self):
        data = [[0.0], [1.0], [-1.0], [2.0]]
        expected = [(0, 0.0), (1, 1.0)]
        config = types.SimpleNamespace(k=2, filter_every=1, mode="exact")
        self.assertEqual(compare.check_hits([(0, 0), (2, 1)], expected, [0], data, config), 1)
        for hits in [
            [(0, 0), (0, 0)], [(0, 0)], [(0, 0), (3, 4)],
            [(0, 0), (4, 1)], [(0, 0), (1, 0)], [(1, 1), (0, 0)],
            [(0, 0), (1, float("nan"))],
        ]:
            with self.assertRaises(AssertionError):
                compare.check_hits(hits, expected, [0], data, config)
        config.filter_every = 2
        with self.assertRaises(AssertionError):
            compare.check_hits([(0, 0), (1, 1)], expected, [0], data, config)
        config.mode = "ann"
        config.filter_every = 1
        self.assertEqual(compare.check_hits([(3, 4)], expected, [0], data, config), 0)


if __name__ == "__main__":
    unittest.main()
