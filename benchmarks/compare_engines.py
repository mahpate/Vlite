"""Multi-engine vector database comparison: VLite against zvec, Chroma, LanceDB,
FAISS, Qdrant and pgvector on identical data, queries and ground truth.

Two tiers are measured and must never be ranked together:

* ``--tier embedded`` (in-process Python): VLite embedded, zvec, Chroma
  ``PersistentClient``, LanceDB and FAISS. FAISS is an in-memory library
  baseline with no persistence or durability; its disk/reopen columns are N/A.
* ``--tier server`` (client/server over localhost): the VLite REST server and
  Qdrant over REST/JSON with one persistent HTTP/1.1 connection, Qdrant over
  gRPC (``qdrant-client``) as an extra row, and PostgreSQL + pgvector over the
  PostgreSQL wire protocol (``psycopg``). Every server container gets the same
  ``--cpus``/``--memory`` limits.

Methodology
-----------
* Clustered Gaussian float32 vectors (``--clusters 64 --spread 0.35 --seed 42``)
  and queries; recall@k is ID-based and tie-aware against a float64 NumPy
  brute-force oracle computed outside timing. Squared L2 (or Euclidean, which
  ranks identically) everywhere.
* Every (engine, mode, trial) runs in a fresh child process (and, for servers,
  a fresh container and volume). Engine order rotates between trials.
* Ingest uses the same batch size for every engine:
  ``max(1, min(1000, 4_000_000 // (dims * 24)))`` records, one commit per batch.
* ``exact`` mode is brute force (N/A for engines without one, e.g. Chroma).
  ``ann`` mode is HNSW with VLite's defaults ``M=16, ef_construction=100``
  applied explicitly to every engine that exposes them, and ``ef_search=100``.
  LanceDB has no plain HNSW index; it uses ``IVF_HNSW_SQ`` with one partition.
  Engines that index incrementally (Chroma, FAISS HNSW) report their graph cost
  inside ``ingest_s``; compare ``ingest_plus_build_s`` across engines.
* Latency is sequential: one client thread issues one query at a time after
  ``--warmup`` queries; ``sequential_qps = queries / sum(latencies)``.
* ``rss_growth_bytes`` is process VmRSS growth (embedded) or container memory
  growth from ``docker stats`` (server) between "empty, opened" and "after
  index build". ``footprint_bytes`` is the on-disk size after a clean close /
  container stop. ``reopen_first_query_s`` covers reopen (or container start),
  readiness, any required index rebuild and the first query.
* Engine durability defaults are not changed; they differ and are recorded in
  ``durability`` in the report. A build exceeding ``--build-timeout`` is
  recorded as ``timeout``; a crashed run is recorded as ``error``.

Run on Linux/WSL with Docker available for the server tier::

    pip install numpy zvec chromadb lancedb faiss-cpu "psycopg[binary]" pgvector \\
        qdrant-client vlite_embedded-<version>-py3-none-manylinux_2_28_x86_64.whl
    python benchmarks/compare_engines.py --tier embedded --output embedded.json
    python benchmarks/compare_engines.py --tier server --output server.json
    python benchmarks/compare_engines.py --summary embedded.json server.json
"""
import argparse
import datetime
import hashlib
import http.client
import json
import os
from pathlib import Path
import platform
import re
import resource
import secrets
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))
from compare_zvec import (  # noqa: E402
    HNSW_M, HNSW_EF_CONSTRUCTION, VLiteEngine, ZvecEngine, batch_size, current_rss,
    directory_bytes, dist_version, make_dataset, recall, summary,
)

DURABILITY = {
    "vlite": "Each batch is one atomic commit fsynced before acknowledgment (Durability::Full, the default "
             "before v0.5.1; v0.5.1+ defaults to periodic sync every 60 s).",
    "zvec": "Defaults; the harness calls flush() before close. Per-insert fsync was not verified.",
    "chroma": "Defaults: SQLite-backed write-ahead log; HNSW segment files are persisted "
              "asynchronously in batches (sync_threshold). Not fsync per add().",
    "lancedb": "Defaults: each add() writes data files and commits a new table version (manifest). "
               "Per-commit fsync was not verified.",
    "faiss": "In-memory library baseline: no persistence, no durability, no reopen.",
    "vlite-server": "Every successful write (each /v1/batches request) is synced before 204 (--durability full, "
                    "the default before v0.5.1; v0.5.1+ defaults to periodic sync every 60 s).",
    "qdrant": "Defaults: WAL appended per request (wait=true waits for apply); WAL and segments are "
              "flushed periodically (flush_interval_sec=5), not fsynced per request.",
    "qdrant-grpc": "Same server defaults as qdrant (gRPC transport only differs).",
    "pgvector": "PostgreSQL defaults fsync=on, synchronous_commit=on: WAL fsynced at each batch "
                "transaction commit.",
}


class BuildTimeout(Exception):
    pass


