"""Tests for the graph reference runner's measured quiet-window checks."""

import json
import os
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from graph_reference import (
    CpuClock, calm_preflight, cpu_summary, reference_identity, run_process, source_manifest,
)


class ReferenceRunnerTests(unittest.TestCase):
    def test_background_mean_is_time_weighted_and_cpu_normalized(self):
        result = cpu_summary([
            {"elapsed_seconds": 0.5, "background_cpu_seconds": 0.04},
            {"elapsed_seconds": 1.5, "background_cpu_seconds": 0.16},
        ], 16)
        self.assertAlmostEqual(result["mean_background_cpu_fraction"], 0.2 / 32)
        self.assertAlmostEqual(result["max_background_cpu_fraction"], 0.16 / 24)
        self.assertTrue(result["quiet"])

    def test_competing_mean_or_sample_rejects_a_run(self):
        for samples in [
            [{"elapsed_seconds": 1, "background_cpu_seconds": 3}],
            [
                {"elapsed_seconds": 0.5, "background_cpu_seconds": 5},
                {"elapsed_seconds": 9.5, "background_cpu_seconds": 0},
            ],
        ]:
            self.assertFalse(cpu_summary(samples, 16)["quiet"])

    def test_gate_tolerates_the_measured_background_floor_but_not_a_build(self):
        floor = [{"elapsed_seconds": 10, "background_cpu_seconds": 8}]
        self.assertTrue(cpu_summary(floor, 16)["quiet"])
        burst = [
            {"elapsed_seconds": 0.5, "background_cpu_seconds": 3.6},
            {"elapsed_seconds": 9.5, "background_cpu_seconds": 4},
        ]
        self.assertTrue(cpu_summary(burst, 16)["quiet"])
        build = [{"elapsed_seconds": 10, "background_cpu_seconds": 40}]
        self.assertFalse(cpu_summary(build, 16)["quiet"])

    def test_preflight_repeats_noisy_probes_and_keeps_every_one(self):
        outcomes = iter([False, False, True, True])
        calls = []

        def probe():
            calls.append(1)
            return {"quiet": next(outcomes)}

        probes = calm_preflight(probe, attempts=5)
        self.assertEqual([item["quiet"] for item in probes], [False, False, True])
        self.assertEqual(len(calls), 3)

    def test_preflight_stops_after_the_attempt_bound(self):
        probes = calm_preflight(lambda: {"quiet": False}, attempts=4)
        self.assertEqual(len(probes), 4)
        self.assertFalse(probes[-1]["quiet"])

    def test_short_exit_sample_does_not_dominate_peak(self):
        result = cpu_summary([
            {"elapsed_seconds": 1, "background_cpu_seconds": 0},
            {"elapsed_seconds": 0.001, "background_cpu_seconds": 0.01},
        ], 16)
        self.assertEqual(result["max_background_cpu_fraction"], 0)
        self.assertGreater(result["mean_background_cpu_fraction"], 0)

    def test_identity_accepts_only_the_azure_baseline_host(self):
        machine = {
            "cpu": [{"Name": "Intel(R) Xeon(R) Platinum 8370C CPU @ 2.80GHz"}],
            "logical_cpus": 16, "ram_bytes": 68_713_967_616, "os_version": "10.0.20348",
            "volume": {"DriveLetter": "F", "FileSystem": "NTFS"},
            "disk": {"FriendlyName": "Msft Virtual Disk", "BusType": "SAS"},
        }
        self.assertTrue(all(reference_identity(machine).values()))
        retired = dict(machine, ram_bytes=68_665_831_424, os_version="10.0.26300",
                       volume={"DriveLetter": "Q", "FileSystem": "ReFS"})
        checks = reference_identity(retired)
        self.assertFalse(checks["ram"] or checks["os"] or checks["volume"])
        self.assertTrue(checks["cpu"] and checks["disk"])

    def test_source_manifest_covers_benchmarks_engine_and_lockfile(self):
        manifest = source_manifest()
        for path in [
            "Cargo.lock", "examples/graph_bench.rs",
            "src/property_graph/engine/state.rs", "scripts/graph_reference.py",
        ]:
            self.assertEqual(len(manifest[path]), 64)

    @unittest.skipUnless(os.name == "nt", "Windows reference host APIs")
    def test_process_monitor_preserves_output_memory_and_failure(self):
        clock = CpuClock()
        result = run_process([
            sys.executable, "-c",
            "import json,time; data=bytearray(16*1024*1024); "
            "time.sleep(.55); print(json.dumps({'bytes':len(data)}))",
        ], clock, os.cpu_count())
        self.assertEqual(result["returncode"], 0)
        self.assertEqual(json.loads(result["stdout"])["bytes"], 16 * 1024 * 1024)
        self.assertGreater(result["process_peak_working_set_bytes"], 0)
        self.assertGreaterEqual(len(result["cpu"]["samples"]), 1)
        failed = run_process([
            sys.executable, "-c", "import sys; print('expected',file=sys.stderr); sys.exit(7)",
        ], clock, os.cpu_count())
        self.assertEqual(failed["returncode"], 7)
        self.assertIn("expected", failed["stderr"])


if __name__ == "__main__":
    unittest.main()
