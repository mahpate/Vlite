"""Validate snapshot benchmark correctness and timing requirements."""
import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "campaign", Path(__file__).resolve().parents[1] / "benchmarks" / "ann_restart_campaign.py")
campaign = importlib.util.module_from_spec(spec)
spec.loader.exec_module(campaign)


class RestartReport(unittest.TestCase):
    def setUp(self):
        self.report = {
            "benchmark": "ann_restart", "profile": "release",
            "exact_hits_match": True, "rebuild_hits_equal_pre_save": True,
            "mismatched_queries": 0, "load_outcome": "loaded",
            "timings_ns": {
                "reopen": 1, "load": 2, "restart_with_snapshot": 3, "save": 4,
                "initial_build": 5, "rebuild_trials": [{"reopen": 1, "build": 5}],
            },
        }

    def test_valid_report(self):
        campaign.validate(self.report)

    def test_rejects_mismatched_results_and_invalid_timings(self):
        for field, value in [
            ("exact_hits_match", False), ("rebuild_hits_equal_pre_save", False),
            ("mismatched_queries", 1), ("load_outcome", "stale"), ("profile", "debug"),
        ]:
            report = copy.deepcopy(self.report)
            report[field] = value
            with self.assertRaises(ValueError):
                campaign.validate(report)
        for value in (-1, float("nan"), True):
            report = copy.deepcopy(self.report)
            report["timings_ns"]["load"] = value
            with self.assertRaises(ValueError):
                campaign.validate(report)


if __name__ == "__main__":
    unittest.main()
