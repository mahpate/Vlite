"""Measure real-embedding cosine recall, latency, deletes and rebuild pauses."""
import argparse
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
FILES = {"vectors.txt", "queries.txt", "updates.txt"}


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def inputs(directory):
    manifest = json.loads((directory / "dataset-manifest.json").read_text(encoding="utf-8"))
    entries = manifest["files"]
    if len(entries) != 3 or {entry["name"] for entry in entries} != FILES:
        raise ValueError("manifest must name exactly vectors.txt, queries.txt, updates.txt")
    if not all(isinstance(manifest.get(key), str) and manifest[key] for key in
               ["dataset", "source_url", "source_sha256", "license", "license_url",
                "model", "normalization", "split_policy"]):
        raise ValueError("dataset/model/license/split provenance is required")
    counts = {}
    dimensions = manifest["dimensions"]
    if type(dimensions) is not int or not 1 <= dimensions <= 1024:
        raise ValueError("dimensions must be 1..1024")
    for entry in entries:
        name = entry["name"]
        path = directory / name
        if path.stat().st_size > 64 * 1024 * 1024:
            raise ValueError("embedding text exceeds 64 MiB")
        if digest(path) != entry["sha256"]:
            raise ValueError(f"dataset hash mismatch: {name}")
        rows = entry["rows"]
        if type(rows) is not int or not 1 <= rows <= 25600:
            raise ValueError("invalid manifest row count")
        # Native child validates components and row counts before database creation.
        counts[name] = rows
    if not (10 <= counts["vectors.txt"] <= 10000 and counts["queries.txt"] <= 100):
        raise ValueError("campaign requires 10..10000 base and 1..100 query rows")
    if counts["vectors.txt"] * counts["queries.txt"] * dimensions > 500_000_000:
        raise ValueError("campaign exceeds 500 million distance components")
    return manifest, counts


