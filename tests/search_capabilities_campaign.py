import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "campaign",
    Path(__file__).resolve().parents[1] / "benchmarks" / "search_capabilities_campaign.py",
)
campaign = importlib.util.module_from_spec(spec)
spec.loader.exec_module(campaign)


def valid_report():
    rows = []
    for metric in ("squared_l2", "cosine"):
        for quantization in ("none", "scalar_u8"):
            rows.append({
                "metric": metric, "quantization": quantization,
                "recall_at_k": 0.9, "recall_denominator": 20,
                "query_samples_ms": [0.1, 0.2], "exact_samples_ms": [0.2, 0.3],
                "graph_link_bytes": 100,
                "code_bytes": 100 if quantization == "scalar_u8" else 0,
                "codebook_bytes": 32 if quantization == "scalar_u8" else 0,
                "buffer_vector_bytes": 0, "resident_vector_bytes": 400,
                "quantized_distance_evaluations": 100 if quantization == "scalar_u8" else 0,
            })
    return {
        "schema": 1, "queries": 2, "k": 10, "rows": rows,
        "metadata_index": {
            "strategy": "exact_indexed", "identical_results": True,
            "indexed_records": 2, "scan_records": 200,
        },
    }


class CampaignTests(unittest.TestCase):
    def test_accepts_complete_report(self):
        campaign.validate(valid_report())

    def test_rejects_invalid_measurements(self):
        mutations = [
            lambda r: r["rows"][0].update(recall_at_k=float("nan")),
            lambda r: r["rows"][0].update(recall_denominator=0),
            lambda r: r["rows"][0].update(query_samples_ms=[-1, 1]),
            lambda r: r["rows"][0].update(exact_samples_ms=[1]),
            lambda r: r["rows"][0].update(resident_vector_bytes=-1),
            lambda r: r["rows"][1].update(quantized_distance_evaluations=0),
            lambda r: r["rows"][0].update(quantized_distance_evaluations=10),
            lambda r: r.update(k=0),
            lambda r: r["rows"].pop(),
            lambda r: r["metadata_index"].update(indexed_records=200),
            lambda r: r["metadata_index"].update(identical_results=False),
        ]
        for mutate in mutations:
            report = copy.deepcopy(valid_report())
            mutate(report)
            with self.subTest(report=report), self.assertRaises(ValueError):
                campaign.validate(report)


if __name__ == "__main__":
    unittest.main()
