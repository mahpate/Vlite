"""Compare synchronous and opt-in background HNSW under identical mixed mutations."""
import argparse
import datetime
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import sys

from embedding_campaign import digest, inputs

ROOT = Path(__file__).resolve().parents[1]


def percentile(values, p):
    return sorted(values)[math.ceil(len(values) * p / 100) - 1]


def summarize(runs):
    groups = {}
    for run in runs:
        groups.setdefault((run["mode"], run["interval_ms"]), []).append(run["measurement"])
    rows = []
    for (mode, interval), values in sorted(groups.items()):
        ann = [v["hnsw"] for v in values]
        served = [v.get("background", {}).get("served_queries", len(v["query_samples_ms"])) for v in ann]
        attempted = [len(v["query_samples_ms"]) for v in ann]
        latencies = []
        for v in ann:
            rejected = set(v.get("background", {}).get("not_ready_query_numbers", []))
            latencies.append([sample for i, sample in enumerate(v["query_samples_ms"], 1) if i not in rejected])
        served_recall = [v.get("background", {}).get("served_recall_at_k", v["recall_at_k"]) for v in ann]
        rows.append({
            "mode": mode, "interval_ms": interval, "trials": len(values),
            "served_queries_per_trial": served,
            "attempted_queries_per_trial": attempted,
            "minimum_availability_fraction": min(s / a for s, a in zip(served, attempted)),
            "minimum_delivered_recall_including_rejections": min(v["recall_at_k"] for v in ann),
            "minimum_served_query_recall": min((v for v in served_recall if v is not None), default=None),
            "median_trial_served_query_p50_ms": statistics.median(percentile(v, 50) for v in latencies if v) if any(latencies) else None,
            "median_trial_served_query_p99_ms": statistics.median(percentile(v, 99) for v in latencies if v) if any(latencies) else None,
            "median_trial_request_p99_ms": statistics.median(percentile(v["request_samples_ms"], 99) for v in ann),
            "maximum_request_ms": max(max(v["request_samples_ms"]) for v in ann),
            "median_peak_whole_child_rss_bytes": statistics.median(v["process_peak_rss_bytes"] for v in values),
            "synchronous_rebuilds_per_trial": [len(v["rebuild_query_numbers"]) for v in ann],
            "background_publications_per_trial": [len(v.get("background", {}).get("published_query_numbers", [])) for v in ann],
            "catchup_overflows_per_trial": [len(v.get("background", {}).get("catchup_overflow_query_numbers", [])) for v in ann],
        })
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--dataset", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--interval-ms", type=int, nargs="+", default=[0, 250])
    parser.add_argument("--writes", type=int, default=16)
    parser.add_argument("--deletes", type=int, default=4)
    args = parser.parse_args()
    if not (1 <= args.trials <= 3 and 1 <= len(args.interval_ms) <= 2
            and len(set(args.interval_ms)) == len(args.interval_ms)
            and all(0 <= n <= 1000 for n in args.interval_ms)
            and 1 <= args.writes and 1 <= args.deletes and args.writes + args.deletes <= 256):
        parser.error("invalid bounded campaign configuration")
    manifest, counts = inputs(args.dataset)
    if counts["updates.txt"] != counts["queries.txt"] * args.writes or args.writes + args.deletes > counts["vectors.txt"]:
        parser.error("update row count/mutation width mismatch")
    binary = args.binary.resolve(strict=True)
    names = ["src/lib.rs", "src/hnsw.rs", "src/rebuild.rs", "src/bin/vlite-bench.rs",
             "benchmarks/background_campaign.py", "benchmarks/embedding_campaign.py", "tests/measure_benchmark.py"]
    report = {
        "schema_version": 1, "status": "running",
        "utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "platform": platform.platform(), "machine": platform.machine(), "logical_cpus": os.cpu_count(),
        "python_version": platform.python_version(),
        "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "source_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT)),
        "source_hashes": {name: digest(ROOT / name) for name in names}, "binary_sha256": digest(binary),
        "dataset": manifest,
        "config": {"trials": args.trials, "interval_ms": args.interval_ms, "writes": args.writes,
                   "deletes": args.deletes, "ef_search": 100, "m": 16, "ef_construction": 100, "k": 10},
        "policy": "Initial synchronous build in both modes. Background mode polls once before each mutation/query and immediately starts a replacement after each terminal/idle result. At most one worker, ready-only queries, no fallback. Sleep before each attempted request; not a fixed arrival rate. Request timing includes poll/publication/snapshot, mutation generation and durable commit, and query; excludes pacing and oracle. Final active job is cancelled/joined by close.",
        "limitations": "Shared unpinned native host. Availability must accompany served-query latency. Rejection is not a successful fast query. Snapshot vector bytes/record count bounded, not total RSS; overlapping snapshot/graphs and mutation originals increase peak memory. No REST/SDK/background API benchmark.",
        "runs": [],
    }
    files = [str((args.dataset / name).resolve()) for name in ["vectors.txt", "queries.txt", "updates.txt"]]
    base = ["--metric", "cosine", "--vectors", files[0], "--query-vectors", files[1],
            "--update-vectors", files[2], "--mixed-writes", str(args.writes), "--mixed-deletes", str(args.deletes)]
    positional = [str(counts["vectors.txt"]), str(manifest["dimensions"]), str(counts["queries.txt"]), "100", "10", "42"]
    scenarios = [(mode, interval) for interval in args.interval_ms for mode in ["lazy", "background"]]
    with args.output.open("x", encoding="utf-8") as stream:
        try:
            for trial in range(args.trials):
                for mode, interval in scenarios[::1 if trial % 2 == 0 else -1]:
                    command = base + ["--rebuild-mode", mode, "--query-interval-ms", str(interval),
                                      "--ann", "16", "100", "100"] + positional
                    result = subprocess.run([sys.executable, str(ROOT / "tests" / "measure_benchmark.py"),
                                             str(binary), *command], capture_output=True, text=True, check=True)
                    value = json.loads(result.stdout)
                    if value["profile"] != "release" or value["recall_at_k"] != 1:
                        raise ValueError("requires release binary and exact oracle agreement")
                    if mode == "background" and value["hnsw"]["rebuild_query_numbers"]:
                        raise ValueError("background benchmark silently rebuilt synchronously")
                    report["runs"].append({
                        "trial": trial + 1, "mode": mode, "interval_ms": interval,
                        "arguments": ["<dataset>/" + Path(v).name if v in files else v for v in command],
                        "measurement": value,
                    })
            if inputs(args.dataset)[0] != manifest or digest(binary) != report["binary_sha256"] or any(
                    digest(ROOT / name) != value for name, value in report["source_hashes"].items()):
                raise ValueError("inputs/source/binary changed during campaign")
            report["summary"] = summarize(report["runs"])
            report["status"] = "complete"
        except (OSError, ValueError, subprocess.CalledProcessError) as error:
            report["status"] = "failed"
            report["error"] = type(error).__name__
            if isinstance(error, subprocess.CalledProcessError):
                report["child_stderr"] = error.stderr
            raise
        finally:
            json.dump(report, stream, indent=2)
            stream.write("\n")


if __name__ == "__main__":
    main()