class ChromaEngine:
    modes = ("ann",)
    na = {"exact": "Chroma has no brute-force query mode (HNSW only)."}

    def __init__(self, workdir, dimensions, mode, args):
        import chromadb
        from chromadb.config import Settings
        self.chromadb, self.settings = chromadb, Settings(anonymized_telemetry=False)
        self.version = dist_version("chromadb")
        self.root = Path(workdir) / "chroma"
        self.client = chromadb.PersistentClient(path=str(self.root), settings=self.settings)
        try:
            self.col = self.client.create_collection("bench", configuration={"hnsw": {
                "space": "l2", "max_neighbors": HNSW_M, "ef_construction": HNSW_EF_CONSTRUCTION,
                "ef_search": args.ef}})
            self.params = "configuration hnsw: space=l2, max_neighbors, ef_construction, ef_search"
        except (TypeError, ValueError):
            self.col = self.client.create_collection("bench", metadata={
                "hnsw:space": "l2", "hnsw:M": HNSW_M, "hnsw:construction_ef": HNSW_EF_CONSTRUCTION,
                "hnsw:search_ef": args.ef})
            self.params = "metadata hnsw:space=l2, hnsw:M, hnsw:construction_ef, hnsw:search_ef"

    def ingest(self, vectors):
        size = batch_size(vectors.shape[1])
        for start in range(0, len(vectors), size):
            chunk = vectors[start:start + size]
            self.col.add(ids=[str(start + i) for i in range(len(chunk))], embeddings=chunk)

    def build(self):
        pass  # HNSW is maintained incrementally during add().

    def search(self, query, k, ef):
        result = self.col.query(query_embeddings=[query], n_results=k, include=["distances"])
        return [int(i) for i in result["ids"][0]]

    def close(self):
        self.col = None
        self.client.clear_system_cache()
        self.client = None

    def reopen(self):
        self.client = self.chromadb.PersistentClient(path=str(self.root), settings=self.settings)
        self.col = self.client.get_collection("bench")

    def first_query_after_reopen(self, query, k, ef):
        return self.search(query, k, ef)

    def footprint(self):
        return directory_bytes(self.root)


class LanceEngine:
    modes = ("exact", "ann")

    def __init__(self, workdir, dimensions, mode, args):
        import lancedb
        import pyarrow as pa
        self.lancedb, self.pa = lancedb, pa
        self.version = dist_version("lancedb")
        self.mode, self.dimensions = mode, dimensions
        self.root = Path(workdir) / "lance"
        self.db = lancedb.connect(str(self.root))
        self.schema = pa.schema([("id", pa.int64()), ("vector", pa.list_(pa.float32(), dimensions))])
        self.table = self.db.create_table("bench", schema=self.schema)
        self.params = ("exact: flat scan, bypass_vector_index" if mode == "exact" else
                       f"IVF_HNSW_SQ num_partitions=1 m={HNSW_M} ef_construction={HNSW_EF_CONSTRUCTION}")

    def ingest(self, vectors):
        size = batch_size(vectors.shape[1])
        for start in range(0, len(vectors), size):
            chunk = vectors[start:start + size]
            ids = self.pa.array(np.arange(start, start + len(chunk), dtype=np.int64))
            vec = self.pa.FixedSizeListArray.from_arrays(self.pa.array(chunk.reshape(-1)), self.dimensions)
            self.table.add(self.pa.Table.from_arrays([ids, vec], schema=self.schema))

    def build(self):
        if self.mode == "ann":
            self.table.create_index(metric="l2", vector_column_name="vector", index_type="IVF_HNSW_SQ",
                                    num_partitions=1, m=HNSW_M, ef_construction=HNSW_EF_CONSTRUCTION)

    def search(self, query, k, ef):
        q = self.table.search(np.asarray(query, dtype=np.float32), vector_column_name="vector")
        q = q.distance_type("l2") if hasattr(q, "distance_type") else q.metric("l2")
        if self.mode == "exact":
            q = q.bypass_vector_index()
        elif hasattr(q, "ef"):
            q = q.ef(ef)
        return q.limit(k).select(["id"]).to_arrow()["id"].to_pylist()

    def close(self):
        self.table = None
        self.db = None

    def reopen(self):
        self.db = self.lancedb.connect(str(self.root))
        self.table = self.db.open_table("bench")

    def first_query_after_reopen(self, query, k, ef):
        return self.search(query, k, ef)

    def footprint(self):
        return directory_bytes(self.root)


class FaissEngine:
    modes = ("exact", "ann")

    def __init__(self, workdir, dimensions, mode, args):
        import faiss
        self.faiss = faiss
        self.version = getattr(faiss, "__version__", None) or dist_version("faiss-cpu")
        if mode == "exact":
            self.index = faiss.IndexFlatL2(dimensions)
            self.params = "IndexFlatL2"
        else:
            self.index = faiss.IndexHNSWFlat(dimensions, HNSW_M)
            self.index.hnsw.efConstruction = HNSW_EF_CONSTRUCTION
            self.index.hnsw.efSearch = args.ef
            self.params = f"IndexHNSWFlat M={HNSW_M} efConstruction={HNSW_EF_CONSTRUCTION} efSearch={args.ef}"
        self.params += f"; omp_max_threads={faiss.omp_get_max_threads()}"

    def ingest(self, vectors):
        size = batch_size(vectors.shape[1])
        for start in range(0, len(vectors), size):
            self.index.add(np.ascontiguousarray(vectors[start:start + size]))

    def build(self):
        pass  # IndexHNSWFlat inserts into the graph during add().

    def search(self, query, k, ef):
        _, ids = self.index.search(np.asarray([query], dtype=np.float32), k)
        return [int(i) for i in ids[0] if i >= 0]

    def close(self):
        pass

    reopen = first_query_after_reopen = footprint = None


