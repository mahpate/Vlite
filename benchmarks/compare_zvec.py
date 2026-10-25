"""Compare embedded VLite and zvec in-process through their Python packages.

Every (engine, mode) pair runs in a fresh child process on identical vectors and
queries, so ingest time, index build, latency, disk footprint and peak RSS are
isolated per engine and per index type. Exact (flat) and HNSW results are
checked against a float64 NumPy oracle outside timing. Engine order alternates
between trials. Run on Linux/WSL (zvec publishes Linux/macOS wheels):

    python3 -m venv .venv && . .venv/bin/activate
    pip install zvec numpy vlite_embedded-<version>-py3-none-manylinux_2_28_x86_64.whl
    python benchmarks/compare_zvec.py --records 20000 --dimensions 128 --output zvec.json
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import shutil
import subprocess
import sys
import tempfile
import time

import numpy as np

HNSW_M, HNSW_EF_CONSTRUCTION = 16, 100  # VLite's defaults, applied to zvec explicitly.
BATCH = 1000


def batch_size(dimensions):
    # Both engines use the same size; keeps VLite JSON requests well under its 8 MiB limit.
    return max(1, min(BATCH, 4_000_000 // (dimensions * 24)))


def percentile(samples, fraction):
    ordered = sorted(samples)
    return ordered[max(0, min(len(ordered) - 1, int(np.ceil(fraction * len(ordered))) - 1))]


def summary(samples_ns):
    return {
        "p50_ms": percentile(samples_ns, 0.50) / 1e6,
        "p95_ms": percentile(samples_ns, 0.95) / 1e6,
        "p99_ms": percentile(samples_ns, 0.99) / 1e6,
        "mean_ms": float(np.mean(samples_ns)) / 1e6,
        "sequential_qps": len(samples_ns) / (sum(samples_ns) / 1e9),
    }


def directory_bytes(path):
    path = Path(path)
    if path.is_file():
        return path.stat().st_size
    return sum(p.stat().st_size for p in path.rglob("*") if p.is_file())


def dist_version(name):
    try:
        from importlib.metadata import version
        return version(name)
    except Exception:
        return None


class VLiteEngine:
    """Each batch is a durable atomic commit (fsync before acknowledgment)."""

    def __init__(self, workdir, dimensions, mode):
        from vlite_embedded import NativeLibrary
        self.version = dist_version("vlite-embedded") or dist_version("vlite_embedded")
        self.mode = mode
        self.path = Path(workdir) / "bench.vlite"
        self.library = NativeLibrary()
        # Float32 buffers skip per-component JSON; older wheels fall back to JSON lists.
        self.typed = getattr(self.library, "supports_float32", False)
        self.transport = "float32" if self.typed else "json"
        self.db = self.library.create(self.path, dimensions)

    def ingest(self, vectors):
        size = batch_size(vectors.shape[1])
        for start in range(0, len(vectors), size):
            rows = vectors[start:start + size]
            if self.typed:
                self.db.batch_put_f32([{"id": str(start + i)} for i in range(len(rows))],
                                      np.ascontiguousarray(rows, dtype=np.float32),
                                      vectors.shape[1], require_native=True)
            else:
                self.db.batch([{"op": "put", "id": str(start + i), "vector": row}
                               for i, row in enumerate(rows.tolist())])

    def build(self):
        if self.mode == "ann":
            self.db.build_index()

    def search(self, query, k, ef):
        if self.typed:
            vector = np.asarray(query, dtype=np.float32)
            if self.mode == "exact":
                hits = self.db.search_f32(vector, k, require_native=True)
            else:
                hits = self.db.search_f32(vector, k, mode="approximate_ready", ef_search=ef,
                                          require_native=True)
        elif self.mode == "exact":
            hits = self.db.search(query, k)
        else:
            hits = self.db.search_approximate_ready(query, k, ef_search=ef)
        return [int(hit["id"]) for hit in hits]

    def close(self):
        self.db.close()

    def reopen(self):
        self.db = self.library.open(self.path)

    def first_query_after_reopen(self, query, k, ef):
        # The HNSW graph is in memory only (snapshots are opt-in), so ANN mode
        # rebuilds here; the measured time includes log replay and rebuild.
        self.build()
        return self.search(query, k, ef)

    def footprint(self):
        return directory_bytes(self.path.parent)


class ZvecEngine:
    def __init__(self, workdir, dimensions, mode):
        import zvec
        self.zvec = zvec
        self.version = dist_version("zvec")
        self.mode = mode
        self.root = Path(workdir) / "collection"
        index = zvec.FlatIndexParam(metric_type=zvec.MetricType.L2) if mode == "exact" else \
            zvec.HnswIndexParam(metric_type=zvec.MetricType.L2, m=HNSW_M, ef_construction=HNSW_EF_CONSTRUCTION)
        schema = zvec.CollectionSchema(
            name="bench",
            vectors=[zvec.VectorSchema(name="v", data_type=zvec.DataType.VECTOR_FP32,
                                       dimension=dimensions, index_param=index)])
        self.collection = zvec.create_and_open(path=str(self.root), schema=schema)

    def ingest(self, vectors):
        size = batch_size(vectors.shape[1])
        for start in range(0, len(vectors), size):
            self.collection.insert([self.zvec.Doc(id=str(start + i), vectors={"v": row})
                                    for i, row in enumerate(vectors[start:start + size].tolist())])

    def build(self):
        self.collection.optimize()

    def search(self, query, k, ef):
        param = None if self.mode == "exact" else self.zvec.HnswQueryParam(ef=ef)
        result = self.collection.query(queries=self.zvec.Query(field_name="v", vector=query, param=param), topk=k)
        return [int(doc.id) for doc in result]

    def close(self):
        if hasattr(self.collection, "flush"):
            self.collection.flush()
        self.collection = None

    def reopen(self):
        self.collection = self.zvec.open(path=str(self.root))

    def first_query_after_reopen(self, query, k, ef):
        return self.search(query, k, ef)

    def footprint(self):
        return directory_bytes(self.root)


ENGINES = {"vlite": VLiteEngine, "zvec": ZvecEngine}


def current_rss():
    with open("/proc/self/status") as status:
        for line in status:
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) * 1024
    return 0


def run_child(args):
    data = np.load(args.data)
    vectors, queries = data["vectors"], data["queries"]
    query_lists = queries.tolist()
    workdir = tempfile.mkdtemp(prefix=f"bench-{args.engine}-")
    try:
        baseline_rss = current_rss()
        engine = ENGINES[args.engine](workdir, vectors.shape[1], args.mode)
        result = {"engine": args.engine, "mode": args.mode, "version": engine.version,
                  "vector_transport": getattr(engine, "transport", "python_list")}
        start = time.perf_counter_ns()
        engine.ingest(vectors)
        result["ingest_s"] = (time.perf_counter_ns() - start) / 1e9
        start = time.perf_counter_ns()
        engine.build()
        result["index_build_s"] = (time.perf_counter_ns() - start) / 1e9
        result["rss_growth_bytes"] = current_rss() - baseline_rss
        for query in query_lists[:args.warmup]:
            engine.search(query, args.k, args.ef)
        samples, ids = [], []
        for query in query_lists:
            begin = time.perf_counter_ns()
            found = engine.search(query, args.k, args.ef)
            samples.append(time.perf_counter_ns() - begin)
            ids.append(found)
        result.update(summary(samples), samples_ns=samples, ids=ids)
        engine.close()
        result["footprint_bytes"] = engine.footprint()
        start = time.perf_counter_ns()
        engine.reopen()
        engine.first_query_after_reopen(query_lists[0], args.k, args.ef)
        result["reopen_first_query_s"] = (time.perf_counter_ns() - start) / 1e9
        engine.close()
        result["peak_rss_bytes"] = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * 1024
        print(json.dumps(result))
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def recall(found, truth_dist, distances_row, k):
    """Tie-aware recall@k: a returned ID counts if within the kth exact distance."""
    cutoff = truth_dist[k - 1] * (1 + 1e-5) + 1e-6
    unique = list(dict.fromkeys(found[:k]))
    return sum(1 for i in unique if 0 <= i < len(distances_row) and distances_row[i] <= cutoff) / k


def make_dataset(args):
    rng = np.random.default_rng(args.seed)
    if args.clusters:
        centers = rng.normal(0, 1, (args.clusters, args.dimensions))
        vectors = centers[rng.integers(0, args.clusters, args.records)] + \
            rng.normal(0, args.spread, (args.records, args.dimensions))
        queries = centers[rng.integers(0, args.clusters, args.queries)] + \
            rng.normal(0, args.spread, (args.queries, args.dimensions))
    else:
        vectors = rng.random((args.records, args.dimensions))
        queries = rng.random((args.queries, args.dimensions))
    return vectors.astype(np.float32), queries.astype(np.float32)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--child", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--engine", choices=sorted(ENGINES), help=argparse.SUPPRESS)
    parser.add_argument("--mode", choices=["exact", "ann"], help=argparse.SUPPRESS)
    parser.add_argument("--data", help=argparse.SUPPRESS)
    parser.add_argument("--records", type=int, default=20000)
    parser.add_argument("--dimensions", type=int, default=128)
    parser.add_argument("--queries", type=int, default=200)
    parser.add_argument("--k", type=int, default=10)
    parser.add_argument("--ef", type=int, default=100)
    parser.add_argument("--warmup", type=int, default=10)
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--clusters", type=int, default=64, help="Gaussian clusters; 0 for uniform vectors")
    parser.add_argument("--spread", type=float, default=0.35, help="cluster standard deviation")
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.child:
        return run_child(args)
    if args.output is None or args.output.exists():
        parser.error("--output must be a new file")

    vectors, queries = make_dataset(args)
    checksum = hashlib.sha256(vectors.tobytes() + queries.tobytes()).hexdigest()
    v64, q64 = vectors.astype(np.float64), queries.astype(np.float64)
    distances = (q64 ** 2).sum(1)[:, None] - 2 * q64 @ v64.T + (v64 ** 2).sum(1)[None, :]
    truth = np.argsort(distances, axis=1, kind="stable")[:, :args.k]
    truth_dist = np.take_along_axis(distances, truth, axis=1)

    report = {
        "benchmark": "vlite-vs-zvec-embedded-python",
        "workload": {k: v for k, v in vars(args).items() if k not in ("child", "engine", "mode", "data", "output")},
        "ingest_batch": batch_size(args.dimensions),
        "hnsw": {"m": HNSW_M, "ef_construction": HNSW_EF_CONSTRUCTION, "ef_search": args.ef},
        "metric": "L2; recall is ID-based and tie-aware against a float64 oracle",
        "dataset_sha256": checksum,
        "host": {"platform": platform.platform(), "python": platform.python_version(),
                 "cpu_count": os.cpu_count(), "machine": platform.machine()},
        "runs": [],
    }
    with tempfile.TemporaryDirectory(prefix="vlite-zvec-") as directory:
        data_path = Path(directory) / "data.npz"
        np.savez(data_path, vectors=vectors, queries=queries)
        for trial in range(args.trials):
            order = ["vlite", "zvec"] if trial % 2 == 0 else ["zvec", "vlite"]
            for mode in ("exact", "ann"):
                for engine in order:
                    command = [sys.executable, __file__, "--child", "--engine", engine, "--mode", mode,
                               "--data", str(data_path), "--k", str(args.k), "--ef", str(args.ef),
                               "--warmup", str(args.warmup)]
                    completed = subprocess.run(command, capture_output=True, text=True)
                    if completed.returncode != 0:
                        raise SystemExit(f"{engine}/{mode} child failed:\n{completed.stderr}")
                    result = json.loads(completed.stdout.strip().splitlines()[-1])
                    scores = [recall(found, truth_dist[i], distances[i], args.k)
                              for i, found in enumerate(result.pop("ids"))]
                    result.update(trial=trial, recall_at_k=float(np.mean(scores)), min_recall=float(np.min(scores)))
                    report["runs"].append(result)
                    print(f"trial {trial} {mode:5} {engine:5}: ingest {result['ingest_s']:.2f}s "
                          f"build {result['index_build_s']:.2f}s p50 {result['p50_ms']:.3f}ms "
                          f"p99 {result['p99_ms']:.3f}ms qps {result['sequential_qps']:.0f} "
                          f"recall {result['recall_at_k']:.4f} rss+ {result['rss_growth_bytes'] / 2**20:.0f}MiB "
                          f"disk {result['footprint_bytes'] / 2**20:.1f}MiB "
                          f"reopen {result['reopen_first_query_s']:.2f}s", file=sys.stderr)
    args.output.write_text(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
