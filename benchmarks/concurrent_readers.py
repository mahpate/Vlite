#!/usr/bin/env python3
"""Compare VLite REST reader throughput before/after concurrent-read changes.

Standard library only. Builds/indexes data outside timed search windows and keeps
one HTTP/1.1 connection per worker. The HNSW path uses approximate_ready so no
lazy rebuild or exact fallback is timed.
"""
import argparse
import datetime as dt
import hashlib
import http.client
import json
import math
import os
import platform
import random
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path


def percentile(samples, p):
    values = sorted(samples)
    return values[max(0, math.ceil(p * len(values)) - 1)] if values else None


def summarize(samples_ms, errors, seconds):
    return {
        "completed_queries": len(samples_ms),
        "qps": len(samples_ms) / seconds if seconds else 0.0,
        "p50_ms": percentile(samples_ms, 0.50),
        "p95_ms": percentile(samples_ms, 0.95),
        "p99_ms": percentile(samples_ms, 0.99),
        "error_count": errors,
        "measurement_seconds": seconds,
    }


def request(conn, method, path, body=None):
    headers = {"Content-Type": "application/json"} if body is not None else {}
    conn.request(method, path, body=body, headers=headers)
    response = conn.getresponse()
    content = response.read()
    if not 200 <= response.status < 300:
        raise RuntimeError(f"HTTP {response.status}: {content[:500]!r}")
    return content


def probe_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_server(port, log_path, seconds=120):
    deadline = time.monotonic() + seconds
    last = None
    while time.monotonic() < deadline:
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            body = request(conn, "GET", "/v1/database")
            conn.close()
            return json.loads(body)
        except Exception as exc:  # readiness polling
            last = exc
            time.sleep(0.1)
    raise TimeoutError(f"server did not become ready: {last}; log={log_path}")


def stop_server(process):
    if process.poll() is not None:
        return
    if os.name == "nt":
        subprocess.run(
            ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
             f"Stop-Process -Id {process.pid} -Force -ErrorAction SilentlyContinue"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False, timeout=15,
        )
    else:
        process.terminate()
    try:
        process.wait(timeout=15)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def make_centers(dimensions, seed):
    rng = random.Random(seed)
    return [[rng.uniform(-1.0, 1.0) for _ in range(dimensions)] for _ in range(64)]


def query_for(centers, dimensions, seed):
    rng = random.Random(seed)
    center = centers[seed % len(centers)]
    return [round(value + rng.gauss(0.0, 0.35), 4) for value in center[:dimensions]]


def generate_jsonl(path, records, dimensions, seed):
    """Generate reproducible 64-cluster Gaussian vectors and return SHA-256."""
    centers = make_centers(dimensions, seed)
    rng = random.Random(seed + 1)
    digest = hashlib.sha256()
    with path.open("w", encoding="utf-8", newline="\n") as stream:
        for record_id in range(records):
            center = centers[record_id % len(centers)]
            vector = [round(value + rng.gauss(0.0, 0.35), 4) for value in center]
            line = json.dumps({"id": record_id, "vector": vector}, separators=(",", ":"))
            stream.write(line + "\n")
            digest.update((line + "\n").encode("utf-8"))
    return digest.hexdigest(), query_for(centers, dimensions, seed + 9)


def import_dataset(importer, db_path, data_path, dimensions, timeout):
    command = [str(importer), "--db", str(db_path), "--input", str(data_path),
               "--format", "jsonl", "--create", str(dimensions), "--chunk-size", "4095"]
    result = subprocess.run(command, capture_output=True, text=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f"vlite-import failed ({result.returncode}): {result.stderr[-2000:]}")