class VLiteEmbedded(VLiteEngine):
    modes = ("exact", "ann")

    def __init__(self, workdir, dimensions, mode, args):
        super().__init__(workdir, dimensions, mode)
        self.params = ("search_f32() brute force" if self.typed else "search() brute force") if mode == "exact" else \
            f"build_index() default HnswConfig M={HNSW_M} ef_construction={HNSW_EF_CONSTRUCTION}; " + \
            ("search_f32(mode=approximate_ready) ef_search" if self.typed else "search_approximate_ready ef_search")
        self.params += f"; {self.transport} vector transport"


class ZvecEmbedded(ZvecEngine):
    modes = ("exact", "ann")

    def __init__(self, workdir, dimensions, mode, args):
        super().__init__(workdir, dimensions, mode)
        self.params = f"FlatIndexParam L2" if mode == "exact" else \
            f"HnswIndexParam L2 m={HNSW_M} ef_construction={HNSW_EF_CONSTRUCTION}"


EMBEDDED = {"vlite": VLiteEmbedded, "zvec": ZvecEmbedded, "chroma": ChromaEngine,
            "lancedb": LanceEngine, "faiss": FaissEngine}


# ---------------------------------------------------------------- server tier

def docker(*arguments, check=True, timeout=600):
    result = subprocess.run(["docker", *arguments], capture_output=True, text=True, timeout=timeout)
    if check and result.returncode != 0:
        raise RuntimeError(f"docker {' '.join(arguments[:2])} failed: {result.stderr.strip()[:500]}")
    return result.stdout.strip()


UNITS = {"B": 1, "KiB": 2**10, "MiB": 2**20, "GiB": 2**30, "kB": 1e3, "MB": 1e6, "GB": 1e9}


def container_memory(name):
    usage = docker("stats", "--no-stream", "--format", "{{.MemUsage}}", name).split("/")[0].strip()
    match = re.match(r"([\d.]+)\s*([A-Za-z]+)", usage)
    return int(float(match.group(1)) * UNITS[match.group(2)])


def volume_bytes(volume):
    mount = docker("volume", "inspect", "--format", "{{.Mountpoint}}", volume)
    try:
        return directory_bytes(mount)
    except OSError:
        out = docker("run", "--rm", "-v", f"{volume}:/d:ro", "alpine:3.21", "du", "-sb", "/d")
        return int(out.split()[0])


class HttpJson:
    def __init__(self, host, port, headers):
        self.port, self.headers = port, dict(headers, **{"Content-Type": "application/json"})
        self.connection = http.client.HTTPConnection(host, port, timeout=120)

    def request(self, method, path, body=None, timeout=None, idempotent=None):
        try:
            return self._request(method, path, body, timeout)
        except (http.client.RemoteDisconnected, ConnectionResetError, BrokenPipeError):
            # Servers close idle keep-alive sockets (e.g. during a long build or a gRPC
            # upload); reconnect and retry once, but only for idempotent requests.
            if not (idempotent if idempotent is not None else method in ("GET", "PUT", "PATCH", "DELETE")):
                raise
            self.connection.close()
            return self._request(method, path, body, timeout)

    def _request(self, method, path, body=None, timeout=None):
        if timeout is not None:
            self.connection.timeout = timeout
            if self.connection.sock:
                self.connection.sock.settimeout(timeout)
        try:
            payload = None if body is None else json.dumps(body, separators=(",", ":"))
            self.connection.request(method, path, payload, self.headers)
            response = self.connection.getresponse()
            content = response.read()
        finally:
            if timeout is not None:
                self.connection.timeout = 120
                if self.connection.sock:
                    self.connection.sock.settimeout(120)
        if not 200 <= response.status < 300:
            raise RuntimeError(f"HTTP {response.status} on {method} {path}: {content[:300]!r}")
        return json.loads(content) if content else None

    def close(self):
        self.connection.close()


def wait_until(probe, what, seconds=120):
    deadline = time.monotonic() + seconds
    while True:
        try:
            return probe()
        except Exception as error:  # noqa: BLE001 - readiness polling
            if time.monotonic() >= deadline:
                raise TimeoutError(f"{what} not ready: {error}")
            time.sleep(0.1)


class ServerEngine:
    """Container lifecycle shared by the server engines."""
    modes = ("exact", "ann")

    def __init__(self, name, dimensions, mode, args):
        self.name, self.volume = name, name + "-data"
        self.dimensions, self.mode, self.args = dimensions, mode, args
        self.token = secrets.token_urlsafe(32)
        self.ports = {}

    def container_args(self):
        return ["run", "--detach", "--name", self.name, "--cpus", str(self.args.cpus),
                "--memory", self.args.memory, "--memory-swap", self.args.memory,
                "--label", "vlite.benchmark=compare_engines"]

    def start_container(self, extra):
        docker("volume", "create", "--label", "vlite.benchmark=compare_engines", self.volume)
        docker(*self.container_args(), *extra)
        self.image_id = docker("inspect", "--format", "{{.Image}}", self.name)
        self.refresh_ports()

    def refresh_ports(self):
        # Connect to the container's bridge IP by default: the userland docker-proxy behind published
        # ports adds ~1 ms per round trip. --published-ports keeps the 127.0.0.1 proxy path instead.
        address = docker("inspect", "--format", "{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}",
                         self.name).split()
        if address and not self.args.published_ports:
            self.host = address[0]
            self.ports = {internal: int(internal.split("/")[0]) for internal in self.internal_ports}
            return
        self.host = "127.0.0.1"
        for internal in self.internal_ports:
            self.ports[internal] = int(docker("port", self.name, internal).splitlines()[0].rsplit(":", 1)[1])

    def stop(self):
        self.disconnect()
        docker("stop", "--time", "60", self.name)

    def restart(self):
        docker("start", self.name)
        self.refresh_ports()
        self.connect()

    def memory(self):
        return container_memory(self.name)

    def footprint(self):
        return volume_bytes(self.volume)

    def cleanup(self):
        try:
            self.disconnect()
        except Exception:  # noqa: BLE001
            pass
        docker("rm", "-f", self.name, check=False)
        docker("volume", "rm", "-f", self.volume, check=False)