def summarize(runs, target):
    groups = {}
    for run in runs:
        groups.setdefault(run["scenario"], []).append(run["measurement"])
    rows = []
    for scenario, values in sorted(groups.items()):
        row = {
            "scenario": scenario, "trials": len(values),
            "median_trial_exact_p50_ms": statistics.median(v["query_p50_ms"] for v in values),
            "median_whole_child_peak_rss_bytes": statistics.median(v["process_peak_rss_bytes"] for v in values),
        }
        if "hnsw" in values[0]:
            ann = [v["hnsw"] for v in values]
            row.update({
                "minimum_trial_mean_recall": min(v["recall_at_k"] for v in ann),
                "meets_target_every_trial": all(v["recall_at_k"] >= target for v in ann),
                "median_trial_ann_p50_ms": statistics.median(v["query_p50_ms"] for v in ann),
                "median_trial_ann_p95_ms": statistics.median(v["query_p95_ms"] for v in ann),
                "median_trial_ann_p99_ms": statistics.median(v["query_p99_ms"] for v in ann),
                "maximum_query_ms": max(max(v["query_samples_ms"]) for v in ann),
                "median_index_build_ms": statistics.median(v["index_build_ms"] for v in ann),
                "rebuilds_per_trial": [len(v["rebuild_query_numbers"]) for v in ann],
                "median_file_growth_bytes": statistics.median(v["final_file_bytes"] - v["file_bytes"] for v in values),
            })
            steady = [sample for v in ann for i, sample in enumerate(v["query_samples_ms"], 1)
                      if i not in v["rebuild_query_numbers"]]
            row["median_non_rebuild_query_ms"] = statistics.median(steady) if steady else None
        rows.append(row)
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--dataset", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--ef", type=int, nargs="+", default=[32, 100, 300])
    parser.add_argument("--writes", type=int, default=16)
    parser.add_argument("--deletes", type=int, default=4)
    parser.add_argument("--recall-target", type=float, default=.99)
    args = parser.parse_args()
    if not (1 <= args.trials <= 3 and 1 <= len(args.ef) <= 4
            and len(set(args.ef)) == len(args.ef) and all(10 <= ef <= 100000 for ef in args.ef)
            and 1 <= args.writes <= 255 and 1 <= args.deletes <= 255
            and args.writes + args.deletes <= 256
            and math.isfinite(args.recall_target) and 0 < args.recall_target <= 1):
        parser.error("invalid bounded campaign arguments")
    manifest, counts = inputs(args.dataset)
    if (counts["updates.txt"] != counts["queries.txt"] * args.writes
            or args.writes + args.deletes > counts["vectors.txt"]):
        parser.error("update rows must equal query rows * writes, and mutation width cannot exceed base rows")
    binary = args.binary.resolve(strict=True)
    scenarios = [("exact", []), ("filtered-10pct", ["--filter-every", "10"])]
    scenarios += [(f"ann-ef-{ef}-{mode}", mixed + ["--ann", "16", "100", str(ef)])
                  for ef in args.ef
                  for mode, mixed in [
                      ("static", []),
                      ("mixed", ["--update-vectors", str((args.dataset / "updates.txt").resolve()),
                                 "--mixed-writes", str(args.writes), "--mixed-deletes", str(args.deletes)])]]
    report = {
        "schema_version": 1, "status": "running",
        "utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "platform": platform.platform(), "machine": platform.machine(), "logical_cpus": os.cpu_count(),
        "python_version": platform.python_version(),
        "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "source_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT)),
        "binary_sha256": digest(binary),
        "source_hashes": {name: digest(ROOT / name) for name in [
            "src/lib.rs", "src/hnsw.rs", "src/bin/vlite-bench.rs",
            "benchmarks/embedding_campaign.py", "benchmarks/prepare_glove.py", "tests/measure_benchmark.py"]},
        "dataset": manifest,
        "config": {"trials": args.trials, "ef": args.ef, "writes": args.writes, "deletes": args.deletes,
                   "recall_target": args.recall_target, "k": 10, "seed": 42},
        "limitations": "Bounded subset, not full ANN-Benchmarks or modern sentence embeddings. Shared unpinned host; whole-child RSS, not marginal-index RAM. Mixed query times include synchronous rebuilds. No general recall/latency guarantee.",
        "runs": [],
    }
    base = ["--metric", "cosine", "--vectors", str((args.dataset / "vectors.txt").resolve()),
            "--query-vectors", str((args.dataset / "queries.txt").resolve())]
    positional = [str(counts["vectors.txt"]), str(manifest["dimensions"]), str(counts["queries.txt"]), "100", "10", "42"]
    with args.output.open("x", encoding="utf-8") as stream:
        try:
            for trial in range(args.trials):
                for scenario, flags in scenarios[::1 if trial % 2 == 0 else -1]:
                    command = base + flags + positional
                    child = subprocess.run([sys.executable, str(ROOT / "tests" / "measure_benchmark.py"),
                                            str(binary), *command], capture_output=True, text=True, check=True)
                    value = json.loads(child.stdout)
                    if value["profile"] != "release" or value["recall_at_k"] != 1:
                        raise ValueError("requires release binary and exact oracle agreement")
                    # Do not publish local filesystem paths.
                    input_paths = {base[3], base[5], str((args.dataset / "updates.txt").resolve())}
                    public_args = ["<dataset>/" + Path(v).name if v in input_paths else v
                                   for v in command]
                    report["runs"].append({"trial": trial + 1, "scenario": scenario,
                                           "arguments": public_args, "measurement": value})
            if inputs(args.dataset)[0] != manifest:
                raise ValueError("dataset provenance changed during campaign")
            if digest(binary) != report["binary_sha256"] or any(
                    digest(ROOT / name) != value for name, value in report["source_hashes"].items()):
                raise ValueError("benchmark binary or recorded source changed during campaign")
            report["summary"] = summarize(report["runs"], args.recall_target)
            report["status"] = "complete"
        except (subprocess.CalledProcessError, ValueError, OSError) as error:
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
