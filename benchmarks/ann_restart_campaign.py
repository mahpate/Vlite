"""Measure validated snapshot restart and rebuild on a licensed embedding set."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import platform
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def validate(value):
    if value.get("benchmark") != "ann_restart" or value.get("profile") != "release":
        raise ValueError("expected release restart benchmark")
    if (value.get("exact_hits_match") is not True
            or value.get("rebuild_hits_equal_pre_save") is not True
            or value.get("mismatched_queries") != 0
            or value.get("load_outcome") != "loaded"):
        raise ValueError("snapshot/rebuild results differ")
    times = value["timings_ns"]
    for key in ("reopen", "load", "restart_with_snapshot", "save", "initial_build"):
        if type(times[key]) is not int or times[key] < 0:
            raise ValueError("invalid timing")
    if not times["rebuild_trials"]:
        raise ValueError("missing rebuild trials")
    for trial in times["rebuild_trials"]:
        if any(type(v) is not int or v < 0 for v in trial.values()):
            raise ValueError("invalid rebuild timing")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--dataset", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if args.output.exists():
        parser.error("output must not exist")
    manifest_path = args.dataset / "dataset-manifest.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    entries = {entry["name"]: entry for entry in manifest["files"]}
    for key in ("license", "license_url", "source_sha256", "model"):
        if not manifest.get(key):
            raise ValueError("missing dataset provenance")
    for name in ("vectors.txt", "queries.txt"):
        if digest(args.dataset / name) != entries[name]["sha256"]:
            raise ValueError("dataset hash mismatch")
    counts = entries["vectors.txt"]["rows"], entries["queries.txt"]["rows"]
    if not (10 <= counts[0] <= 10000 and 1 <= counts[1] <= 100):
        raise ValueError("bounded campaign requires 10..10000 vectors, 1..100 queries")
    sources = [
        "src/ann_snapshot.rs", "src/hnsw.rs", "src/quantize.rs", "src/lib.rs",
        "examples/ann_restart_bench.rs", "benchmarks/ann_restart_campaign.py",
        "Cargo.toml", "Cargo.lock",
    ]
    hashes = {name: digest(ROOT / name) for name in sources}
    binary = args.binary.resolve(strict=True)
    binary_hash = digest(binary)
    rows = []
    for quantization in ("none", "u8"):
        command = [
            str(binary), "--vectors", str((args.dataset / "vectors.txt").resolve()),
            "--queries", str((args.dataset / "queries.txt").resolve()),
            "--manifest", str(manifest_path.resolve()),
            "--count", str(counts[0]), "--query-count", str(counts[1]),
            "--dims", str(manifest["dimensions"]), "--metric", "cosine",
            "--quantization", quantization, "--ef", "100", "--rebuild-trials", "3",
        ]
        child = subprocess.run(command, check=True, capture_output=True, text=True, timeout=300)
        value = json.loads(child.stdout)
        validate(value)
        for item in value["provenance"]:
            item["path"] = "<dataset>/" + Path(item["path"]).name
        rows.append(value)
    if hashes != {name: digest(ROOT / name) for name in sources} or digest(binary) != binary_hash:
        raise ValueError("sources changed during measurement")
    report = {
        "schema": 1, "created_utc": datetime.now(timezone.utc).isoformat(),
        "platform": platform.platform(), "dataset": manifest, "rows": rows,
        "source_sha256": hashes, "binary_sha256": binary_hash,
        "limitations": [
            "One bounded GloVe subset, not modern sentence embeddings or full ANN-Benchmarks.",
            "Shared unpinned host; warm-cache reopen timings, not cold-machine startup.",
            "Snapshot checksum/content validation remains necessary; primary log still replays.",
            "Repeated rebuilds in one child; no process RSS or production tail claim.",
        ],
    }
    with args.output.open("x", encoding="utf-8") as stream:
        json.dump(report, stream, indent=2, allow_nan=False)
        stream.write("\n")
    print("Recorded validated restart measurements:", args.output)


if __name__ == "__main__":
    main()