class VLiteServer(ServerEngine):
    internal_ports = ("8080/tcp",)

    def start(self):
        self.image = self.args.vlite_image
        self.start_container(["--read-only", "--cap-drop", "ALL", "--security-opt", "no-new-privileges:true",
                              "--env", f"VLITE_AUTH_TOKEN={self.token}", "--publish", "127.0.0.1::8080",
                              "--mount", f"type=volume,src={self.volume},dst=/data",
                              "--entrypoint", "/usr/local/bin/vlite-server", self.image,
                              "/data/bench.vlite", "--create-if-missing", str(self.dimensions),
                              "--bind", "0.0.0.0:8080", "--allow-remote"])
        info = self.connect()
        self.version = f"{self.image} (format_version {info.get('format_version') if isinstance(info, dict) else 'n/a'})"
        self.params = "REST/JSON HTTP/1.1 keep-alive; " + ("POST /v1/batches; search mode exact" if self.mode == "exact" else
                       f"POST /v1/index/hnsw {{}} (defaults M={HNSW_M} ef_construction={HNSW_EF_CONSTRUCTION}); "
                       "search mode approximate_ready ef_search")

    def connect(self):
        def probe():
            client = HttpJson(self.host, self.ports["8080/tcp"], {"Authorization": "Bearer " + self.token})
            try:
                return client, client.request("GET", "/v1/database")
            except Exception:
                client.close()
                raise
        self.client, info = wait_until(probe, "vlite-server")
        return info

    def disconnect(self):
        if getattr(self, "client", None):
            self.client.close()
            self.client = None

    def ingest(self, vectors):
        size = batch_size(vectors.shape[1])
        for start in range(0, len(vectors), size):
            self.client.request("POST", "/v1/batches", {"operations": [
                {"op": "put", "id": str(start + i), "vector": row}
                for i, row in enumerate(vectors[start:start + size].tolist())]})

    def build(self):
        if self.mode == "ann":
            self.client.request("POST", "/v1/index/hnsw", {}, timeout=self.args.build_timeout)

    def search(self, query, k, ef):
        body = {"vector": query, "k": k, "mode": "exact"}
        if self.mode == "ann":
            body.update(mode="approximate_ready", ef_search=ef)
        return [int(hit["id"]) for hit in self.client.request("POST", "/v1/search", body, idempotent=True)["hits"]]

    def first_query_after_reopen(self, query, k, ef):
        self.build()  # The HNSW graph is in memory only; reopen requires a rebuild.
        return self.search(query, k, ef)


