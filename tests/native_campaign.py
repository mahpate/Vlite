"""Campaign aggregation and CLI safety tests; no benchmark build or Docker needed."""

import importlib.util
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
ROOT = pathlib.Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("campaign", ROOT / "benchmarks" / "native_campaign.py")
campaign = importlib.util.module_from_spec(spec)
spec.loader.exec_module(campaign)
metric_spec = importlib.util.spec_from_file_location("metric_campaign", ROOT / "benchmarks" / "metric_campaign.py")
metric_campaign = importlib.util.module_from_spec(metric_spec)
metric_spec.loader.exec_module(metric_campaign)


class CampaignTests(unittest.TestCase):
    def test_metric_campaign_invalid_bounds_and_child_failures_are_explicit(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "metrics.json"
            command = [sys.executable, str(ROOT / "benchmarks" / "metric_campaign.py"),
                       "--binary", sys.executable, "--output", str(output)]
            for flags in [["--trials", "0"], ["--records", "1"], ["--queries", "0"],
                          ["--dimensions", "128", "128"]]:
                self.assertNotEqual(subprocess.run(command + flags, capture_output=True).returncode, 0)
                self.assertFalse(output.exists())
            self.assertNotEqual(subprocess.run(command + ["--trials", "1", "--dimensions", "8"],
                                              capture_output=True).returncode, 0)
            report = json.loads(output.read_text())
            self.assertEqual(report["status"], "failed")
            self.assertEqual(report["runs"], [])
            original = output.read_bytes()
            self.assertNotEqual(subprocess.run(command, capture_output=True).returncode, 0)
            self.assertEqual(output.read_bytes(), original)

    def test_metric_reports_match_raw_trials_and_keep_recall_after_norm_caching(self):
        reports = []
        for name in ["metric-cosine-windows.json", "metric-cosine-cached-windows.json"]:
            report = json.loads((ROOT / "benchmarks" / "results" / name).read_text())
            reports.append(report)
            self.assertEqual(report["status"], "complete")
            self.assertEqual(len(report["runs"]), 54)
            self.assertEqual(report["summary"], metric_campaign.summarize(report["runs"]))
            for run in report["runs"]:
                value = run["measurement"]
                self.assertEqual(value["recall_at_k"], 1)
                self.assertEqual(value["profile"], "release")
                self.assertGreater(value["process_peak_rss_bytes"], 0)
                self.assertEqual(len(value["query_samples_ms"]), 30)
                if "hnsw" in value:
                    self.assertGreaterEqual(value["hnsw"]["recall_at_k"], 0)
                    self.assertLessEqual(value["hnsw"]["recall_at_k"], 1)
                    self.assertEqual(len(value["hnsw"]["query_samples_ms"]), 30)
        before = {(r["dimensions"], r["scenario"], r["trial"]): r["measurement"]
                  for r in reports[0]["runs"]}
        for run in reports[1]["runs"]:
            original = before[(run["dimensions"], run["scenario"], run["trial"])]
            value = run["measurement"]
            self.assertEqual(value["dataset_checksum"], original["dataset_checksum"])
            self.assertEqual(value["query_checksum"], original["query_checksum"])
            if "hnsw" in value:
                self.assertEqual(value["hnsw"]["recall_at_k"], original["hnsw"]["recall_at_k"])

    def test_recall_target_requires_every_trial_and_medians_are_not_pooled(self):
        runs = []
        for latency, recall in [(1, 1), (3, .98), (20, 1)]:
            runs.append({
                "mixed_writes_per_query": 0, "ef_search": 32,
                "measurement": {
                    "hnsw": {"recall_at_k": recall, "query_p50_ms": latency,
                             "query_p95_ms": latency + 1, "query_p99_ms": latency + 2,
                             "index_build_ms": 100},
                    "query_p50_ms": 4, "process_peak_rss_bytes": 1024,
                    "file_bytes": 100, "final_file_bytes": 200,
                },
            })
        result, = campaign.summarize(runs, .99)
        self.assertFalse(result["meets_target_every_trial"])
        self.assertEqual(result["minimum_trial_mean_recall"], .98)
        self.assertEqual(result["median_trial_ann_p50_ms"], 3)
        self.assertEqual(result["median_file_growth_bytes"], 100)

    def test_invalid_campaign_does_not_create_output(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "must-not-exist.json"
            for flags in [["--trials", "0"], ["--ef", "1"], ["--ef", "32", "32"],
                          ["--mixed-writes", "0"], ["--recall-target", "nan"]]:
                run = subprocess.run(
                    [sys.executable, str(ROOT / "benchmarks" / "native_campaign.py"),
                     "--binary", "missing", "--output", str(output), *flags],
                    capture_output=True,
                )
                self.assertNotEqual(run.returncode, 0)
                self.assertFalse(output.exists())

    def test_failed_child_is_reported_and_existing_output_is_never_clobbered(self):
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / "failure.json"
            command = [sys.executable, str(ROOT / "benchmarks" / "native_campaign.py"),
                       "--binary", sys.executable, "--output", str(output),
                       "--trials", "1", "--ef", "32"]
            first = subprocess.run(command, capture_output=True)
            self.assertNotEqual(first.returncode, 0)
            report = json.loads(output.read_text())
            self.assertEqual(report["status"], "failed")
            self.assertEqual(report["runs"], [])
            self.assertIn("error", report)
            original = output.read_bytes()
            second = subprocess.run(command, capture_output=True)
            self.assertNotEqual(second.returncode, 0)
            self.assertEqual(output.read_bytes(), original)

    def test_published_native_campaign_matches_its_raw_trials(self):
        report = json.loads((ROOT / "benchmarks" / "results" / "native-384-mixed.json").read_text())
        self.assertEqual(report["status"], "complete")
        self.assertEqual(len(report["runs"]), 12)
        self.assertEqual(report["summary"], campaign.summarize(report["runs"], .99))
        datasets, queries = set(), set()
        for run in report["runs"]:
            value = run["measurement"]
            datasets.add(value["dataset_checksum"])
            queries.add(value["query_checksum"])
            self.assertEqual(value["profile"], "release")
            self.assertEqual(value["recall_at_k"], 1.0)
            self.assertGreater(value["process_peak_rss_bytes"], 0)
            self.assertEqual(len(value["query_samples_ms"]), 30)
            self.assertEqual(len(value["hnsw"]["query_samples_ms"]), 30)
            for metrics in [value, value["hnsw"]]:
                samples = sorted(metrics["query_samples_ms"])
                for percentile in [50, 95, 99]:
                    index = (len(samples) * percentile + 99) // 100 - 1
                    self.assertAlmostEqual(metrics[f"query_p{percentile}_ms"], samples[index], places=6)
            writes = run["mixed_writes_per_query"]
            self.assertEqual(len(value["hnsw"]["write_batch_samples_ms"]), 30 if writes else 0)
            self.assertGreaterEqual(value["final_file_bytes"], value["file_bytes"])
            if writes:
                # Query 17 crosses the documented 256-ID buffer capacity.
                samples = value["hnsw"]["query_samples_ms"]
                self.assertEqual(max(samples), samples[16])
        self.assertEqual(len(datasets), 1)
        self.assertEqual(len(queries), 1)


if __name__ == "__main__":
    unittest.main()
