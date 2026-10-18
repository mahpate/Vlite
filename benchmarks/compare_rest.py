"""Sequential, reproducible VLite/Qdrant REST comparison; Python standard library only."""

import argparse
import datetime
import hashlib
import http.client
import json
import math
import os
import platform
import secrets
import statistics
import struct
import subprocess
import time
import uuid


def docker(*arguments, env=None):
    result = subprocess.run(
        ["docker", *arguments], check=True, capture_output=True, text=True,
        timeout=600, env=env,
    )
    return result.stdout.strip()


def dataset(count, dimensions, state):
    mask = (1 << 64) - 1
    rows = []
    for _ in range(count):
        row = []
        for _ in range(dimensions):
            state = (state + 0x9E3779B97F4A7C15) & mask
            value = state
            value = ((value ^ (value >> 30)) * 0xBF58476D1CE4E5B9) & mask
            value = ((value ^ (value >> 27)) * 0x94D049BB133111EB) & mask
            value ^= value >> 31
            row.append(((value >> 48) - 32768) / 32768.0)
        rows.append(row)
    return rows, state


def distance(a, b):
    return sum((x - y) ** 2 for x, y in zip(a, b))


def percentile(values, fraction):
    ordered = sorted(values)
    return ordered[math.ceil(len(ordered) * fraction) - 1]

def check_hits(hits, expected, vector, data, config):
    ids = [point_id for point_id, _ in hits]
    if len(ids) != len(set(ids)) or len(ids) > config.k:
        raise AssertionError("duplicate IDs or excess hits")
    for point_id, score in hits:
        if not 0 <= point_id < len(data) or point_id % config.filter_every:
            raise AssertionError(f"invalid/ineligible ID {point_id}")
        if not math.isfinite(score) or not math.isclose(
                score, distance(vector, data[point_id]), rel_tol=2e-5, abs_tol=1e-5):
            raise AssertionError("incorrect distance")
    if any(left[1] > right[1] + max(1e-5, abs(right[1])*2e-5)
           for left, right in zip(hits, hits[1:])):
        raise AssertionError("results not ordered by distance")
    cutoff = expected[-1][1]
    # Accept actual kth-distance ties, never a lower-precision score as proof of a tie.
    correct = sum(distance(vector, data[i]) <= cutoff + 1e-12 for i in ids)
    recall = correct / len(expected)
    if config.mode == "exact" and (recall != 1 or len(ids) != len(expected)):
        raise AssertionError("exact search failed ground truth")
    return recall


class Client:
    def __init__(self, port, engine, token):
        self.connection = http.client.HTTPConnection("127.0.0.1", port, timeout=120)
        self.headers = {"Content-Type": "application/json"}
        self.headers["Authorization" if engine == "vlite" else "api-key"] = (
            "Bearer " + token if engine == "vlite" else token
        )

    def request(self, method, path, body=None):
        self.connection.request(
            method, path, None if body is None else json.dumps(body, separators=(",", ":")),
            self.headers,
        )
        response = self.connection.getresponse()
        content = response.read()
        if not 200 <= response.status < 300:
            raise RuntimeError(f"HTTP {response.status} on {method} {path}: {content[:200]!r}")
        return json.loads(content) if content else None

    def close(self):
        self.connection.close()