class QdrantServer(ServerEngine):
    internal_ports = ("6333/tcp", "6334/tcp")
    grpc = False

    def start(self):
        self.image = self.args.qdrant_image
        self.start_container(["--env", f"QDRANT__SERVICE__API_KEY={self.token}",
                              "--env", "QDRANT__TELEMETRY_DISABLED=true",
                              "--publish", "127.0.0.1::6333", "--publish", "127.0.0.1::6334",
                              "--mount", f"type=volume,src={self.volume},dst=/qdrant/storage", self.image])
        self.connect()
        self.version = f"{self.image} ({self.rest.request('GET', '/')['version']})"
        # Bulk-load recipe: upload with indexing disabled, then enable indexing as the build step.
        # full_scan_threshold is lowered so HNSW (not a plain scan) serves ANN queries.
        self.rest.request("PUT", "/collections/bench", {
            "vectors": {"size": self.dimensions, "distance": "Euclid"},
            "hnsw_config": {"m": HNSW_M, "ef_construct": HNSW_EF_CONSTRUCTION,
                            "full_scan_threshold": 10 if self.mode == "ann" else 10000},
            "optimizers_config": {"indexing_threshold": 0}})
        self.params = (("gRPC (qdrant-client)" if self.grpc else "REST/JSON HTTP/1.1 keep-alive") +
                       f"; Euclid; hnsw m={HNSW_M} ef_construct={HNSW_EF_CONSTRUCTION} full_scan_threshold="
                       f"{10 if self.mode == 'ann' else 10000}; indexing_threshold 0 during upload, 1 at build; "
                       "exact mode uses params.exact=true without building HNSW")

    def connect(self):
        def probe():
            client = HttpJson(self.host, self.ports["6333/tcp"], {"api-key": self.token})
            try:
                client.request("GET", "/collections")
                return client
            except Exception:
                client.close()
                raise
        self.rest = wait_until(probe, "qdrant")
        if self.grpc:
            from qdrant_client import QdrantClient
            self.qc = QdrantClient(host=self.host, port=self.ports["6333/tcp"], grpc_port=self.ports["6334/tcp"],
                                   prefer_grpc=True, api_key=self.token, https=False, timeout=120)

    def disconnect(self):
        if getattr(self, "rest", None):
            self.rest.close()
            self.rest = None
        if getattr(self, "qc", None):
            self.qc.close()
            self.qc = None

    def ingest(self, vectors):
        size = batch_size(vectors.shape[1])
        for start in range(0, len(vectors), size):
            chunk = vectors[start:start + size]
            ids = list(range(start, start + len(chunk)))
            if self.grpc:
                from qdrant_client import models
                self.qc.upsert("bench", points=models.Batch(ids=ids, vectors=chunk.tolist()), wait=True)
            else:
                self.rest.request("PUT", "/collections/bench/points?wait=true", {
                    "points": [{"id": i, "vector": row} for i, row in zip(ids, chunk.tolist())]})

    def wait_green(self, deadline_s, require_indexed):
        deadline = time.monotonic() + deadline_s
        while True:
            info = self.rest.request("GET", "/collections/bench")["result"]
            indexed = info.get("indexed_vectors_count") or 0
            if info["status"] == "green" and (not require_indexed or indexed >= info["points_count"]):
                return info
            if time.monotonic() >= deadline:
                raise BuildTimeout(f"qdrant index not ready after {deadline_s}s: {info.get('status')} {indexed}")
            time.sleep(0.05)

    def build(self):
        if self.mode == "ann":
            self.rest.request("PATCH", "/collections/bench", {"optimizers_config": {"indexing_threshold": 1}})
            time.sleep(0.05)
            self.info = self.wait_green(self.args.build_timeout, True)
        else:
            self.info = self.wait_green(self.args.build_timeout, False)

    def search(self, query, k, ef):
        exact = self.mode == "exact"
        if self.grpc:
            from qdrant_client import models
            result = self.qc.query_points("bench", query=query, limit=k, with_payload=False,
                                          search_params=models.SearchParams(hnsw_ef=ef, exact=exact))
            return [int(p.id) for p in result.points]
        body = {"query": query, "limit": k, "with_payload": False, "with_vector": False,
                "params": {"exact": exact, "hnsw_ef": ef}}
        return [int(p["id"]) for p in self.rest.request("POST", "/collections/bench/points/query", body,
                                                             idempotent=True)["result"]["points"]]

    def first_query_after_reopen(self, query, k, ef):
        self.wait_green(self.args.build_timeout, self.mode == "ann")
        return self.search(query, k, ef)


class QdrantGrpcServer(QdrantServer):
    grpc = True


class PgvectorServer(ServerEngine):
    internal_ports = ("5432/tcp",)

    def start(self):
        self.image = self.args.pgvector_image
        settings = ["shared_buffers=1GB", "maintenance_work_mem=1GB"]
        self.start_container(["--shm-size", "1g", "--env", f"POSTGRES_PASSWORD={self.token}",
                              "--publish", "127.0.0.1::5432",
                              "--mount", f"type=volume,src={self.volume},dst=/var/lib/postgresql/data",
                              self.image, "postgres"] + [x for s in settings for x in ("-c", s)])
        self.connect(create=True)
        with self.conn.cursor() as cur:
            cur.execute("SHOW server_version")
            server = cur.fetchone()[0]
            cur.execute("SELECT extversion FROM pg_extension WHERE extname = 'vector'")
            ext = cur.fetchone()[0]
            cur.execute(f"CREATE TABLE items (id bigint PRIMARY KEY, embedding vector({self.dimensions}))")
        self.conn.commit()
        self.version = f"{self.image} (PostgreSQL {server}, pgvector {ext})"
        self.params = ("PostgreSQL wire protocol (psycopg 3); COPY BINARY per batch transaction; "
                       f"{', '.join(settings)}, shm 1g; exact = sequential scan ORDER BY <-> (no index); "
                       f"ann = HNSW vector_l2_ops m={HNSW_M} ef_construction={HNSW_EF_CONSTRUCTION}, "
                       "hnsw.ef_search per session")

    def connect(self, create=False):
        import psycopg
        from pgvector.psycopg import register_vector

        def probe():
            conn = psycopg.connect(host=self.host, port=self.ports["5432/tcp"], user="postgres",
                                   password=self.token, dbname="postgres", connect_timeout=5)
            try:
                if create:
                    conn.execute("CREATE EXTENSION IF NOT EXISTS vector")
                    conn.commit()
                register_vector(conn)
                conn.execute("SELECT 1")
                return conn
            except Exception:
                conn.close()
                raise
        self.conn = wait_until(probe, "postgres", 180)
        self.conn.execute(f"SET hnsw.ef_search = {int(self.args.ef)}")
        self.conn.commit()

    def disconnect(self):
        if getattr(self, "conn", None):
            self.conn.close()
            self.conn = None

    def ingest(self, vectors):
        size = batch_size(vectors.shape[1])
        for start in range(0, len(vectors), size):
            with self.conn.cursor() as cur:
                with cur.copy("COPY items (id, embedding) FROM STDIN WITH (FORMAT BINARY)") as copy:
                    copy.set_types(["int8", "vector"])
                    for i, row in enumerate(vectors[start:start + size]):
                        copy.write_row((start + i, row))
            self.conn.commit()

    def build(self):
        with self.conn.cursor() as cur:
            cur.execute(f"SET statement_timeout = '{int(self.args.build_timeout)}s'")
            if self.mode == "ann":
                cur.execute(f"CREATE INDEX ON items USING hnsw (embedding vector_l2_ops) "
                            f"WITH (m = {HNSW_M}, ef_construction = {HNSW_EF_CONSTRUCTION})")
            cur.execute("ANALYZE items")
            cur.execute("SET statement_timeout = 0")
        self.conn.commit()

    def search(self, query, k, ef):
        rows = self.conn.execute("SELECT id FROM items ORDER BY embedding <-> %s LIMIT %s",
                                 (np.asarray(query, dtype=np.float32), k)).fetchall()
        self.conn.commit()
        return [int(r[0]) for r in rows]

    def relation_bytes(self):
        row = self.conn.execute("SELECT pg_total_relation_size('items')").fetchone()
        self.conn.commit()
        return int(row[0])

    def first_query_after_reopen(self, query, k, ef):
        return self.search(query, k, ef)


