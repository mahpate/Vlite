"""Paired native revision comparison using isolated, prebuilt release binaries.

Run after all builds finish, on an otherwise quiet host:
python benchmarks/revision_campaign.py --variant tag SOURCE BINARY FEATURES \
    --variant current SOURCE BINARY FEATURES --output new-report.json
SOURCE must be an archive of --commit recorded in a source-commit.txt file.
FEATURES is a descriptive build-feature label, e.g. default or graph.
"""

import argparse
import datetime
import hashlib
import json
from pathlib import Path
import platform
import statistics
import subprocess
import sys
import time


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def cpu_sample():
    values = [int(v) for v in Path("/proc/stat").read_text().splitlines()[0].split()[1:9]]
    return sum(values), values[3] + values[4]


def quiet_cpu():
    before = cpu_sample()
    time.sleep(2)
    after = cpu_sample()
    total, idle = after[0] - before[0], after[1] - before[1]
    return 100 * (total - idle) / total if total else None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--variant", nargs=4, action="append", required=True,
                        metavar=("LABEL", "SOURCE", "BINARY", "FEATURES"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--trials", type=int, default=5)
    parser.add_argument("--records", type=int, default=4096)
    parser.add_argument("--dimensions", type=int, default=128)
    parser.add_argument("--queries", type=int, default=100)
    args = parser.parse_args()
    if not 2 <= len(args.variant) <= 4 or not 3 <= args.trials <= 10:
        parser.error("use 2..4 variants and 3..10 repetitions")
    if min(args.records, args.dimensions, args.queries) < 1:
        parser.error("positive workload sizes required")
    report = {
        "schema_version": 1, "status": "running",
        "utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "platform": platform.platform(), "cpu": platform.processor(),
        "cpu_info": next((line.split(":", 1)[1].strip() for line in
                         Path("/proc/cpuinfo").read_text().splitlines() if line.startswith("model name")), ""),
        "compiler": subprocess.check_output(["/root/.cargo/bin/rustc", "-Vv"], text=True).strip(),
        "config": {"records": args.records, "dimensions": args.dimensions,
                   "queries": args.queries, "trials": args.trials, "k": 10,
                   "seed": 42, "m": 16, "ef_construction": 100, "ef_search": 100,
                   "batch_size": 100, "mixed_writes": [0, 16]},
        "runner_sha256": digest(Path(__file__)),
        "limitations": [
            "Linux/WSL on a shared Windows host; no affinity, cold-cache or production-tail claim.",
            "Synthetic uniform vectors; not modern embeddings or a cross-engine comparison.",
            "Whole-child peak RSS includes dataset, oracle, database and index.",
            "Mixed ANN lazily rebuilds synchronously; rebuild stalls remain in query samples.",
            "Graph-enabled variant uses vector records, not graph traversal; graph overhead only.",
            "Pre-child CPU readings cover WSL, not all Windows-host activity.",
            "Fixed exact-before-ANN order within each child; variants rotate between repetitions.",
        ], "variants": [], "runs": [],
    }
    for label, source, binary, features in args.variant:
        source, binary = Path(source).resolve(), Path(binary).resolve()
        report["variants"].append({
            "label": label, "source_commit": (source / "source-commit.txt").read_text().strip(),
            "features": features, "binary_sha256": digest(binary),
            "source_hashes": {name: digest(source / name) for name in
                              ("Cargo.lock", "src/lib.rs", "src/hnsw.rs", "src/bin/vlite-bench.rs")},
        })
    with args.output.open("x", encoding="utf-8") as stream:
        def persist():
            stream.seek(0)
            json.dump(report, stream, indent=2, allow_nan=False)
            stream.truncate()
            stream.flush()

        try:
            for trial in range(args.trials):
                order = args.variant[trial % len(args.variant):] + args.variant[:trial % len(args.variant)]
                for writes in ([0, 16] if trial % 2 == 0 else [16, 0]):
                    for label, source, binary, _ in order:
                        flags = ["--mixed-writes", str(writes)] if writes else []
                        flags += ["--ann", "16", "100", "100", str(args.records),
                                  str(args.dimensions), str(args.queries), "100", "10", "42"]
                        ambient = quiet_cpu()
                        start = time.monotonic()
                        child = subprocess.run([sys.executable, str(Path(source) / "tests/measure_benchmark.py"),
                                                binary, *flags], capture_output=True, text=True,
                                               check=True, timeout=600)
                        measurement = json.loads(child.stdout)
                        if measurement["profile"] != "release":
                            raise ValueError("release binary required")
                        report["runs"].append({
                            "trial": trial + 1, "variant": label, "mixed_writes": writes,
                            "pre_child_wsl_cpu_percent": ambient, "arguments": flags,
                            "elapsed_seconds": time.monotonic() - start, "measurement": measurement,
                        })
                        persist()
                        print(f"{trial + 1}/{args.trials} {label} writes={writes}: "
                              f"recall={measurement['hnsw']['recall_at_k']}", flush=True)
            checksums = {(r["measurement"]["dataset_checksum"], r["measurement"]["query_checksum"])
                         for r in report["runs"]}
            if len(checksums) != 1:
                raise ValueError("variant datasets differ")
            report["summary"] = []
            for variant in report["variants"]:
                for writes in (0, 16):
                    samples = [r["measurement"] for r in report["runs"]
                               if r["variant"] == variant["label"] and r["mixed_writes"] == writes]
                    row = {"variant": variant["label"], "mixed_writes": writes,
                           "trials": len(samples),
                           "minimum_recall_at_k": min(s["hnsw"]["recall_at_k"] for s in samples)}
                    for key in ("query_p50_ms", "query_p95_ms", "query_p99_ms",
                                "ingest_vectors_per_second", "reopen_ms", "process_peak_rss_bytes"):
                        row["median_" + key] = statistics.median(s[key] for s in samples)
                    for key in ("index_build_ms", "query_p50_ms", "query_p95_ms", "query_p99_ms"):
                        row["median_ann_" + key] = statistics.median(s["hnsw"][key] for s in samples)
                    write_samples = [v for s in samples for v in s["hnsw"]["write_batch_samples_ms"]]
                    row["pooled_write_batch_p50_ms"] = statistics.median(write_samples) if write_samples else None
                    report["summary"].append(row)
            report["status"] = "complete"
        except (OSError, subprocess.SubprocessError, ValueError, KeyError) as error:
            report["status"] = "failed"
            report["error"] = type(error).__name__
            raise
        finally:
            persist()


if __name__ == "__main__":
    main()
