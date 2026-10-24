"""Bounded paired float32/JSON conversion and end-to-end benchmark; no dependencies."""
import argparse
from array import array
import ctypes
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import shutil
import statistics
import time
import uuid

from vlite_embedded import NativeLibrary, _float32_snapshot


def bounded(low, high):
    def parse(text):
        value = int(text)
        if not low <= value <= high:
            raise argparse.ArgumentTypeError(f"must be {low}..{high}")
        return value
    return parse


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--library", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--rounds", type=bounded(1, 9), default=5)
    parser.add_argument("--queries", type=bounded(1, 500), default=80)
    parser.add_argument("--records", type=bounded(1, 2048), default=256)
    parser.add_argument("--dimensions", type=bounded(1, 1536), default=128)
    parser.add_argument("--bulk-rows", type=bounded(1, 512), default=128)
    args = parser.parse_args()
    if args.output.exists():
        parser.error("output must be a new file")
    if args.bulk_rows * args.dimensions > 131072:
        parser.error("bulk-rows * dimensions must be at most 131072 for a bounded JSON baseline")
    native = NativeLibrary(args.library)
    if not native.supports_float32:
        parser.error("benchmark requires both native float32 entry points")
    rng = random.Random(42)
    vectors = [array("f", (rng.uniform(-1, 1) for _ in range(args.dimensions)))
               for _ in range(max(args.records, args.bulk_rows))]
    # JSON and typed modes see exactly the same f32 values, IDs, k and metadata.
    lists = [list(vector) for vector in vectors]
    rows = [{"id": str(i)} for i in range(args.bulk_rows)]
    operations = [{"op": "put", **row, "vector": lists[i]} for i, row in enumerate(rows)]
    flat = array("f", (value for vector in vectors[:args.bulk_rows] for value in vector))
    query = vectors[0]
    json_query = {"op": "search", "vector": lists[0], "k": 10, "filter": []}
    typed_query = {"op": "search", "k": 10, "filter": [], "mode": "exact", "explain": False}
    json_bulk = {"op": "batch", "operations": operations}
    typed_bulk = {"dimensions": args.dimensions, "operations": rows}
    def encode(request):
        return ctypes.create_string_buffer(json.dumps(request, allow_nan=False).encode("utf-8"))
    samples = {name: {"json": [], "float32": []} for name in
               ("query_conversion", "bulk_conversion", "search", "bulk_memory", "bulk_durable")}
    def paired(name, actions, repeats, trial):
        modes = ("json", "float32") if trial % 2 == 0 else ("float32", "json")
        for mode in modes:
            start = time.perf_counter_ns()
            for _ in range(repeats):
                actions[mode]()
            samples[name][mode].append((time.perf_counter_ns() - start) / repeats)
    directory = args.output.parent / ("typed-benchmark-" + uuid.uuid4().hex)
    directory.mkdir(parents=True)
    try:
        with native.create_in_memory(args.dimensions) as search_db:
            for start in range(0, args.records, 64):
                search_db.batch([{"op": "put", "id": str(i), "vector": lists[i]}
                                 for i in range(start, min(start + 64, args.records))])
            expected = search_db.search(lists[0], 10)
            assert search_db.search_f32(query, 10, require_native=True) == expected
            for trial in range(args.rounds):
                paired("query_conversion", {"json": lambda: encode(json_query),
                       "float32": lambda: (_float32_snapshot(query), encode(typed_query))}, args.queries, trial)
                paired("bulk_conversion", {"json": lambda: encode(json_bulk),
                       "float32": lambda: (_float32_snapshot(flat), encode(typed_bulk))}, 5, trial)
                paired("search", {"json": lambda: search_db.search(lists[0], 10),
                       "float32": lambda: search_db.search_f32(query, 10, require_native=True)}, args.queries, trial)
                assert search_db.search(lists[0], 10) == expected
                assert search_db.search_f32(query, 10) == expected
                for name in ("bulk_memory", "bulk_durable"):
                    databases = {}
                    try:
                        for mode in ("json", "float32"):
                            databases[mode] = (native.create_in_memory(args.dimensions) if name == "bulk_memory"
                                               else native.create(directory / f"{trial}-{mode}.vlite", args.dimensions))
                        paired(name, {"json": lambda: databases["json"].batch(operations),
                               "float32": lambda: databases["float32"].batch_put_f32(
                                   rows, flat, args.dimensions, require_native=True)}, 1, trial)
                        for start in range(0, len(rows), 100):
                            ids = [row["id"] for row in rows[start:start + 100]]
                            assert databases["json"].get_many(ids) == databases["float32"].get_many(ids)
                    finally:
                        for db in databases.values():
                            db.close()
                    if name == "bulk_durable":
                        with native.open(directory / f"{trial}-json.vlite") as baseline, \
                                native.open(directory / f"{trial}-float32.vlite") as candidate:
                            for start in range(0, len(rows), 100):
                                ids = [row["id"] for row in rows[start:start + 100]]
                                assert baseline.get_many(ids) == candidate.get_many(ids)
    finally:
        shutil.rmtree(directory)
    summary = {}
    for name, modes in samples.items():
        baseline = statistics.median(modes["json"])
        candidate = statistics.median(modes["float32"])
        summary[name] = {"json_median_us": baseline / 1000, "float32_median_us": candidate / 1000,
                         "ratio_json_over_float32": baseline / candidate, "samples_ns": modes}
    report = {
        "verified": True, "workload": {key: value for key, value in vars(args).items()
                                      if key not in ("library", "output")},
        "environment": {"platform": platform.platform(), "python": platform.python_version(),
                        "cpu_count": os.cpu_count()},
        "library_sha256": hashlib.sha256(args.library.read_bytes()).hexdigest(),
        "source_sha256": {str(path.relative_to(Path(__file__).resolve().parents[2])): hashlib.sha256(path.read_bytes()).hexdigest()
                          for path in (Path(__file__).resolve(),
                                       Path(__file__).parent / "vlite_embedded" / "__init__.py",
                                       Path(__file__).resolve().parents[1] / "c" / "src" / "typed.rs",
                                       Path(__file__).resolve().parents[1] / "c" / "src" / "lib.rs")},
        "scope": "Paired same-library JSON baseline/typed candidate; f32-identical values; no index changes",
        "limitations": ["Shared host: other build/benchmark agents may contend for CPU; not isolated speedup evidence",
                        "Warm single-threaded reads; no RSS, cold-cache or concurrency throughput claims",
                        "Durable mode compares one identical atomic batch/sync, not per-record acknowledgments",
                        "Conversion includes stable snapshot/finite validation; baseline uses pre-existing Python lists",
                        "Bulk end-to-end timing excludes creation, validation reads and close"],
        "measurements": summary,
    }
    with args.output.open("x", encoding="utf-8") as stream:
        json.dump(report, stream, indent=2)
        stream.write("\n")
    print(json.dumps({name: {key: round(value, 3) for key, value in values.items()
                            if key != "samples_ns"} for name, values in summary.items()}, indent=2))


if __name__ == "__main__":
    main()
