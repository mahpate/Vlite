"""Compare exact metrics, filtered scans and cosine ANN with raw samples and peak RSS."""
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


def summarize(runs):
    groups = {}
    for run in runs:
        groups.setdefault((run["dimensions"], run["scenario"]), []).append(run["measurement"])
    result = []
    for (dimensions, scenario), values in sorted(groups.items()):
        row = {
            "dimensions": dimensions, "scenario": scenario, "trials": len(values),
            "median_trial_exact_p50_ms": statistics.median(v["query_p50_ms"] for v in values),
            "median_trial_exact_p95_ms": statistics.median(v["query_p95_ms"] for v in values),
            "median_whole_child_peak_rss_bytes": statistics.median(v["process_peak_rss_bytes"] for v in values),
        }
        if "hnsw" in values[0]:
            row.update({
                "median_trial_ann_p50_ms": statistics.median(v["hnsw"]["query_p50_ms"] for v in values),
                "median_trial_ann_p95_ms": statistics.median(v["hnsw"]["query_p95_ms"] for v in values),
                "median_index_build_ms": statistics.median(v["hnsw"]["index_build_ms"] for v in values),
                "minimum_trial_recall": min(v["hnsw"]["recall_at_k"] for v in values),
            })
        result.append(row)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--records", type=int, default=1500)
    parser.add_argument("--dimensions", nargs="+", type=int, default=[128, 384])
    parser.add_argument("--queries", type=int, default=30)
    parser.add_argument("--trials", type=int, default=3)
    args = parser.parse_args()
    if not (1 <= args.trials <= 3 and 1 <= len(args.dimensions) <= 2
            and len(set(args.dimensions)) == len(args.dimensions)
            and all(1 <= d <= 1024 for d in args.dimensions)
            and 10 <= args.records <= 10000 and 1 <= args.queries <= 100):
        parser.error("bounded campaign: trials 1..3, one/two distinct dimensions 1..1024, records 10..10000, queries 1..100")
    binary = args.binary.resolve(strict=True)
    report = {
        "schema_version": 1, "status": "running",
        "utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "platform": platform.platform(), "machine": platform.machine(), "logical_cpus": os.cpu_count(),
        "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "source_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT)),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "source_hashes": {name: hashlib.sha256((ROOT / name).read_bytes()).hexdigest() for name in [
            "src/lib.rs", "src/hnsw.rs", "src/bin/vlite-bench.rs",
            "benchmarks/metric_campaign.py", "tests/measure_benchmark.py",
        ]},
        "config": {"records": args.records, "dimensions": args.dimensions,
                   "queries": args.queries, "trials": args.trials, "k": 10, "seed": 42},
        "limitations": "Synthetic fixed-point vectors, not trained embeddings; shared host, whole-child peak RSS, no marginal-index memory or general recall/latency guarantee. Mixed-query timings include synchronous rebuilds.",
        "runs": [],
    }
    scenarios = [(metric + suffix, ["--metric", metric] + flags)
                 for metric in ["squared_l2", "cosine", "inner_product"]
                 for suffix, flags in [("-exact", []), ("-filtered-10pct", ["--filter-every", "10"])]]
    scenarios += [(f"cosine-ann-{ef}-writes-{writes}", ["--metric", "cosine"]
                   + (["--mixed-writes", str(writes)] if writes else [])
                   + ["--ann", "16", "100", str(ef)])
                  for ef, writes in [(32, 0), (100, 0), (100, 16)]]
    with args.output.open("x", encoding="utf-8") as stream:
        try:
            for trial in range(args.trials):
                for dimensions in args.dimensions:
                    for scenario, flags in scenarios[::1 if trial % 2 == 0 else -1]:
                        flags = flags + [str(args.records), str(dimensions), str(args.queries), "100", "10", "42"]
                        child = subprocess.run([sys.executable, str(ROOT / "tests" / "measure_benchmark.py"),
                                                str(binary), *flags], capture_output=True, text=True, check=True)
                        value = json.loads(child.stdout)
                        if value["profile"] != "release" or value["recall_at_k"] != 1:
                            raise ValueError("campaign requires a release binary and exact oracle agreement")
                        report["runs"].append({"trial": trial + 1, "dimensions": dimensions,
                                               "scenario": scenario, "arguments": flags, "measurement": value})
            report["summary"] = summarize(report["runs"])
            report["status"] = "complete"
        except (subprocess.CalledProcessError, ValueError) as error:
            report["status"] = "failed"
            report["error"] = str(error)
            if isinstance(error, subprocess.CalledProcessError):
                report["child_stderr"] = error.stderr
            raise
        finally:
            json.dump(report, stream, indent=2)
            stream.write("\n")


if __name__ == "__main__":
    main()
