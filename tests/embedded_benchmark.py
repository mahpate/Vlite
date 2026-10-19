"""Integrity checks for retained embedded-client measurements and CLI bounds."""
import json
import math
from pathlib import Path
import statistics
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class EmbeddedBenchmarkTests(unittest.TestCase):
    def test_retained_samples_and_throughput_are_consistent(self):
        report = json.loads((ROOT / "benchmarks" / "results" / "embedded-python-windows.json").read_text())
        self.assertTrue(report["verified"])
        self.assertEqual(report["schema_version"], 1)
        workload = report["workload"]
        for name, data in report["measurements"].items():
            samples = data["elapsed_ns"]
            count = workload["trials"] * (workload["read_passes"] if name in ("get", "get_many") else 1)
            self.assertEqual(len(samples), count)
            self.assertTrue(all(type(n) is int and n > 0 for n in samples))
            expected = workload["records"] * 1e9 / statistics.median(samples)
            self.assertTrue(math.isfinite(data["median_records_per_second"]))
            self.assertAlmostEqual(data["median_records_per_second"], expected)
        self.assertEqual(len(report["library_sha256"]), 64)

    def test_invalid_bounds_and_existing_output_fail_without_overwrite(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "report.json"
            args = [sys.executable, str(ROOT / "benchmarks" / "embedded.py"),
                    "--library", "missing-native-library", "--output", str(output)]
            result = subprocess.run(args + ["--records", "0"], capture_output=True, timeout=20)
            self.assertEqual(result.returncode, 2)
            self.assertFalse(output.exists())
            output.write_text("keep", encoding="utf-8")
            result = subprocess.run(args, capture_output=True, timeout=20)
            self.assertEqual(result.returncode, 2)
            self.assertEqual(output.read_text(), "keep")


if __name__ == "__main__":
    unittest.main()