def bench_concurrency(port, query, mode, concurrency, duration, warmup):
    path = "/v1/search"
    search = {"vector": query, "k": 10,
              "mode": "exact" if mode == "exact" else "approximate_ready"}
    if mode != "exact":
        search["ef_search"] = 100
    request_body = json.dumps(search, separators=(",", ":")).encode("utf-8")
    worker_count = concurrency
    connections_ready = threading.Barrier(worker_count + 1)
    warmup_ready = threading.Barrier(worker_count + 1)
    warmup_start = threading.Event()
    measuring_start = threading.Event()
    stop = threading.Event()
    shared = {"deadline": 0.0}
    results = [{"latencies_ms": [], "error_count": 0, "warmup_errors": 0, "error_samples": []}
               for _ in range(worker_count)]

    def worker(index):
        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=120)
        try:
            connections_ready.wait(timeout=30)
            warmup_start.wait()
            warm_end = time.monotonic() + warmup
            while time.monotonic() < warm_end:
                try:
                    request(conn, "POST", path, request_body)
                except Exception as exc:
                    results[index]["warmup_errors"] += 1
                    if len(results[index]["error_samples"]) < 3:
                        results[index]["error_samples"].append(f"warmup: {exc!r}")
                    conn.close()
                    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=120)
            warmup_ready.wait(timeout=120)
            measuring_start.wait()
            while not stop.is_set():
                started = time.perf_counter_ns()
                try:
                    request(conn, "POST", path, request_body)
                    elapsed = (time.perf_counter_ns() - started) / 1e6
                    if time.monotonic() <= shared["deadline"]:
                        results[index]["latencies_ms"].append(elapsed)
                except Exception as exc:
                    if time.monotonic() <= shared["deadline"]:
                        results[index]["error_count"] += 1
                        if len(results[index]["error_samples"]) < 3:
                            results[index]["error_samples"].append(f"measurement: {exc!r}")
                    conn.close()
                    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=120)
        finally:
            conn.close()

    threads = [threading.Thread(target=worker, args=(i,), daemon=True) for i in range(worker_count)]
    for thread in threads:
        thread.start()
    try:
        connections_ready.wait(timeout=30)
        warmup_start.set()
        time.sleep(warmup)
        warmup_ready.wait(timeout=120)
        start = time.monotonic()
        shared["deadline"] = start + duration
        measuring_start.set()
        time.sleep(duration)
        stop.set()
        for thread in threads:
            thread.join(timeout=130)
            if thread.is_alive():
                raise TimeoutError("search worker did not finish")
    except Exception:
        stop.set()
        measuring_start.set()
        raise
    latencies = [latency for result in results for latency in result["latencies_ms"]]
    errors = sum(result["error_count"] for result in results)
    error_samples = [sample for result in results for sample in result["error_samples"]]
    return summarize(latencies, errors, duration) | {
        "warmup_errors": sum(result["warmup_errors"] for result in results),
        "error_samples": error_samples[:12],
    }


def bench_single_exact(port, query, duration, warmup):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=120)
    body = json.dumps({"vector": query, "k": 10, "mode": "exact"}, separators=(",", ":")).encode()
    try:
        warm_end = time.monotonic() + warmup
        while time.monotonic() < warm_end:
            request(conn, "POST", "/v1/search", body)
        end = time.monotonic() + duration
        samples, errors = [], 0
        while time.monotonic() < end:
            started = time.perf_counter_ns()
            try:
                request(conn, "POST", "/v1/search", body)
                samples.append((time.perf_counter_ns() - started) / 1e6)
            except Exception:
                errors += 1
                conn.close()
                conn = http.client.HTTPConnection("127.0.0.1", port, timeout=120)
        return summarize(samples, errors, duration)
    finally:
        conn.close()


def build_index(port):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=1200)
    try:
        request(conn, "POST", "/v1/index/hnsw", b"{}")
    finally:
        conn.close()


def start_server(binary, db_path, temp, label, timeout):
    port = probe_port()
    log_path = "server stdout/stderr redirected to DEVNULL"
    log = None
    process = subprocess.Popen(
        [str(binary), str(db_path), "--bind", f"127.0.0.1:{port}", "--insecure-local"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, cwd=binary.parent,
        creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
    )
    try:
        info = wait_server(port, log_path, timeout)
        return process, log, port, info, log_path
    except Exception:
        stop_server(process)
        raise


def stop_run(process, log):
    stop_server(process)


def cpu_model():
    if os.name == "nt":
        try:
            result = subprocess.run(
                ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
                 "Get-CimInstance Win32_Processor | Select-Object -First 1 -ExpandProperty Name"],
                capture_output=True, text=True, timeout=8,
            )
            if result.returncode == 0 and result.stdout.strip():
                return result.stdout.strip().splitlines()[-1]
        except Exception:
            pass
    return platform.processor() or os.environ.get("PROCESSOR_IDENTIFIER") or platform.machine()


def physical_core_count():
    if os.name == "nt":
        try:
            result = subprocess.run(
                ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
                 "(Get-CimInstance Win32_Processor | Measure-Object -Property NumberOfCores -Sum).Sum"],
                capture_output=True, text=True, timeout=8,
            )
            if result.returncode == 0 and result.stdout.strip():
                return int(float(result.stdout.strip().splitlines()[-1]))
        except Exception:
            pass
    return None


