"""Bounded paired exact/filtered ANN campaign with raw samples and whole-child RSS."""

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
SCENARIOS = [
    ("unfiltered", [], None),
    ("broad-50pct", ["--filter-every", "2"], 2),
    ("restrictive-1pct", ["--filter-every", "100"], 100),
    ("no-matches", ["--filter-none"], 0),
]


def validate_measurement(value, records, queries, every):
    eligible = records if every is None else 0 if every == 0 else (records - 1) // every + 1
    expected = min(10, eligible)
    ann = value["hnsw"]
    if (value["profile"] != "release" or value["recall_at_k"] != 1
            or value["eligible_records"] != eligible
            or value["returned_counts"] != [expected] * queries
            or ann["eligible_records_samples"] != [eligible] * queries
            or ann["recall_denominator_samples"] != [expected] * queries
            or ann["recall_denominator"] != expected * queries
            or ann["rebuild_query_numbers"]):
        raise ValueError("release/oracle/eligibility/denominator/no-rebuild validation failed")
    if (len(ann["returned_counts"]) != queries
            or any(not 0 <= count <= expected for count in ann["returned_counts"])
            or not 0 <= ann["recall_at_k"] <= 1
            or (expected == 0 and ann["recall_at_k"] != 1)):
        raise ValueError("invalid returned count or recall")


def summarize(runs, target):
    groups = {}
    for run in runs:
        groups.setdefault((run["metric"], run["scenario"]), []).append(run["measurement"])
    rows = []
    for (metric, scenario), values in sorted(groups.items()):
        denominator = values[0]["hnsw"]["recall_denominator"]
        rows.append({
            "metric": metric, "scenario": scenario, "trials": len(values),
            "eligible_records": values[0]["eligible_records"],
            "exact_returned_per_query": values[0]["returned_per_query"],
            "ann_returned_per_query_min": min(min(v["hnsw"]["returned_counts"]) for v in values),
            "ann_returned_per_query_max": max(max(v["hnsw"]["returned_counts"]) for v in values),
            "recall_denominator_per_trial": denominator,
            "minimum_trial_recall": min(v["hnsw"]["recall_at_k"] for v in values),
            "meets_recall_target_every_trial": None if denominator == 0 else all(
                v["hnsw"]["recall_at_k"] >= target for v in values),
            "median_trial_exact_p50_ms": statistics.median(v["query_p50_ms"] for v in values),
            "median_trial_exact_p95_ms": statistics.median(v["query_p95_ms"] for v in values),
            "median_trial_ann_p50_ms": statistics.median(v["hnsw"]["query_p50_ms"] for v in values),
            "median_trial_ann_p95_ms": statistics.median(v["hnsw"]["query_p95_ms"] for v in values),
            "median_index_build_ms": statistics.median(v["hnsw"]["index_build_ms"] for v in values),
            "median_whole_child_peak_rss_bytes": statistics.median(v["process_peak_rss_bytes"] for v in values),
        })
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--records", type=int, default=2000)
    parser.add_argument("--dimensions", type=int, default=32)
    parser.add_argument("--queries", type=int, default=30)
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--ef", type=int, default=100)
    parser.add_argument("--recall-target", type=float, default=.99)
    args = parser.parse_args()
    if not (100 <= args.records <= 10000 and 1 <= args.dimensions <= 384
            and 1 <= args.queries <= 100 and 1 <= args.trials <= 3
            and args.records * args.dimensions * args.queries <= 100_000_000
            and 10 <= args.ef <= 10000 and 0 < args.recall_target <= 1):
        parser.error("bounded campaign: records 100..10000, dimensions 1..384, queries 1..100, "
                     "trials 1..3, ef 10..10000, at most 100 million oracle components per child")
    binary = args.binary.resolve(strict=True)
    report = {
        "schema_version": 1, "status": "running",
        "utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "platform": platform.platform(), "machine": platform.machine(),
        "processor": platform.processor(), "logical_cpus": os.cpu_count(),
        "python": platform.python_version(),
        "source_commit": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "source_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT)),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "source_hashes": {name: hashlib.sha256((ROOT / name).read_bytes()).hexdigest()
                          for name in ["src/lib.rs", "src/hnsw.rs", "src/bin/vlite-bench.rs",
                                       "benchmarks/filtered_ann_campaign.py", "tests/measure_benchmark.py"]},
        "config": {key: value for key, value in vars(args).items() if key not in ("binary", "output")},
        "fixed_config": {"m": 16, "ef_construction": 100, "k": 10, "seed": 42, "batch_size": 100},
        "recall_policy": "Tie-aware against independently verified filtered exact top-k. "
                         "Recall denominator is sum of min(k, eligible exact count); "
                         "zero denominator uses vacuous recall 1, not measured retrieval quality.",
        "limitations": "Synthetic SplitMix64 f32, independent seeded queries, shared unpinned host, "
                       "single sequential client, warm queries, no concurrent writes. "
                       "Selective and empty predicates may traverse the whole reachable graph. "
                       "RSS is measured whole-child peak, not incremental index memory. "
                       "No hidden exact fallback; exact verification is outside ANN timing.",
        "runs": [],
    }
    scenarios = [(metric, name, flags, every) for metric in ["squared_l2", "cosine"]
                 for name, flags, every in SCENARIOS]
    with args.output.open("x", encoding="utf-8") as stream:
        try:
            for trial in range(args.trials):
                for metric, name, flags, every in scenarios[::1 if trial % 2 == 0 else -1]:
                    arguments = ["--metric", metric, *flags, "--ann", "16", "100", str(args.ef),
                                 str(args.records), str(args.dimensions), str(args.queries), "100", "10", "42"]
                    child = subprocess.run(
                        [sys.executable, str(ROOT / "tests" / "measure_benchmark.py"), str(binary), *arguments],
                        cwd=ROOT, capture_output=True, text=True, check=True,
                    )
                    value = json.loads(child.stdout)
                    validate_measurement(value, args.records, args.queries, every)
                    report["runs"].append({"trial": trial + 1, "metric": metric, "scenario": name,
                                           "arguments": arguments, "measurement": value})
            report["summary"] = summarize(report["runs"], args.recall_target)
            report["status"] = "complete"
        except (OSError, subprocess.SubprocessError, ValueError, KeyError) as error:
            report["status"] = "failed"
            report["error"] = type(error).__name__
            raise
        finally:
            json.dump(report, stream, indent=2, allow_nan=False)
            stream.write("\n")


if __name__ == "__main__":
    main()
