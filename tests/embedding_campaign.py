"""Offline real-input campaign tests; optional release binary exercises the full pipeline."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("embedding_campaign", ROOT / "benchmarks" / "embedding_campaign.py")
campaign = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(campaign)
BINARY = None
if "--binary" in sys.argv:
    index = sys.argv.index("--binary")
    BINARY = str(Path(sys.argv[index + 1]).resolve(strict=True))
    del sys.argv[index:index + 2]


def fixture(directory):
    manifest = {
        "dataset": "synthetic offline integration fixture (NOT GloVe)",
        "source_url": "fixture", "source_sha256": "fixture",
        "license": "MIT test fixture", "license_url": "fixture",
        "model": "none", "normalization": "none", "split_policy": "separate generated values",
        "dimensions": 8, "files": [],
    }
    for index, (name, rows) in enumerate([("vectors.txt", 32), ("queries.txt", 2), ("updates.txt", 8)]):
        path = directory / name
        path.write_text("".join(" ".join(str((r + 1) * (d + 2) + index) for d in range(8)) + "\n"
                                for r in range(rows)), encoding="ascii")
        manifest["files"].append({"name": name, "rows": rows, "sha256": campaign.digest(path)})
    (directory / "dataset-manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
    return manifest


class EmbeddingCampaignTests(unittest.TestCase):
    def command(self, directory, output, binary=None):
        return [sys.executable, str(ROOT / "benchmarks" / "embedding_campaign.py"),
                "--binary", binary or sys.executable, "--dataset", str(directory),
                "--output", str(output), "--writes", "4", "--deletes", "2",
                "--ef", "32", "--trials", "1"]

    def test_hashes_paths_and_provenance_are_checked_before_execution(self):
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            manifest = fixture(directory)
            self.assertEqual(campaign.inputs(directory)[1]["updates.txt"], 8)
            (directory / "vectors.txt").write_text("changed")
            with self.assertRaisesRegex(ValueError, "hash mismatch"):
                campaign.inputs(directory)
            fixture(directory)
            for field, value in [("files", manifest["files"] + [manifest["files"][0]]),
                                 ("files", [{"name": "../escape"}]),
                                 ("license", ""), ("dimensions", 0)]:
                changed = dict(manifest, **{field: value})
                (directory / "dataset-manifest.json").write_text(json.dumps(changed))
                with self.assertRaises(ValueError):
                    campaign.inputs(directory)

    def test_invalid_bounds_and_failed_child_never_look_successful(self):
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            fixture(directory)
            output = directory / "report.json"
            command = self.command(directory, output)
            for flags in [["--trials", "0"], ["--ef", "1"], ["--ef", "32", "32"],
                          ["--writes", "0"], ["--deletes", "256"], ["--recall-target", "nan"],
                          ["--writes", "5"]]:
                self.assertNotEqual(subprocess.run(command + flags, capture_output=True).returncode, 0)
                self.assertFalse(output.exists())
            self.assertNotEqual(subprocess.run(command, capture_output=True).returncode, 0)
            report = json.loads(output.read_text())
            self.assertEqual(report["status"], "failed")
            self.assertEqual(report["runs"], [])
            original = output.read_bytes()
            self.assertNotEqual(subprocess.run(command, capture_output=True).returncode, 0)
            self.assertEqual(output.read_bytes(), original)

    def test_summary_preserves_low_recall_and_rebuild_tails(self):
        values = []
        for recall, samples in [(1, [1, 100, 3]), (.98, [2, 200, 4])]:
            values.append({"scenario": "mixed", "measurement": {
                "query_p50_ms": 4, "process_peak_rss_bytes": 1024,
                "file_bytes": 100, "final_file_bytes": 200,
                "hnsw": {"recall_at_k": recall, "query_p50_ms": samples[2],
                         "query_p95_ms": samples[1], "query_p99_ms": samples[1],
                         "query_samples_ms": samples, "index_build_ms": 100,
                         "rebuild_query_numbers": [2]}}})
        row, = campaign.summarize(values, .99)
        self.assertFalse(row["meets_target_every_trial"])
        self.assertEqual(row["minimum_trial_mean_recall"], .98)
        self.assertEqual(row["maximum_query_ms"], 200)
        self.assertEqual(row["median_non_rebuild_query_ms"], 2.5)
        self.assertEqual(row["rebuilds_per_trial"], [1, 1])

    def test_preparation_rejects_missing_source_without_output(self):
        # Import optional dependencies only in the actual preparation process.
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            output = directory / "export"
            result = subprocess.run([sys.executable, str(ROOT / "benchmarks" / "prepare_glove.py"),
                                     "--source", str(directory / "missing.hdf5"), "--output", str(output)],
                                    capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(output.exists())

    def test_published_real_campaign_matches_raw_samples_and_mutation_history(self):
        report = json.loads((ROOT / "benchmarks" / "results" / "glove-100-windows.json").read_text())
        self.assertEqual(report["status"], "complete")
        self.assertEqual(len(report["runs"]), 24)
        self.assertEqual(report["summary"], campaign.summarize(report["runs"], .99))
        manifest = report["dataset"]
        self.assertIn("PDDL", manifest["license"])
        base, queries, updates = manifest["files"]
        self.assertEqual((base["split"], queries["split"], updates["split"]), ("train", "test", "train"))
        self.assertEqual(base["stop"], updates["start"])
        checksums = set()
        update_checksums = set()
        for run in report["runs"]:
            value = run["measurement"]
            self.assertEqual(value["recall_at_k"], 1)
            self.assertEqual(value["input"], "external-f32-text")
            self.assertEqual(len(value["query_samples_ms"]), 100)
            self.assertGreater(value["process_peak_rss_bytes"], 0)
            checksums.add((value["dataset_checksum"], value["query_checksum"]))
            if "hnsw" in value:
                ann = value["hnsw"]
                self.assertEqual(len(ann["recall_samples"]), 100)
                self.assertEqual(len(ann["live_records_samples"]), 100)
                self.assertAlmostEqual(ann["recall_at_k"], sum(ann["recall_samples"]) / 100, places=6)
                self.assertEqual(len(ann["query_samples_ms"]), 100)
                if run["scenario"].endswith("-mixed"):
                    self.assertEqual(ann["mixed_deletes_per_query"], 4)
                    self.assertEqual(ann["mixed_writes_per_query"], 16)
                    self.assertEqual(ann["rebuild_query_numbers"], list(range(13, 100, 13)))
                    self.assertEqual(ann["live_records_samples"], list(range(2996, 2599, -4)))
                    self.assertEqual(value["final_live_records"], 2600)
                    update_checksums.add(value["update_checksum"])
                else:
                    self.assertEqual(ann["rebuild_query_numbers"], [])
        self.assertEqual(len(checksums), 1)
        self.assertEqual(len(update_checksums), 1)

    @unittest.skipUnless(BINARY, "pass --binary to exercise background pipeline")
    def test_background_campaign_preserves_availability_and_no_overwrite_contract(self):
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            fixture(directory)
            output = directory / "background.json"
            command = [sys.executable, str(ROOT / "benchmarks" / "background_campaign.py"),
                       "--binary", BINARY, "--dataset", str(directory), "--output", str(output),
                       "--trials", "1", "--interval-ms", "0", "--writes", "4", "--deletes", "2",
                       "--modes", "lazy", "background", "maintenance"]
            for flags in [["--interval-ms", "-1"], ["--interval-ms", "1001"],
                          ["--interval-ms", "0", "0"], ["--interval-ms", "0", "1", "2", "3"],
                          ["--modes", "maintenance", "maintenance"], ["--trials", "0"]]:
                self.assertNotEqual(subprocess.run(command + flags, capture_output=True).returncode, 0)
                self.assertFalse(output.exists())
            result = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads(output.read_text())
            self.assertEqual(report["status"], "complete")
            self.assertEqual(len(report["runs"]), 3)
            self.assertEqual(len(report["summary"]), 3)
            self.assertNotIn(str(directory), output.read_text())
            background, = [r["measurement"]["hnsw"] for r in report["runs"] if r["mode"] == "background"]
            self.assertEqual(background["rebuild_query_numbers"], [])
            state = background["background"]
            self.assertEqual(state["served_queries"] + len(state["not_ready_query_numbers"]), 2)
            maintenance, = [r["measurement"]["hnsw"] for r in report["runs"] if r["mode"] == "maintenance"]
            self.assertEqual(maintenance["rebuild_query_numbers"], [])
            state = maintenance["background"]
            self.assertEqual(state["start_query_numbers"], [])
            self.assertEqual(state["policy"]["rebuild_at_buffered_ids"], 128)
            self.assertEqual(len(state["policy"]["capacity_samples"]), 2)
            self.assertEqual(state["policy"]["capacity_samples"][1]["active_buffer"]["buffered_ids"], 12)
            original = output.read_bytes()
            self.assertNotEqual(subprocess.run(command, capture_output=True).returncode, 0)
            self.assertEqual(output.read_bytes(), original)

    def test_published_background_report_does_not_hide_rejected_requests(self):
        report = json.loads((ROOT / "benchmarks" / "results" / "glove-100-background-windows.json").read_text())
        self.assertEqual(report["status"], "complete")
        self.assertEqual(len(report["runs"]), 12)
        # The runner imports its shared provenance helper from the benchmark folder.
        sys.path.insert(0, str(ROOT / "benchmarks"))
        try:
            spec = importlib.util.spec_from_file_location("background_campaign", ROOT / "benchmarks" / "background_campaign.py")
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
        finally:
            sys.path.pop(0)
        self.assertEqual(report["summary"], module.summarize(report["runs"]))
        for run in report["runs"]:
            measurement = run["measurement"]
            ann = measurement["hnsw"]
            self.assertEqual(measurement["recall_at_k"], 1)
            self.assertEqual(measurement["final_live_records"], 2600)
            self.assertEqual(len(ann["request_samples_ms"]), 100)
            if run["mode"] == "background":
                state = ann["background"]
                rejected = state["not_ready_query_numbers"]
                self.assertEqual(ann["rebuild_query_numbers"], [])
                self.assertEqual(state["served_queries"] + len(rejected), 100)
                for query in rejected:
                    self.assertEqual(ann["recall_samples"][query - 1], 0)
                self.assertAlmostEqual(ann["recall_at_k"], sum(ann["recall_samples"]) / 100, places=6)
                self.assertEqual(state["served_recall_at_k"], 1)
                self.assertTrue(all(1 <= q <= 100 for q in state["published_query_numbers"]))
                self.assertEqual(len(state["maintenance_samples_ms"]), 100)
            else:
                self.assertNotIn("background", ann)
                self.assertEqual(ann["recall_at_k"], 1)

    def test_published_maintenance_report_preserves_capacity_and_overload(self):
        report = json.loads((ROOT / "benchmarks" / "results" / "glove-100-maintenance-windows.json").read_text())
        self.assertEqual(report["status"], "complete")
        self.assertEqual(len(report["runs"]), 18)
        sys.path.insert(0, str(ROOT / "benchmarks"))
        try:
            spec = importlib.util.spec_from_file_location("background_campaign", ROOT / "benchmarks" / "background_campaign.py")
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
        finally:
            sys.path.pop(0)
        self.assertEqual(report["summary"], module.summarize(report["runs"]))
        for run in report["runs"]:
            measurement = run["measurement"]
            ann = measurement["hnsw"]
            self.assertEqual(measurement["recall_at_k"], 1)
            self.assertEqual(measurement["final_live_records"], 2600)
            self.assertEqual(len(ann["request_samples_ms"]), 100)
            self.assertAlmostEqual(ann["recall_at_k"], sum(ann["recall_samples"]) / 100, places=6)
            if run["mode"] == "lazy":
                self.assertNotIn("background", ann)
                self.assertEqual(ann["recall_at_k"], 1)
                continue
            self.assertEqual(run["mode"], "maintenance")
            self.assertEqual(ann["rebuild_query_numbers"], [])
            state = ann["background"]
            self.assertEqual(state["start_query_numbers"][0], 8)
            rejected = state["not_ready_query_numbers"]
            self.assertEqual(state["served_queries"] + len(rejected), 100)
            self.assertAlmostEqual(state["served_recall_at_k"],
                                   sum(ann["recall_samples"]) / state["served_queries"], places=6)
            policy = state["policy"]
            self.assertEqual(policy["rebuild_at_buffered_ids"], 128)
            self.assertEqual(policy["rebuild_at_buffer_vector_bytes"], 4 * 1024 * 1024)
            self.assertEqual(len(policy["capacity_samples"]), 100)
            for number, sample in enumerate(policy["capacity_samples"], 1):
                self.assertEqual(sample["ready"], number not in rejected)
                active = sample["active_buffer"]
                if active is not None:
                    self.assertEqual(active["remaining_ids"] + active["buffered_ids"], 256)
                    self.assertEqual(active["remaining_vector_bytes"] + active["buffer_vector_bytes"], 8 * 1024 * 1024)
                job = sample["rebuild"]
                if job is not None:
                    self.assertLessEqual(job["processed_records"], job["snapshot_records"])
                    if job["catchup_remaining_ids"] is not None:
                        self.assertEqual(job["catchup_remaining_ids"] + job["catchup_buffered_ids"], 256)
                        self.assertEqual(job["catchup_remaining_vector_bytes"] + job["catchup_vector_bytes"], 8 * 1024 * 1024)
                if number in rejected:
                    self.assertIn(sample["pressure"], ["Unavailable", "CatchUpOverflow"])
                    self.assertEqual(ann["recall_samples"][number - 1], 0)

    @unittest.skipUnless(BINARY, "pass --binary to exercise native pipeline")
    def test_offline_release_campaign_reports_real_input_mutations_and_no_local_paths(self):
        with tempfile.TemporaryDirectory() as name:
            directory = Path(name)
            fixture(directory)
            output = directory / "report.json"
            result = subprocess.run(self.command(directory, output, BINARY), capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads(output.read_text())
            self.assertEqual(report["status"], "complete")
            self.assertEqual(len(report["runs"]), 4)
            self.assertEqual(report["summary"], campaign.summarize(report["runs"], .99))
            self.assertNotIn(str(directory), output.read_text())
            for run in report["runs"]:
                measurement = run["measurement"]
                self.assertEqual(measurement["input"], "external-f32-text")
                self.assertGreater(measurement["process_peak_rss_bytes"], 0)
                if run["scenario"].endswith("-mixed"):
                    self.assertEqual(measurement["hnsw"]["mixed_deletes_per_query"], 2)
                    self.assertEqual(measurement["hnsw"]["live_records_samples"], [30, 28])
                    self.assertEqual(len(measurement["hnsw"]["recall_samples"]), 2)
                    self.assertIn("update_checksum", measurement)


if __name__ == "__main__":
    unittest.main()
