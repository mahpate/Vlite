"""Record one bounded search-capabilities release run with provenance."""
import argparse
import hashlib
import json
import math
import platform
from pathlib import Path
import subprocess
import sys
from datetime import datetime, timezone


def validate(report):
    if report.get("schema") != 1 or report.get("queries", 0) <= 0:
        raise ValueError("invalid benchmark schema or query count")
    if not isinstance(report.get("k"), int) or report["k"] <= 0:
        raise ValueError("invalid top-k")
    rows = report["rows"]
    expected = {
        (metric, quantization)
        for metric in ("squared_l2", "cosine")
        for quantization in ("none", "scalar_u8")
    }
    if len(rows) != 4 or {(r["metric"], r["quantization"]) for r in rows} != expected:
        raise ValueError("missing or duplicated metric/quantization rows")
    for row in rows:
        recall = row["recall_at_k"]
        if not math.isfinite(recall) or not 0 <= recall <= 1:
            raise ValueError("invalid recall")
        if row["recall_denominator"] != report["queries"] * report["k"]:
            raise ValueError("unexpected recall denominator")
        for field in ("query_samples_ms", "exact_samples_ms"):
            samples = row[field]
            if len(samples) != report["queries"] or any(
                not math.isfinite(sample) or sample < 0 for sample in samples
            ):
                raise ValueError("invalid timing samples")
        for field in (
            "graph_link_bytes", "code_bytes", "codebook_bytes",
            "buffer_vector_bytes", "resident_vector_bytes",
        ):
            if not isinstance(row[field], int) or row[field] < 0:
                raise ValueError("invalid component memory count")
        if row["quantization"] == "scalar_u8" and (
            row["code_bytes"] <= 0 or row["quantized_distance_evaluations"] <= 0
        ):
            raise ValueError("quantized path did not execute")
        if row["quantization"] == "none" and (
            row["code_bytes"] != 0 or row["codebook_bytes"] != 0
            or row["quantized_distance_evaluations"] != 0
        ):
            raise ValueError("unquantized row contains quantized work")
    index = report["metadata_index"]
    if (
        index["strategy"] != "exact_indexed"
        or index["identical_results"] is not True
        or not 0 < index["indexed_records"] < index["scan_records"]
    ):
        raise ValueError("indexed search did not narrow the exact workload")


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    if args.output.exists():
        parser.error("output must not already exist")
    root = Path(__file__).resolve().parents[1]
    paths = [
        Path("examples/search_capabilities_bench.rs"),
        Path("src/hnsw.rs"), Path("src/quantize.rs"),
        Path("src/planner.rs"), Path("src/metadata_index.rs"),
        Path("src/lib.rs"), Path("src/quality.rs"),
        Path("src/collection.rs"), Path("src/rebuild.rs"), Path("src/stats.rs"),
        Path("src/format.rs"), Path("src/metadata.rs"),
        Path("Cargo.toml"), Path("Cargo.lock"),
    ]
    source_hashes = {path.as_posix(): sha256(root / path) for path in paths}
    binary_hash = sha256(binary)
    process = subprocess.run(
        [str(binary)], capture_output=True, text=True, check=True, timeout=300,
    )
    report = json.loads(process.stdout)
    validate(report)
    if source_hashes != {
        path.as_posix(): sha256(root / path) for path in paths
    } or binary_hash != sha256(binary):
        raise RuntimeError("source or binary changed during campaign; discard run")
    report["provenance"] = {
        "created_utc": datetime.now(timezone.utc).isoformat(),
        "platform": platform.platform(),
        "python": platform.python_version(),
        "binary_sha256": binary_hash,
        "source_sha256": source_hashes,
        "limitations": [
            "One synthetic clustered workload; not real embedding quality.",
            "Single release child on a shared unpinned host; not production tails.",
            "Component byte accounting excludes allocator and process overhead.",
            "Resident f32 vectors remain in memory; no total-RAM savings claim.",
            "Exact baseline precedes ANN; order and cache effects are not controlled.",
        ],
    }
    with args.output.open("x", encoding="utf-8") as output:
        json.dump(report, output, indent=2, allow_nan=False)
        output.write("\n")
    print(f"Recorded validated search-capabilities run: {args.output}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        sys.exit(1)