SERVERS = {"vlite-server": VLiteServer, "qdrant": QdrantServer, "qdrant-grpc": QdrantGrpcServer,
           "pgvector": PgvectorServer}
TIERS = {"embedded": EMBEDDED, "server": SERVERS}


def _alarm(signum, frame):
    raise BuildTimeout("build exceeded --build-timeout")


def measure_queries(engine, query_lists, args, result):
    for query in query_lists[:args.warmup]:
        engine.search(query, args.k, args.ef)
    samples, ids = [], []
    for query in query_lists:
        begin = time.perf_counter_ns()
        found = engine.search(query, args.k, args.ef)
        samples.append(time.perf_counter_ns() - begin)
        ids.append(found)
    result.update(summary(samples), samples_ns=samples, ids=ids)


def run_child(args):
    data = np.load(args.data)
    vectors, queries = data["vectors"], data["queries"]
    query_lists = queries.tolist()
    result = {"engine": args.engine, "mode": args.mode, "status": "ok"}
    if args.tier == "embedded":
        workdir = tempfile.mkdtemp(prefix=f"bench-{args.engine}-", dir=args.workdir)
        engine = None
        try:
            baseline_rss = current_rss()
            engine = EMBEDDED[args.engine](workdir, vectors.shape[1], args.mode, args)
            result.update(version=engine.version, params=engine.params)
            start = time.perf_counter_ns()
            engine.ingest(vectors)
            result["ingest_s"] = (time.perf_counter_ns() - start) / 1e9
            signal.signal(signal.SIGALRM, _alarm)
            signal.alarm(int(args.build_timeout))
            start = time.perf_counter_ns()
            try:
                engine.build()
            except BuildTimeout:
                result.update(status="timeout", index_build_s=None)
                print(json.dumps(result))
                return
            finally:
                signal.alarm(0)
            result["index_build_s"] = (time.perf_counter_ns() - start) / 1e9
            result["rss_growth_bytes"] = current_rss() - baseline_rss
            measure_queries(engine, query_lists, args, result)
            engine.close()
            if engine.footprint is None:
                result.update(footprint_bytes=None, reopen_first_query_s=None)
            else:
                result["footprint_bytes"] = engine.footprint()
                start = time.perf_counter_ns()
                engine.reopen()
                engine.first_query_after_reopen(query_lists[0], args.k, args.ef)
                result["reopen_first_query_s"] = (time.perf_counter_ns() - start) / 1e9
                engine.close()
            result["peak_rss_bytes"] = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * 1024
        finally:
            shutil.rmtree(workdir, ignore_errors=True)
    else:
        engine = SERVERS[args.engine](args.container_name, vectors.shape[1], args.mode, args)
        try:
            engine.start()
            result.update(version=engine.version, image_id=engine.image_id, params=engine.params,
                          container_limits={"cpus": args.cpus, "memory": args.memory})
            baseline = engine.memory()
            start = time.perf_counter_ns()
            engine.ingest(vectors)
            result["ingest_s"] = (time.perf_counter_ns() - start) / 1e9
            start = time.perf_counter_ns()
            try:
                engine.build()
            except (BuildTimeout, TimeoutError) as error:
                result.update(status="timeout", index_build_s=None, error=str(error)[:300])
                print(json.dumps(result))
                return
            result["index_build_s"] = (time.perf_counter_ns() - start) / 1e9
            result["container_memory_after_build_bytes"] = engine.memory()
            result["rss_growth_bytes"] = result["container_memory_after_build_bytes"] - baseline
            measure_queries(engine, query_lists, args, result)
            if isinstance(engine, PgvectorServer):
                result["relation_bytes"] = engine.relation_bytes()
            engine.stop()
            result["footprint_bytes"] = engine.footprint()
            start = time.perf_counter_ns()
            engine.restart()
            engine.first_query_after_reopen(query_lists[0], args.k, args.ef)
            result["reopen_first_query_s"] = (time.perf_counter_ns() - start) / 1e9
        finally:
            engine.cleanup()
    print(json.dumps(result))


# ------------------------------------------------------------------ reporting

COLUMNS = [("p50_ms", "p50 ms", "{:.3f}"), ("p95_ms", "p95 ms", "{:.3f}"), ("sequential_qps", "QPS", "{:,.0f}"),
           ("recall_at_k", "recall@10", "{:.4f}"), ("ingest_s", "ingest s", "{:.2f}"),
           ("index_build_s", "build s", "{:.2f}"), ("ingest_plus_build_s", "ingest+build s", "{:.2f}"),
           ("rss_growth_bytes", "RSS growth MiB", "MiB"),
           ("footprint_bytes", "disk MiB", "MiB"), ("reopen_first_query_s", "reopen+1st query s", "{:.2f}")]


