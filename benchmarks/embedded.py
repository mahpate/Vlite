"""Measure Python embedded calls and batching against the same native library."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import statistics
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "bindings" / "python"))
from vlite_embedded import NativeLibrary  # noqa: E402


def bounded(low, high):
    def parse(text):
        number = int(text)
        if not low <= number <= high:
            raise argparse.ArgumentTypeError(f"must be {low}..{high}")
        return number
    return parse


def measure(action):
    start = time.perf_counter_ns()
    action()
    return time.perf_counter_ns() - start


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--library", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--records", type=bounded(1, 4096), default=128)
    parser.add_argument("--dimensions", type=bounded(1, 1024), default=32)
    parser.add_argument("--batch-size", type=bounded(1, 100), default=32)
    parser.add_argument("--trials", type=bounded(1, 10), default=3)
    parser.add_argument("--read-passes", type=bounded(1, 100), default=10)
    args = parser.parse_args()
    if args.output.exists():
        parser.error("output must be a new file")
    native = NativeLibrary(args.library)
    rng = random.Random(42)
    # Fixed small integers are exactly representable in both f32 and Python.
    records = [{"op": "put", "id": str(i),
                "vector": [rng.randrange(-10, 11) for _ in range(args.dimensions)]}
               for i in range(args.records)]
    ids = [record["id"] for record in records]
    query = records[0]["vector"]
    truth = sorted(((sum((a-b)**2 for a, b in zip(query, record["vector"])), int(record["id"]))
                    for record in records))[:min(10, args.records)]
    samples = {name: [] for name in ["put", "batch_put", "get", "get_many"]}
    with tempfile.TemporaryDirectory(prefix="vlite-embedded-bench-") as directory:
        for trial in range(args.trials):
            # Alternate mode order to reduce systematic warm-cache/order bias.
            modes = ["put", "batch_put"] if trial % 2 == 0 else ["batch_put", "put"]
            for mode in modes:
                with native.create(Path(directory) / f"{trial}-{mode}.vlite", args.dimensions) as db:
                    def write():
                        if mode == "put":
                            for record in records:
                                db.put(record["id"], record["vector"])
                        else:
                            for start in range(0, args.records, args.batch_size):
                                db.batch(records[start:start + args.batch_size])
                    samples[mode].append(measure(write))
                    assert db.info()["records"] == args.records
                    hits = db.search(query, 10)
                    assert [(hit["distance"], int(hit["id"])) for hit in hits] == truth
                    if mode != "put":
                        continue
                    def read_single():
                        return [db.get(id) for id in ids]
                    def read_many():
                        return [record for start in range(0, args.records, args.batch_size)
                                for record in db.get_many(ids[start:start + args.batch_size])]
                    # Warm and verify both paths before recording timed, verified passes.
                    expected = read_single()
                    assert read_many() == expected
                    for index in range(args.read_passes):
                        readers = [("get", read_single), ("get_many", read_many)]
                        if index % 2:
                            readers.reverse()
                        for name, read in readers:
                            started = time.perf_counter_ns()
                            result = read()
                            samples[name].append(time.perf_counter_ns() - started)
                            assert result == expected
    report = {
        "schema_version": 1,
        "scope": "Python ctypes JSON ABI, single process/thread; cached reads; synchronous durable writes",
        "limits": [
            "batch_put amortizes sync across each atomic batch; it is not per-record acknowledgment",
            "get_many reduces calls, but JSON encoding/copying can outweigh that benefit",
            "No cross-product, concurrent-load, cold-read or power-loss claims",
            "Timing excludes correctness comparisons, creation/open/close and warmups",
        ],
        "environment": {"platform": platform.platform(), "machine": platform.machine(),
                        "python": platform.python_version(), "cpu_count": os.cpu_count()},
        "library_sha256": hashlib.sha256(args.library.read_bytes()).hexdigest(),
        "source_sha256": {name: hashlib.sha256((ROOT / name).read_bytes()).hexdigest()
                          for name in ["Cargo.toml", "Cargo.lock",
                                       "src/lib.rs", "src/format.rs", "src/hnsw.rs", "src/metadata.rs",
                                       "bindings/c/Cargo.toml", "bindings/c/src/lib.rs",
                                       "bindings/python/vlite_embedded/__init__.py",
                                       "benchmarks/embedded.py"]},
        "workload": {"records": args.records, "dimensions": args.dimensions,
                     "batch_size": args.batch_size, "trials": args.trials,
                     "read_passes": args.read_passes, "seed": 42},
        "verified": True,
        "measurements": {name: {"elapsed_ns": values,
                               "median_records_per_second": args.records * 1e9 / statistics.median(values)}
                         for name, values in samples.items()},
    }
    with args.output.open("x", encoding="utf-8") as stream:
        json.dump(report, stream, indent=2, allow_nan=False)
        stream.write("\n")
    print(json.dumps({name: round(data["median_records_per_second"], 2)
                      for name, data in report["measurements"].items()}))


if __name__ == "__main__":
    main()
