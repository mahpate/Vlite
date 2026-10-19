"""Repeat native exact/ANN measurements; preserve failures and all child samples."""

import argparse
import datetime
import hashlib
import json
import math
import os
import pathlib
import platform
import statistics
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]


def summarize(runs, target):
    groups = {}
    for run in runs:
        key = (run["mixed_writes_per_query"], run["ef_search"])
        groups.setdefault(key, []).append(run["measurement"])
    summary = []
    for (writes, ef), measurements in sorted(groups.items()):
        summary.append({
            "mixed_writes_per_query": writes,
            "ef_search": ef,
            "trials": len(measurements),
            "minimum_trial_mean_recall": min(m["hnsw"]["recall_at_k"] for m in measurements),
            "meets_target_every_trial": all(m["hnsw"]["recall_at_k"] >= target for m in measurements),
            "median_trial_ann_p50_ms": statistics.median(m["hnsw"]["query_p50_ms"] for m in measurements),
            "median_trial_ann_p95_ms": statistics.median(m["hnsw"]["query_p95_ms"] for m in measurements),
            "median_trial_ann_p99_ms": statistics.median(m["hnsw"]["query_p99_ms"] for m in measurements),
            "median_trial_exact_p50_ms": statistics.median(m["query_p50_ms"] for m in measurements),
            "median_index_build_ms": statistics.median(m["hnsw"]["index_build_ms"] for m in measurements),
            "median_whole_child_peak_rss_bytes": statistics.median(m["process_peak_rss_bytes"] for m in measurements),
            "median_file_growth_bytes": statistics.median(m["final_file_bytes"] - m["file_bytes"] for m in measurements),
        })
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--records", type=int, default=2048)
    parser.add_argument("--dimensions", type=int, default=384)
    parser.add_argument("--queries", type=int, default=30)
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--ef", type=int, nargs="+", default=[32, 100])
    parser.add_argument("--mixed-writes", type=int, default=16)
    parser.add_argument("--recall-target", type=float, default=.99)
    args = parser.parse_args()
    if not (1 <= args.trials <= 10 and 1 <= len(args.ef) <= 4
            and args.trials * len(args.ef) * 2 <= 40
            and len(set(args.ef)) == len(args.ef)
            and all(10 <= ef <= 100000 for ef in args.ef)
            and 1 <= args.mixed_writes <= 256
            and math.isfinite(args.recall_target) and 0 < args.recall_target <= 1
            and args.records >= 10 and args.dimensions > 0 and args.queries > 0):
        parser.error("invalid campaign bounds (maximum 40 benchmark children)")
    binary = pathlib.Path(args.binary).resolve(strict=True)
    report = {
        "schema_version": 1, "status": "running",
        "utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "platform": platform.platform(), "machine": platform.machine(),
        "processor": platform.processor(), "logical_cpus": os.cpu_count(),
        "python": platform.python_version(),
        "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "source_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT)),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "source_hashes": {name: hashlib.sha256((ROOT / name).read_bytes()).hexdigest()
                          for name in ["src/bin/vlite-bench.rs", "src/hnsw.rs", "src/lib.rs",
                                       "benchmarks/native_campaign.py", "tests/measure_benchmark.py"]},
        "config": {key: value for key, value in vars(args).items() if key not in ("binary", "output")},
        "runs": [],
    }
    # Exclusive creation: never clobber an archived report, even on failure.
    with open(args.output, "x", encoding="utf-8") as stream:
        try:
            for trial in range(args.trials):
                modes = [0, args.mixed_writes] if trial % 2 == 0 else [args.mixed_writes, 0]
                for writes in modes:
                    for ef in args.ef:
                        flags = ["--mixed-writes", str(writes)] if writes else []
                        flags += ["--ann", "16", "100", str(ef), str(args.records),
                                  str(args.dimensions), str(args.queries), "100", "10", "42"]
                        result = subprocess.run(
                            [sys.executable, str(ROOT / "tests" / "measure_benchmark.py"), str(binary), *flags],
                            cwd=ROOT, capture_output=True, text=True, check=True,
                        )
                        measurement = json.loads(result.stdout)
                        if measurement["profile"] != "release":
                            raise ValueError("campaign requires a release binary")
                        report["runs"].append({
                            "trial": trial + 1, "mixed_writes_per_query": writes,
                            "ef_search": ef, "arguments": flags, "measurement": measurement,
                        })
            report["summary"] = summarize(report["runs"], args.recall_target)
            report["status"] = "complete"
        except (OSError, subprocess.SubprocessError, ValueError, KeyError) as error:
            report["status"] = "failed"
            # Do not publish arbitrary child stderr, which can contain host paths.
            report["error"] = type(error).__name__
            raise
        finally:
            json.dump(report, stream, indent=2, allow_nan=False)
            stream.write("\n")


if __name__ == "__main__":
    main()