def benchmark(engine, trial, config, data, queries, truth):
    name = "vlite-perf-" + uuid.uuid4().hex[:12]
    volume = name + "-data"
    token = secrets.token_urlsafe(32)
    env = dict(os.environ, VLITE_AUTH_TOKEN=token, QDRANT__SERVICE__API_KEY=token)
    image = config.vlite_image if engine == "vlite" else config.qdrant_image
    docker("volume", "create", "--label", "vlite.benchmark=true", volume)
    run = ["run", "--detach", "--name", name, "--cpus", str(config.cpus),
           "--memory", config.memory, "--cap-drop", "ALL",
           "--security-opt", "no-new-privileges:true"]
    if engine == "vlite":
        run += ["--read-only", "--env", "VLITE_AUTH_TOKEN", "--publish", "127.0.0.1::8080",
                "--mount", f"type=volume,src={volume},dst=/data",
                "--entrypoint", "/usr/local/bin/vlite-server", image,
                "/data/bench.vlite", "--create-if-missing", str(config.dimensions),
                "--bind", "0.0.0.0:8080", "--allow-remote"]
        internal_port = "8080/tcp"
    else:
        run += ["--env", "QDRANT__SERVICE__API_KEY", "--env", "QDRANT__TELEMETRY_DISABLED=true",
                "--publish", "127.0.0.1::6333",
                "--mount", f"type=volume,src={volume},dst=/qdrant/storage", image]
        internal_port = "6333/tcp"
    client = None
    try:
        docker(*run, env=env)
        port = int(docker("port", name, internal_port).rsplit(":", 1)[1])
        deadline = time.monotonic() + 60
        while True:
            try:
                if client:
                    client.close()
                client = Client(port, engine, token)
                version = client.request("GET", "/v1/database" if engine == "vlite" else "/")
                break
            except (OSError, http.client.HTTPException):
                if time.monotonic() >= deadline:
                    raise TimeoutError(f"{engine} startup timeout")
                time.sleep(0.2)
        if engine == "qdrant":
            client.request("PUT", "/collections/bench", {
                "vectors": {"size": config.dimensions, "distance": "Euclid", "on_disk": False},
                "hnsw_config": {"m": 16, "ef_construct": 100, "max_indexing_threads": config.cpus,
                                "full_scan_threshold": 10 if config.mode == "ann" else 10000},
                "optimizers_config": {"indexing_threshold": 1 if config.mode == "ann" else 0},
                "on_disk_payload": False,
            })
        started = time.perf_counter()
        for start in range(0, len(data), config.batch_size):
            points = list(enumerate(data[start:start + config.batch_size], start))
            if engine == "vlite":
                body = {"operations": [
                    {"op": "put", "id": str(i), "vector": vector, "metadata": {
                        "selected": {"type": "boolean", "value": i % config.filter_every == 0}
                    }} for i, vector in points
                ]}
                client.request("POST", "/v1/batches", body)
            else:
                client.request("PUT", "/collections/bench/points?wait=true", {
                    "points": [{"id": i, "vector": vector, "payload": {
                        "selected": i % config.filter_every == 0
                    }} for i, vector in points],
                })
        ingest_seconds = time.perf_counter() - started

        def query(vector):
            if engine == "vlite":
                body = {"vector": vector, "k": config.k,
                        "mode": "approximate" if config.mode == "ann" else "exact"}
                if config.mode == "ann":
                    body["ef_search"] = 100
                if config.filter_every != 1:
                    body["filter"] = [{"op": "eq", "field": "selected",
                                       "value": {"type": "boolean", "value": True}}]
                return [(int(hit["id"]), hit["distance"])
                        for hit in client.request("POST", "/v1/search", body)["hits"]]
            body = {"query": vector, "limit": config.k,
                    "with_payload": False, "with_vector": False,
                    "params": {"exact": config.mode == "exact", "hnsw_ef": 100}}
            if config.filter_every != 1:
                body["filter"] = {"must": [{"key": "selected", "match": {"value": True}}]}
            return [(int(hit["id"]), hit["score"] ** 2)
                    for hit in client.request("POST", "/collections/bench/points/query", body)["result"]["points"]]

        indexing_started = time.perf_counter()
        if engine == "qdrant" and config.mode == "ann":
            deadline = time.monotonic() + 300
            while True:
                info = client.request("GET", "/collections/bench")["result"]
                if info["status"] == "green" and info["indexed_vectors_count"] >= len(data):
                    break
                if time.monotonic() >= deadline:
                    raise TimeoutError(f"Qdrant index not ready: {info}")
                time.sleep(0.2)
        query(queries[0])  # Includes VLite's synchronous initial graph build.
        first_query_and_index_wait_seconds = time.perf_counter() - indexing_started
        for vector in queries[:min(10, len(queries))]:
            query(vector)
        elapsed_ms = []
        recall = []
        for vector, expected in zip(queries, truth):
            started = time.perf_counter_ns()
            hits = query(vector)
            elapsed_ms.append((time.perf_counter_ns() - started) / 1e6)
            recall.append(check_hits(hits, expected, vector, data, config))
        result = {
            "engine": engine, "trial": trial, "image": image,
            "image_id": docker("inspect", "--format", "{{.Image}}", name),
            "server_version": version, "retained_volume": volume,
            "ingest_seconds": ingest_seconds,
            "first_query_and_index_wait_seconds": first_query_and_index_wait_seconds,
            "latency_ms": {"p50": percentile(elapsed_ms, .5), "p95": percentile(elapsed_ms, .95),
                           "p99": percentile(elapsed_ms, .99)},
            "query_samples_ms": elapsed_ms, "mean_tie_aware_recall_at_k": statistics.mean(recall),
            "query_recall_samples": recall,
            "sequential_queries_per_second": len(elapsed_ms) / (sum(elapsed_ms) / 1000),
            "peak_memory_bytes": None,
        }
        if engine == "vlite":
            result["metrics"] = client.request("GET", "/metrics")
        else:
            result["collection"] = client.request("GET", "/collections/bench")["result"]
        return result
    finally:
        if client:
            client.close()
        # Named data volumes are deliberately retained; only our uniquely named container is removed.
        existing = docker("ps", "--all", "--quiet", "--filter", "name=^/" + name + "$")
        if existing:
            docker("stop", "--time", "30", name)
            docker("rm", name)
        print(f"Retained benchmark volume: {volume}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--vlite-image", default="vlite-smoke:local")
    parser.add_argument("--qdrant-image", default="qdrant/qdrant:v1.14.1")
    parser.add_argument("--records", type=int, default=10000)
    parser.add_argument("--dimensions", type=int, default=32)
    parser.add_argument("--queries", type=int, default=100)
    parser.add_argument("--batch-size", type=int, default=100)
    parser.add_argument("--k", type=int, default=10)
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--filter-every", type=int, default=1)
    parser.add_argument("--mode", choices=["exact", "ann"], default="exact")
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--cpus", type=int, default=2)
    parser.add_argument("--memory", default="2g")
    parser.add_argument("--output", required=True)
    config = parser.parse_args()
    if not (1 <= config.records <= 100000 and 1 <= config.dimensions <= 4096
            and 1 <= config.queries <= 1000 and 1 <= config.k <= min(config.records, 10000)
            and 1 <= config.batch_size <= 1000 and 1 <= config.trials <= 10
            and config.filter_every >= 1 and config.cpus >= 1
            and config.records * config.dimensions * config.queries <= 100000000):
        parser.error("workload outside bounded comparison limits")
    if config.mode == "ann" and (config.filter_every != 1 or config.k > 100):
        parser.error("ANN comparison requires no filter and k <= ef_search=100")
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    dirty = bool(subprocess.check_output(["git", "status", "--porcelain"], text=True).strip())
    # Refuse to overwrite published results before starting any containers.
    with open(config.output, "x", encoding="utf-8") as output:
        output.write("")
    data, state = dataset(config.records, config.dimensions, config.seed)
    queries, _ = dataset(config.queries, config.dimensions, state)
    truth = []
    for vector in queries:
        candidates = [(i, distance(vector, row)) for i, row in enumerate(data)
                      if i % config.filter_every == 0]
        candidates.sort(key=lambda hit: (hit[1], hit[0]))
        truth.append(candidates[:config.k])
    checksum = hashlib.sha256(b"".join(struct.pack("<f", x) for row in data + queries for x in row)).hexdigest()
    source_hash = hashlib.sha256()
    for path in ["Cargo.toml", "Cargo.lock", "Dockerfile", "benchmarks/compare_rest.py"] + sorted(
            subprocess.check_output(["git", "ls-files", "src"], text=True).splitlines()):
        source_hash.update(path.encode("utf-8") + b"\0")
        with open(path, "rb") as source:
            source_hash.update(source.read())
    cpu = platform.processor()
    if os.path.exists("/proc/cpuinfo"):
        with open("/proc/cpuinfo", encoding="utf-8") as cpuinfo:
            cpu = next((line.split(":", 1)[1].strip() for line in cpuinfo
                        if line.startswith("model name")), cpu)
    report = {
        "schema_version": 1, "status": "running",
        "utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "git_commit": commit,
        "git_dirty": dirty,
        "source_tree_sha256": source_hash.hexdigest(),
        "platform": platform.platform(), "cpu": cpu,
        "python": platform.python_version(),
        "docker": json.loads(docker("info", "--format", "{{json .}}")),
        "config": vars(config), "dataset_and_queries_sha256": checksum, "runs": [],
    }
    # Publish no host paths, registry credentials, environment, or Docker daemon labels.
    report["docker"] = {key: report["docker"].get(key)
                        for key in ["ServerVersion", "Architecture", "NCPU", "MemTotal", "KernelVersion", "OperatingSystem"]}
    def save():
        with open(config.output, "w", encoding="utf-8") as output:
            json.dump(report, output, indent=2)
            output.write("\n")
    save()
    try:
        for trial in range(config.trials):
            engines = ["vlite", "qdrant"] if trial % 2 == 0 else ["qdrant", "vlite"]
            for engine in engines:
                report["runs"].append(benchmark(engine, trial + 1, config, data, queries, truth))
                save()
        report["status"] = "complete"
    except BaseException:
        report["status"] = "failed"
        raise
    finally:
        save()


if __name__ == "__main__":
    main()