def median_row(runs, key):
    values = [r.get(key) for r in runs if r.get(key) is not None]
    return statistics.median(values) if values else None


def summarize(paths):
    for path in paths:
        report = json.loads(Path(path).read_text())
        w = report["workload"]
        print(f"\n### {Path(path).name}: tier={report['tier']} {w['records']:,} x {w['dimensions']}d, "
              f"{w['queries']} queries, k={w['k']}, ef_search={w['ef']}, trials={w['trials']}, "
              f"batch={report['ingest_batch']}\n")
        print("| mode | engine | " + " | ".join(c[1] for c in COLUMNS) + " |")
        print("|---|---|" + "---|" * len(COLUMNS))
        for mode in ("exact", "ann"):
            for engine in report["engines"]:
                runs = [r for r in report["runs"] if r["engine"] == engine and r["mode"] == mode]
                if not runs:
                    continue
                if runs[0]["status"] == "n/a":
                    print(f"| {mode} | {engine} | " + " | ".join("N/A" for _ in COLUMNS) + " |")
                    continue
                ok = [r for r in runs if r["status"] == "ok"]
                if not ok:
                    print(f"| {mode} | {engine} | {runs[0]['status']} " + "| " * (len(COLUMNS) - 1) + "|")
                    continue
                cells = []
                for key, _, fmt in COLUMNS:
                    value = median_row(ok, key)
                    if value is None:
                        cells.append("N/A")
                    elif fmt == "MiB":
                        cells.append(f"{value / 2**20:.1f}")
                    else:
                        cells.append(fmt.format(value))
                note = "" if len(ok) == len(runs) else f" ({len(ok)}/{len(runs)} ok)"
                print(f"| {mode} | {engine}{note} | " + " | ".join(cells) + " |")
        print("\nVersions: " + "; ".join(f"{e}: {v}" for e, v in report.get("versions", {}).items()))


# ----------------------------------------------------------------------- main

