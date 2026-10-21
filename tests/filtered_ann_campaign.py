"""Contract checks for the bounded filtered ANN campaign (no latency assertions)."""

import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location(
    "filtered_ann_campaign", Path(__file__).resolve().parents[1] / "benchmarks" / "filtered_ann_campaign.py")
CAMPAIGN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CAMPAIGN)


def measurement(eligible, returned, denominator, recall=1):
    return {
        "profile": "release", "recall_at_k": 1, "eligible_records": eligible,
        "returned_per_query": returned, "returned_counts": [returned] * 2,
        "query_p50_ms": .2, "query_p95_ms": .3, "process_peak_rss_bytes": 1000,
        "hnsw": {
            "eligible_records_samples": [eligible] * 2,
            "recall_denominator_samples": [returned] * 2,
            "recall_denominator": denominator, "returned_counts": [returned] * 2,
            "recall_at_k": recall, "rebuild_query_numbers": [],
            "query_p50_ms": .4, "query_p95_ms": .5, "index_build_ms": 1,
        },
    }


class FilteredCampaignTests(unittest.TestCase):
    def test_all_selectivities_validate_and_empty_denominator_is_explicit(self):
        for every, eligible, returned in [(None, 200, 10), (2, 100, 10), (100, 2, 2), (0, 0, 0)]:
            with self.subTest(every=every):
                value = measurement(eligible, returned, returned * 2)
                CAMPAIGN.validate_measurement(value, 200, 2, every)
        rows = CAMPAIGN.summarize([{"metric": "cosine", "scenario": "no-matches",
                                    "measurement": measurement(0, 0, 0)}], .99)
        self.assertIsNone(rows[0]["meets_recall_target_every_trial"])
        self.assertEqual(rows[0]["recall_denominator_per_trial"], 0)

    def test_wrong_denominator_or_ineligible_count_is_rejected(self):
        for field, invalid in [("recall_denominator", 20), ("returned_counts", [3, 3]),
                               ("eligible_records_samples", [200, 200]),
                               ("rebuild_query_numbers", [1])]:
            with self.subTest(field=field):
                value = measurement(2, 2, 4)
                value["hnsw"][field] = invalid
                with self.assertRaises(ValueError):
                    CAMPAIGN.validate_measurement(value, 200, 2, 100)

    def test_recall_target_is_reported_not_assumed_from_ef(self):
        rows = CAMPAIGN.summarize([{"metric": "cosine", "scenario": "broad",
                                    "measurement": measurement(100, 10, 20, .8)}], .99)
        self.assertFalse(rows[0]["meets_recall_target_every_trial"])
        self.assertEqual(rows[0]["minimum_trial_recall"], .8)


if __name__ == "__main__":
    unittest.main()
