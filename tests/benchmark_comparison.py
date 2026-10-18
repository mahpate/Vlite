"""Unit tests for the cross-database benchmark; no Docker required."""

import importlib.util
import json
import pathlib
import statistics
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

    def test_published_measurements_match_raw_samples_and_summary(self):
        root = pathlib.Path(__file__).resolve().parents[1] / "benchmarks"
        summary = (root / "RESULTS.md").read_text(encoding="utf-8")
        checksums = set()
        for file, workload in [
            ("rest-exact-10k", "Exact, unfiltered"),
            ("rest-filtered-10k", "Exact, 10% eligible"),
            ("rest-ann-indexed-10k", "HNSW, unfiltered"),
        ]:
            report = json.loads((root / "results" / (file + ".json")).read_text(encoding="utf-8"))
            self.assertEqual(report["status"], "complete")
            self.assertEqual(len(report["runs"]), 6)
            self.assertEqual(pathlib.Path(report["config"]["output"]).name, report["config"]["output"])
            checksums.add(report["dataset_and_queries_sha256"])
            for engine in ["vlite", "qdrant"]:
                runs = [run for run in report["runs"] if run["engine"] == engine]
                self.assertEqual(len(runs), 3)
                for run in runs:
                    self.assertEqual(len(run["query_samples_ms"]), 100)
                    self.assertEqual(len(run["query_recall_samples"]), 100)
                    self.assertEqual(statistics.mean(run["query_recall_samples"]),
                                     run["mean_tie_aware_recall_at_k"])
                    for field, fraction in [("p50", .5), ("p95", .95), ("p99", .99)]:
                        self.assertEqual(compare.percentile(run["query_samples_ms"], fraction),
                                         run["latency_ms"][field])
                label = "VLite" if engine == "vlite" else "Qdrant 1.14.1"
                values = [statistics.median(run["latency_ms"][field] for run in runs)
                          for field in ["p50", "p95", "p99"]]
                row = f"| {workload} | {label} | " + " | ".join(f"{value:.4f}" for value in values) + " |"
                self.assertIn(row, summary)
        self.assertEqual(len(checksums), 1)


if __name__ == "__main__":
    unittest.main()