def sample_cpu_load():
    if os.name != "nt":
        return None
    try:
        result = subprocess.run(
            ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
             "(Get-Counter '\\Processor(_Total)\\% Processor Time' -SampleInterval 1 -MaxSamples 2).CounterSamples[-1].CookedValue"],
            capture_output=True, text=True, timeout=8,
        )
        return round(float(result.stdout.strip().splitlines()[-1]), 1) if result.returncode == 0 else None
    except Exception:
        return None


def wait_for_headroom(threshold_pct=50.0, timeout_seconds=120):
    started = time.monotonic()
    samples = []
    while True:
        load = sample_cpu_load()
        samples.append(load)
        if load is None or load <= threshold_pct:
            return {"load_pct": load, "wait_seconds": round(time.monotonic() - started, 1),
                    "samples_pct": samples, "timed_out": False}
        if time.monotonic() - started >= timeout_seconds:
            return {"load_pct": load, "wait_seconds": round(time.monotonic() - started, 1),
                    "samples_pct": samples, "timed_out": True}
        time.sleep(10)


def run_version(name, binary, importer, revision, worktree_dirty, data20_path, data100_path, hashes, args, temp):
    result = {"name": name, "revision": revision, "worktree_dirty": worktree_dirty,
              "release_version": "0.3.0", "binary": str(binary),
              "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(), "datasets": {},
              "database_format_version": None}
    for dataset_name, records, dimensions, data_path in (
        ("clustered_20k_128d", 20_000, 128, data20_path),
        ("clustered_100k_768d", 100_000, 768, data100_path),
    ):
        db_path = temp / f"{name}-{dataset_name}.vlite"
        import_dataset(importer, db_path, data_path, dimensions, args.import_timeout)
        process, log, port, info, log_path = start_server(binary, db_path, temp, f"{name}-{dataset_name}", args.start_timeout)
        if result["database_format_version"] is None:
            result["database_format_version"] = info.get("format_version")
        query = query_for(make_centers(dimensions, args.seed), dimensions, args.seed + 9)
        try:
            ds = {"records": records, "dimensions": dimensions, "dataset_sha256": hashes[dataset_name],
                  "server_database_info": info, "modes": {}}
            if records == 20_000:
                for mode in ("exact", "hnsw_approximate_ready"):
                    if mode == "hnsw_approximate_ready":
                        build_index(port)
                    mode_key = "approximate_ready" if mode.startswith("hnsw") else mode
                    by_concurrency = {}
                    for concurrency in (1, 4, 8, 16):
                        trials = []
                        for trial in range(args.trials):
                            headroom = wait_for_headroom()
                            metrics = bench_concurrency(port, query, mode_key, concurrency,
                                                        args.duration, args.warmup)
                            metrics["trial"] = trial + 1
                            metrics["host_cpu_before_trial"] = headroom
                            trials.append(metrics)
                        by_concurrency[str(concurrency)] = {
                            "trials": trials,
                            "median_qps": statistics.median(t["qps"] for t in trials),
                            "median_p50_ms": statistics.median([t["p50_ms"] for t in trials if t["p50_ms"] is not None]) if any(t["p50_ms"] is not None for t in trials) else None,
                            "median_p95_ms": statistics.median([t["p95_ms"] for t in trials if t["p95_ms"] is not None]) if any(t["p95_ms"] is not None for t in trials) else None,
                            "median_p99_ms": statistics.median([t["p99_ms"] for t in trials if t["p99_ms"] is not None]) if any(t["p99_ms"] is not None for t in trials) else None,
                            "median_errors": statistics.median(t["error_count"] for t in trials),
                        }
                    ds["modes"][mode] = by_concurrency
            else:
                ds["single_query_exact"] = [
                    {"trial": trial + 1, "host_cpu_before_trial": wait_for_headroom(),
                     **bench_single_exact(port, query, args.single_duration, args.warmup)}
                    for trial in range(args.trials)
                ]
                ds["single_query_exact_median"] = {
                    key: statistics.median(trial[key] for trial in ds["single_query_exact"])
                    for key in ("qps", "p50_ms", "p95_ms", "p99_ms", "error_count")
                }
            result["datasets"][dataset_name] = ds
        finally:
            stop_run(process, log)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-binary", required=True, type=Path)
    parser.add_argument("--after-binary", required=True, type=Path)
    parser.add_argument("--baseline-importer", required=True, type=Path)
    parser.add_argument("--after-importer", required=True, type=Path)
    parser.add_argument("--baseline-commit", required=True)
    parser.add_argument("--after-commit", required=True)
    parser.add_argument("--after-dirty", action="store_true")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--duration", type=float, default=10.0)
    parser.add_argument("--single-duration", type=float, default=10.0)
    parser.add_argument("--warmup", type=float, default=2.0)
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--import-timeout", type=int, default=3600)
    parser.add_argument("--start-timeout", type=int, default=180)
    args = parser.parse_args()
    if args.trials < 1 or args.duration <= 0 or args.single_duration <= 0 or args.warmup < 0:
        parser.error("trials/durations must be positive and warmup non-negative")
    if args.output.exists():
        parser.error(f"output already exists: {args.output}")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    cpu = cpu_model()
    initial_cpu_load = sample_cpu_load()
    datasets = ("clustered_20k_128d", "clustered_100k_768d")
    with tempfile.TemporaryDirectory(prefix="vlite-reader-bench-") as work:
        temp = Path(work)
        dataset_paths = {datasets[0]: temp / "clustered-20k-128d.jsonl",
                         datasets[1]: temp / "clustered-100k-768d.jsonl"}
        specs = ((datasets[0], 20_000, 128), (datasets[1], 100_000, 768))
        hashes = {}
        for dataset_name, records, dimensions in specs:
            hashes[dataset_name], _ = generate_jsonl(dataset_paths[dataset_name], records, dimensions, args.seed)
        runs = []
        for name, binary, importer, revision, dirty in (
            ("baseline", args.baseline_binary.resolve(), args.baseline_importer.resolve(), args.baseline_commit, False),
            ("after", args.after_binary.resolve(), args.after_importer.resolve(), args.after_commit, args.after_dirty),
        ):
            runs.append(run_version(name, binary, importer, revision, dirty, dataset_paths[datasets[0]],
                                    dataset_paths[datasets[1]], hashes, args, temp))
        report = {
            "schema_version": 1,
            "created_at_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
            "platform": platform.platform(),
            "python": platform.python_version(),
            "hardware": {"cpu_model": cpu, "physical_cores": physical_core_count(),
                         "logical_cores": os.cpu_count(), "initial_host_cpu_load_pct": initial_cpu_load},
            "methodology": {
                "transport": "REST JSON over HTTP/1.1 with one persistent connection per worker",
                "search_k": 10,
                "modes": {"exact": "POST /v1/search mode=exact",
                          "hnsw_approximate_ready": "HNSW built before timing; POST /v1/search mode=approximate_ready ef_search=100"},
                "worker_permit_limit": 16,
                "concurrency_levels": [1, 4, 8, 16],
                "duration_seconds": args.duration,
                "warmup_seconds": args.warmup,
                "trials_per_condition": args.trials,
                "single_exact_duration_seconds": args.single_duration,
                "dataset": "64-cluster Gaussian float32-like vectors, stdlib random seed 42, each component rounded to 4 decimals; reused identical JSONL for both binaries",
                "ingestion": "vlite-import JSONL, chunk-size 4095; ingestion and HNSW build excluded from search timing",
                "query_generation": "64-cluster query vector, deterministic seed derived from dataset seed",
                "latency_includes": "client request serialization, loopback HTTP, server JSON parsing/REST work, and response read; no request pipelining",
                "server_output": "stdout/stderr redirected to DEVNULL; request logs are not written to benchmark disk",
                "pre_trial_cpu_gate": "Waits for total CPU load <= 50% before each trial, up to 120 seconds; load samples and timeout status are stored per trial",
                "limitations": ["shared unpinned Windows host; CPU load can vary during the long run",
                                "single host, synthetic data, no concurrent writes; server worker permits cap at 16",
                                "single-query exact is REST end-to-end latency, not in-process kernel-only latency"],
            },
            "datasets": {name: {"records": rows, "dimensions": dims, "sha256_jsonl": hashes[name]}
                         for name, rows, dims in specs},
            "runs": runs,
        }
        args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"output": str(args.output), "runs": len(runs), "cpu_model": cpu,
                      "logical_cores": os.cpu_count()}))


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("interrupted", file=sys.stderr)
        raise SystemExit(130)