def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--child", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--engine", help=argparse.SUPPRESS)
    parser.add_argument("--mode", choices=["exact", "ann"], help=argparse.SUPPRESS)
    parser.add_argument("--data", help=argparse.SUPPRESS)
    parser.add_argument("--container-name", help=argparse.SUPPRESS)
    parser.add_argument("--summary", nargs="+", metavar="RESULT_JSON", help="print median Markdown tables and exit")
    parser.add_argument("--tier", choices=sorted(TIERS), default="embedded")
    parser.add_argument("--engines", help="comma-separated subset; default all engines of the tier")
    parser.add_argument("--modes", default="exact,ann")
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
    parser.add_argument("--build-timeout", type=float, default=1200, help="seconds; longer builds are recorded as timeout")
    parser.add_argument("--workdir", help="scratch directory for data and embedded databases (default: system temp)")
    parser.add_argument("--cpus", type=float, default=4, help="server tier: --cpus for every container")
    parser.add_argument("--memory", default="4g", help="server tier: --memory for every container")
    parser.add_argument("--published-ports", action="store_true",
                        help="server tier: connect via 127.0.0.1 published ports (docker-proxy) instead of container IPs")
    parser.add_argument("--vlite-image", default="ghcr.io/mahpate/vlite-api:v0.3.0")
    parser.add_argument("--qdrant-image", default="qdrant/qdrant:v1.15.5")
    parser.add_argument("--pgvector-image", default="pgvector/pgvector:0.8.1-pg17")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.summary:
        return summarize(args.summary)
    if args.child:
        return run_child(args)
    if args.output is None or args.output.exists():
        parser.error("--output must be a new file")
    registry = TIERS[args.tier]
    engines = args.engines.split(",") if args.engines else list(registry)
    modes = args.modes.split(",")
    unknown = [e for e in engines if e not in registry] + [m for m in modes if m not in ("exact", "ann")]
    if unknown:
        parser.error(f"unknown engines/modes for tier {args.tier}: {unknown}")

    vectors, queries = make_dataset(args)
    checksum = hashlib.sha256(vectors.tobytes() + queries.tobytes()).hexdigest()
    v64, q64 = vectors.astype(np.float64), queries.astype(np.float64)
    distances = (q64 ** 2).sum(1)[:, None] - 2 * q64 @ v64.T + (v64 ** 2).sum(1)[None, :]
    truth = np.argsort(distances, axis=1, kind="stable")[:, :args.k]
    truth_dist = np.take_along_axis(distances, truth, axis=1)

    cpu = platform.processor()
    if os.path.exists("/proc/cpuinfo"):
        with open("/proc/cpuinfo") as cpuinfo:
            cpu = next((line.split(":", 1)[1].strip() for line in cpuinfo if line.startswith("model name")), cpu)
    host = {"platform": platform.platform(), "python": platform.python_version(), "cpu": cpu,
            "cpu_count": os.cpu_count(), "machine": platform.machine(),
            "mem_total_bytes": os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES")}
    if args.tier == "server":
        info = json.loads(docker("info", "--format", "{{json .}}"))
        host["docker"] = {key: info.get(key) for key in
                          ("ServerVersion", "NCPU", "MemTotal", "CgroupVersion", "KernelVersion", "OperatingSystem")}
    try:
        commit = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True,
                                         cwd=Path(__file__).parent, stderr=subprocess.DEVNULL).strip()
    except Exception:  # noqa: BLE001
        commit = None
    report = {
        "benchmark": f"vlite-multi-engine-{args.tier}",
        "status": "running",
        "utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "git_commit": commit,
        "tier": args.tier,
        "engines": engines,
        "workload": {k: v for k, v in vars(args).items()
                     if k in ("records", "dimensions", "queries", "k", "ef", "warmup", "trials", "clusters",
                              "spread", "seed", "build_timeout", "modes")},
        "ingest_batch": batch_size(args.dimensions),
        "hnsw": {"m": HNSW_M, "ef_construction": HNSW_EF_CONSTRUCTION, "ef_search": args.ef,
                 "note": "VLite HnswConfig defaults, applied explicitly to every engine exposing them"},
        "metric": "squared L2 / Euclidean; recall is ID-based and tie-aware against a float64 oracle",
        "latency": "sequential, one client thread, one query in flight; QPS = queries / sum(latency)",
        "dataset_sha256": checksum,
        "host": host,
        "durability": {e: DURABILITY[e] for e in engines},
        "versions": {}, "params": {}, "runs": [],
    }
    if args.tier == "server":
        report["containers"] = {"cpus": args.cpus, "memory": args.memory, "connection":
                                "127.0.0.1 published ports via docker-proxy" if args.published_ports else
                                "container bridge IP from the host (no docker-proxy)", "images": {
            "vlite-server": args.vlite_image, "qdrant": args.qdrant_image, "pgvector": args.pgvector_image}}

    def save():
        args.output.write_text(json.dumps(report, indent=2) + "\n")
    save()
    for mode in modes:
        for engine in engines:
            if mode not in registry[engine].modes:
                report["runs"].append({"engine": engine, "mode": mode, "status": "n/a",
                                       "reason": getattr(registry[engine], "na", {}).get(mode, "unsupported")})
    with tempfile.TemporaryDirectory(prefix="vlite-engines-", dir=args.workdir) as directory:
        data_path = Path(directory) / "data.npz"
        np.savez(data_path, vectors=vectors, queries=queries)
        try:
            for trial in range(args.trials):
                order = engines[trial % len(engines):] + engines[:trial % len(engines)]
                for mode in modes:
                    for engine in order:
                        if mode not in registry[engine].modes:
                            continue
                        name = f"vlite-bench-{engine}-{mode}-{trial}-{secrets.token_hex(4)}"
                        command = [sys.executable, __file__, "--child", "--tier", args.tier, "--engine", engine,
                                   "--mode", mode, "--data", str(data_path), "--k", str(args.k),
                                   "--ef", str(args.ef), "--warmup", str(args.warmup),
                                   "--build-timeout", str(args.build_timeout), "--container-name", name,
                                   "--cpus", str(args.cpus), "--memory", args.memory,
                                   "--vlite-image", args.vlite_image, "--qdrant-image", args.qdrant_image,
                                   "--pgvector-image", args.pgvector_image]
                        if args.published_ports:
                            command.append("--published-ports")
                        if args.workdir:
                            command += ["--workdir", args.workdir]
                        try:
                            completed = subprocess.run(command, capture_output=True, text=True,
                                                       timeout=args.build_timeout + 3600)
                            if completed.returncode != 0:
                                result = {"engine": engine, "mode": mode, "status": "error",
                                          "error": completed.stderr.strip()[-1500:]}
                            else:
                                result = json.loads(completed.stdout.strip().splitlines()[-1])
                        except subprocess.TimeoutExpired:
                            result = {"engine": engine, "mode": mode, "status": "timeout",
                                      "error": "child exceeded build timeout + 3600 s"}
                        if args.tier == "server":
                            docker("rm", "-f", name, check=False)
                            docker("volume", "rm", "-f", name + "-data", check=False)
                        if result["status"] == "ok":
                            scores = [recall(found, truth_dist[i], distances[i], args.k)
                                      for i, found in enumerate(result.pop("ids"))]
                            result.update(recall_at_k=float(np.mean(scores)), min_recall=float(np.min(scores)),
                                          ingest_plus_build_s=result["ingest_s"] + result["index_build_s"])
                            report["versions"][engine] = result.get("version")
                            report["params"][f"{engine}/{mode}"] = result.get("params")
                            print(f"trial {trial} {mode:5} {engine:12}: ingest {result['ingest_s']:.2f}s "
                                  f"build {result['index_build_s']:.2f}s p50 {result['p50_ms']:.3f}ms "
                                  f"p95 {result['p95_ms']:.3f}ms qps {result['sequential_qps']:.0f} "
                                  f"recall {result['recall_at_k']:.4f} rss+ {result['rss_growth_bytes'] / 2**20:.0f}MiB "
                                  f"disk {(result['footprint_bytes'] or 0) / 2**20:.1f}MiB "
                                  f"reopen {result['reopen_first_query_s'] or 0:.2f}s", file=sys.stderr, flush=True)
                        else:
                            print(f"trial {trial} {mode:5} {engine:12}: {result['status']} "
                                  f"{result.get('error', '')[-400:]}", file=sys.stderr, flush=True)
                        result["trial"] = trial
                        report["runs"].append(result)
                        save()
            report["status"] = "complete"
        except BaseException:
            report["status"] = "interrupted"
            raise
        finally:
            save()


if __name__ == "__main__":
    main()
