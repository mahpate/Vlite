"""Embedded VLite using ctypes and an ABI-v1 native shared library."""
import base64
import ctypes
import json
import math
import os
from pathlib import Path
import threading
import sys


class EmbeddedError(Exception):
    def __init__(self, code: str, message: str):
        self.code = code
        super().__init__(f"{code}: {message}")


def _float32_snapshot(vectors):
    # Own a stable snapshot: ctypes releases the GIL during the native call.
    view = memoryview(vectors)
    native_format = "<f" if sys.byteorder == "little" else ">f"
    if not view.c_contiguous or view.format not in ("f", "@f", "=f", native_format) or view.itemsize != 4:
        raise ValueError("vectors must be a C-contiguous native-endian float32 buffer")
    count = view.nbytes // 4
    if count > 2097152:
        raise ValueError("vector buffer exceeds 2097152 float32 values")
    snapshot = (ctypes.c_float * count).from_buffer_copy(view)
    # A float64 sum of float32 values cannot overflow, so it is finite iff every value is.
    if not math.isfinite(sum(memoryview(snapshot).cast("B").cast("f"))):
        raise ValueError("vector values must be finite")
    return snapshot


class NativeLibrary:
    """Load the bundled library, or an explicit ABI-v1 native library path."""

    def __init__(self, path: str | os.PathLike[str] | None = None):
        if path is None:
            name = "vlite_ffi.dll" if sys.platform == "win32" else "libvlite_ffi.dylib" if sys.platform == "darwin" else "libvlite_ffi.so"
            path = Path(__file__).parent / name
            if not path.is_file():
                raise FileNotFoundError("Bundled VLite library is missing; install a platform wheel or supply an explicit native library path")
        self._pid = os.getpid()
        self._lib = ctypes.CDLL(str(Path(path).resolve(strict=True)))
        self._lib.vlite_abi_version.argtypes = []
        self._lib.vlite_abi_version.restype = ctypes.c_uint32
        if self._lib.vlite_abi_version() != 1:
            raise RuntimeError("Unsupported VLite ABI version")
        self._lib.vlite_execute.argtypes = [ctypes.c_uint64, ctypes.c_void_p, ctypes.c_size_t]
        self._lib.vlite_execute.restype = ctypes.c_void_p
        self._lib.vlite_response_free.argtypes = [ctypes.c_void_p]
        self._lib.vlite_response_free.restype = None
        self._float32_functions = {}
        for name in ("vlite_search_f32", "vlite_batch_put_f32"):
            function = getattr(self._lib, name, None)
            if function is not None:
                function.argtypes = [ctypes.c_uint64, ctypes.c_void_p, ctypes.c_size_t,
                                     ctypes.POINTER(ctypes.c_float), ctypes.c_size_t]
                function.restype = ctypes.c_void_p
                self._float32_functions[name] = function

    @property
    def supports_float32(self) -> bool:
        """Whether both optional typed-vector entry points are available."""
        return len(self._float32_functions) == 2

    def _execute(self, handle: int, request: dict):
        if os.getpid() != self._pid:
            raise RuntimeError("VLite cannot be used after fork; use a fresh spawned process")
        data = json.dumps(request, allow_nan=False).encode("utf-8")
        buffer = ctypes.create_string_buffer(data)
        pointer = self._lib.vlite_execute(handle, buffer, len(data))
        return self._response(pointer)

    def _response(self, pointer):
        if not pointer:
            raise RuntimeError("Native library returned a null response; mutation outcome unknown")
        try:
            response = json.loads(ctypes.string_at(pointer))
        finally:
            self._lib.vlite_response_free(pointer)
        if not response["ok"]:
            error = response["error"]
            raise EmbeddedError(error["code"], error["message"])
        return response["result"]

    def create(self, path: str | os.PathLike[str], dimensions: int, *, format: int | None = None,
               key: bytes | None = None, passphrase: str | None = None,
               commit_timestamps: bool = False) -> "Database":
        """Create without overwrite. Default format is 3; 5 enables all new surfaces."""
        if format == 5 or key is not None or passphrase is not None or commit_timestamps:
            return self._create_feature(str(Path(path)), dimensions, format, key, passphrase, commit_timestamps)
        request = {"op": "create", "path": str(Path(path)), "dimensions": dimensions}
        if format is not None:
            request["format"] = format
        return Database(self, int(self._execute(0, request)["handle"]))

    def _feature(self, operation: str, **fields):
        return self._execute(0, {"op": "features", "request": {"op": operation, **fields}})

    def _create_feature(self, path, dimensions, format, key, passphrase, commit_timestamps):
        options = {"credentials": _credentials(key, passphrase), "commit_timestamps": commit_timestamps,
                   **_given(format=format)}
        result = self._feature("create", path=path, dimensions=dimensions, options=options)
        return Database(self, int(result["handle"]))

    def create_in_memory(self, dimensions: int, *, format: int | None = None, key: bytes | None = None,
                         passphrase: str | None = None, commit_timestamps: bool = False) -> "Database":
        """Host-owned byte-image database; persist with Database.to_bytes()."""
        return self._create_feature(None, dimensions, format, key, passphrase, commit_timestamps)

    def open(self, path: str | os.PathLike[str], *, key: bytes | None = None,
             passphrase: str | None = None, read_only: bool = False) -> "Database":
        """Open an existing file. ``read_only=True`` takes a shared OS lock so any
        number of processes can read the same file concurrently; mutations raise
        ``EmbeddedError`` with code ``read_only`` and writers get ``locked``.
        The handle reads a snapshot at open; close and reopen to see new commits.
        C ABI calls are serialized within one process. For parallel reads, spawn
        multiple processes, each opening its own ``read_only=True`` handle.
        Key/passphrase credentials also work with read-only opens.
        """
        if key is None and passphrase is None:
            request = {"op": "open", "path": str(Path(path))}
            if read_only:
                request["read_only"] = True
            result = self._execute(0, request)
        else:
            result = self._feature("open", path=str(Path(path)), credentials=_credentials(key, passphrase),
                                   **({"read_only": True} if read_only else {}))
        return Database(self, int(result["handle"]))

    def open_from_bytes(self, data: bytes, *, key: bytes | None = None,
                        passphrase: str | None = None) -> "Database":
        result = self._feature("open_from_bytes", bytes_base64=_bytes_base64(data),
                               credentials=_credentials(key, passphrase))
        return Database(self, int(result["handle"]))

    def restore_to(self, log_path: str | os.PathLike[str], destination: str | os.PathLike[str], *,
                   sequence: str | int | None = None, timestamp_ms: str | int | None = None,
                   base_backup: str | os.PathLike[str] | None = None,
                   key: bytes | None = None, passphrase: str | None = None) -> dict:
        """Restore a closed log into a new path; choose exactly one sequence or timestamp."""
        return self._feature("restore_to", log_path=str(Path(log_path)), destination=str(Path(destination)),
                             **_given(sequence=None if sequence is None else str(sequence),
                                      timestamp_ms=None if timestamp_ms is None else str(timestamp_ms),
                                      base_backup=None if base_backup is None else str(Path(base_backup))),
                             credentials=_credentials(key, passphrase))


class Database:
    def __init__(self, library: NativeLibrary, handle: int):
        self._library, self._handle = library, handle
        self._lock = threading.Lock()

    def __enter__(self) -> "Database":
        return self

    def __exit__(self, *_):
        self.close()

    def _execute(self, operation: str, **fields):
        if os.getpid() != self._library._pid:
            raise RuntimeError("VLite cannot be used after fork; use a fresh spawned process")
        with self._lock:
            if self._handle is None:
                raise EmbeddedError("invalid_handle", "Database is closed")
            return self._library._execute(self._handle, {"op": operation, **fields})

    def close(self) -> None:
        if os.getpid() != self._library._pid:
            raise RuntimeError("VLite cannot be used after fork; use a fresh spawned process")
        with self._lock:
            if self._handle is not None:
                handle = self._handle
                self._handle = None
                self._library._execute(handle, {"op": "close"})

    def info(self) -> dict:
        return self._execute("info")

    def get(self, id: str, *, collection: str | None = None) -> dict:
        """Return the record, including its current ``revision`` as a decimal string."""
        return self._execute("get", id=id, **_given(collection=collection))

    def get_many(self, ids: list[str], *, collection: str | None = None) -> list[dict | None]:
        return self._execute("get_many", ids=ids, **_given(collection=collection))["records"]

    def put(self, id: str, vector: list[float], *, payload_base64: str = "", metadata: dict | None = None,
            collection: str | None = None, expected_revision: str | None = None,
            expires_at_ms: int | str | None = None) -> str | None:
        """Write a record. With ``expected_revision`` (``"absent"`` or a decimal revision string)
        the write is compare-and-set and returns the new revision; otherwise it returns None."""
        if expires_at_ms is not None:
            if expected_revision is not None:
                raise ValueError("expiry writes do not support expected_revision")
            self.put_with_expiry(id, vector, expires_at_ms=expires_at_ms, payload_base64=payload_base64,
                                 metadata=metadata, collection=collection)
            return None
        result = self._execute("put", id=id, vector=vector, payload_base64=payload_base64,
                               metadata={} if metadata is None else metadata,
                               **_given(collection=collection, expected_revision=expected_revision))
        return None if expected_revision is None else result["revision"]

    def delete(self, id: str, *, collection: str | None = None, expected_revision: str | None = None) -> bool:
        return self._execute("delete", id=id,
                             **_given(collection=collection, expected_revision=expected_revision))["deleted"]

    def batch(self, operations: list[dict], *, collection: str | None = None) -> None:
        self._execute("batch", operations=operations, **_given(collection=collection))

    def revision(self, id: str, *, collection: str | None = None) -> str | None:
        return self._execute("revision", id=id, **_given(collection=collection))["revision"]

    def search(self, vector: list[float], k: int, *, filter: list[dict] | None = None, metric: str | None = None,
               collection: str | None = None, explain: bool = False) -> list[dict] | dict:
        """Exact search. Returns the hit list, or ``{"hits", "stats"}`` when ``explain`` is true."""
        result = self._execute("search", vector=vector, k=k, filter=[] if filter is None else filter,
                               **_given(metric=metric, collection=collection, explain=explain or None))
        return result if explain else result["hits"]

    def search_auto(self, vector: list[float], k: int, *, filter: list[dict] | None = None, metric: str | None = None,
                    collection: str | None = None, ef_search: int | None = None, quantization: str | None = None,
                    rerank_multiplier: int | None = None, explain: bool = False) -> dict:
        """Planned search; ``plan`` and ``stats`` are included only when ``explain`` is true."""
        return self._execute("search", mode="auto", vector=vector, k=k, explain=explain,
                             filter=[] if filter is None else filter,
                             **_given(metric=metric, collection=collection, ef_search=ef_search,
                                      quantization=quantization, rerank_multiplier=rerank_multiplier))

    def _execute_float32(self, name, request, vectors, require_native):
        if os.getpid() != self._library._pid:
            raise RuntimeError("VLite cannot be used after fork; use a fresh spawned process")
        snapshot = _float32_snapshot(vectors)
        with self._lock:
            if self._handle is None:
                raise EmbeddedError("invalid_handle", "Database is closed")
            function = self._library._float32_functions.get(name)
            if function is None:
                if require_native:
                    raise EmbeddedError("unsupported", "Native library lacks the float32 entry point")
                if name == "vlite_search_f32":
                    request = {**request, "vector": list(snapshot)}
                else:
                    dimensions = request["dimensions"]
                    request = {"op": "batch", **_given(collection=request.get("collection")),
                               "operations": [{"op": "put", **row,
                                               "vector": list(snapshot[i * dimensions:(i + 1) * dimensions])}
                                              for i, row in enumerate(request["operations"])]}
                return self._library._execute(self._handle, request)
            data = json.dumps(request, allow_nan=False).encode("utf-8")
            buffer = ctypes.create_string_buffer(data)
            return self._library._response(function(self._handle, buffer, len(data), snapshot, len(snapshot)))

    def search_f32(self, vector, k: int, *, filter: list[dict] | None = None, metric: str | None = None,
                   collection: str | None = None, explain: bool = False, mode: str = "exact",
                   ef_search: int | None = None, quantization: str | None = None,
                   rerank_multiplier: int | None = None, require_native: bool = False) -> list[dict] | dict:
        """Search a contiguous float32 buffer. Exact returns hits (or explained result);
        auto returns the same dict as search_auto; approximate_ready returns the same
        hits as search_approximate_ready. Old libraries fall back to JSON."""
        if mode == "approximate_ready":
            if filter is not None or explain:
                raise ValueError("approximate_ready does not accept filter or explain")
            return self._execute_float32("vlite_search_f32",
                                         {"op": "search_approximate_ready", "k": k,
                                          "ef_search": 100 if ef_search is None else ef_search,
                                          **_given(metric=metric, collection=collection,
                                                   quantization=quantization,
                                                   rerank_multiplier=rerank_multiplier)},
                                         vector, require_native)["hits"]
        if mode not in ("exact", "auto"):
            raise ValueError("mode must be exact, auto or approximate_ready")
        result = self._execute_float32("vlite_search_f32",
                                      {"op": "search", "mode": mode, "k": k, "explain": explain,
                                       "filter": [] if filter is None else filter,
                                       **_given(metric=metric, collection=collection, ef_search=ef_search,
                                                quantization=quantization, rerank_multiplier=rerank_multiplier)},
                                      vector, require_native)
        return result if mode == "auto" or explain else result["hits"]

    def batch_put_f32(self, records: list[dict], vectors, dimensions: int, *,
                      collection: str | None = None, require_native: bool = False) -> None:
        """Atomic put-only batch: records contain id and optional metadata/payload_base64;
        vectors is a flat or row-major contiguous float32 buffer."""
        if isinstance(dimensions, bool) or not isinstance(dimensions, int) or dimensions <= 0:
            raise ValueError("dimensions must be a positive integer")
        if len(records) > 4096 or memoryview(vectors).nbytes != len(records) * dimensions * 4:
            raise ValueError("bulk requires at most 4096 rows and rows * dimensions float32 values")
        if any(not isinstance(row, dict) or "id" not in row
               or row.keys() - {"id", "payload_base64", "metadata"} for row in records):
            raise ValueError("records require id and only optional payload_base64/metadata")
        self._execute_float32("vlite_batch_put_f32",
                              {"dimensions": dimensions, "operations": records,
                               **_given(collection=collection)}, vectors, require_native)

    def search_hybrid(self, text: str, k: int, *, vector: list[float] | None = None, filter: list[dict] | None = None,
                      metric: str | None = None, collection: str | None = None, fusion: dict | None = None,
                      vector_leg: str | None = None, ef_search: int | None = None, quantization: str | None = None,
                      rerank_multiplier: int | None = None, explain: bool = False) -> dict:
        """BM25 over the collection's declared text fields, fused with an optional vector leg."""
        return self._execute("search", mode="hybrid", text=text, k=k, explain=explain,
                             filter=[] if filter is None else filter,
                             **_given(vector=vector, metric=metric, collection=collection, fusion=fusion,
                                      vector_leg=vector_leg, ef_search=ef_search, quantization=quantization,
                                      rerank_multiplier=rerank_multiplier))

    def build_index(self, *, metric: str | None = None, collection: str | None = None, quantization: str | None = None,
                    rerank_multiplier: int | None = None) -> None:
        self._execute("build_index", **_given(metric=metric, collection=collection, quantization=quantization,
                                              rerank_multiplier=rerank_multiplier))

    def search_approximate_ready(self, vector: list[float], k: int, *, ef_search: int = 100, metric: str | None = None,
                                 collection: str | None = None, quantization: str | None = None,
                                 rerank_multiplier: int | None = None) -> list[dict]:
        return self._execute("search_approximate_ready", vector=vector, k=k, ef_search=ef_search,
                             **_given(metric=metric, collection=collection, quantization=quantization,
                                      rerank_multiplier=rerank_multiplier))["hits"]

    def list_collections(self) -> list[dict]:
        return self._execute("list_collections")["collections"]

    def create_collection(self, name: str, dimensions: int, *, settings: dict | None = None) -> dict:
        return self._execute("create_collection", name=name, dimensions=dimensions, **_given(settings=settings))

    def drop_collection(self, name: str) -> None:
        self._execute("drop_collection", name=name)

    def set_collection_settings(self, name: str, settings: dict) -> dict:
        """Full replacement: omitted settings revert to engine defaults. Returns ``{"persisted": bool}``."""
        return self._execute("set_collection_settings", name=name, settings=settings)

    def next_import_job_id(self) -> str:
        return self._execute("next_import_job_id")["job"]

    def import_chunk(self, job: str, chunk: str, operations: list[dict], *, collection: str | None = None,
                     policy: str | None = None) -> dict:
        return self._execute("import_chunk", job=job, chunk=chunk, operations=operations,
                             **_given(collection=collection, policy=policy))

    def import_status(self, job: str) -> dict:
        return self._execute("import_status", job=job)

    def import_finish(self, job: str, *, collection: str | None = None) -> dict:
        return self._execute("import_finish", job=job, **_given(collection=collection))

    def import_abort(self, job: str, *, collection: str | None = None) -> dict:
        return self._execute("import_abort", job=job, **_given(collection=collection))

    def ann_quality(self, *, collection: str | None = None, metric: str | None = None, quantization: str | None = None,
                    rerank_multiplier: int | None = None, samples: int | None = None, k: int | None = None,
                    ef_search: int | None = None, seed: str | None = None,
                    max_distance_evaluations: int | None = None) -> dict:
        return self._execute("ann_quality", **_given(
            collection=collection, metric=metric, quantization=quantization, rerank_multiplier=rerank_multiplier,
            samples=samples, k=k, ef_search=ef_search, seed=seed, max_distance_evaluations=max_distance_evaluations))

    def ann_memory(self, *, collection: str | None = None, metric: str | None = None, quantization: str | None = None,
                   rerank_multiplier: int | None = None) -> dict:
        return self._execute("ann_memory", **_given(collection=collection, metric=metric, quantization=quantization,
                                                    rerank_multiplier=rerank_multiplier))

    def _feature(self, operation: str, **fields):
        return self._execute("features", request={"op": operation, **fields})

    def put_with_expiry(self, id: str, vector: list[float], *, expires_at_ms: int | str | None = None,
                        payload_base64: str = "", metadata: dict | None = None,
                        collection: str | None = None) -> str:
        """Put with absolute Unix-ms expiry (None means no expiry); returns revision. Requires V5."""
        return self._feature("put_with_expiry", id=id, vector=vector, payload_base64=payload_base64,
                             metadata={} if metadata is None else metadata,
                             expires_at_ms=None if expires_at_ms is None else str(expires_at_ms),
                             **_given(collection=collection))["revision"]

    def update_metadata(self, id: str, metadata: dict, *, collection: str | None = None) -> str:
        """Replace metadata, retaining vectors, payload, and expiry; returns revision. Requires V5."""
        return self._feature("update_metadata", id=id, metadata=metadata, **_given(collection=collection))["revision"]

    def search_grouped(self, vector: list[float], groups: int, hits_per_group: int, field: str, *,
                       filter: list[dict] | None = None, metric: str | None = None,
                       collection: str | None = None) -> list[dict]:
        return self._feature("search_grouped", vector=vector, groups=groups, hits_per_group=hits_per_group,
                             field=field, filter=[] if filter is None else filter, **_given(metric=metric, collection=collection))

    def recommend(self, k: int, *, positive_ids: list[str] | None = None, negative_ids: list[str] | None = None,
                  positive_vectors: list[list[float]] | None = None, negative_vectors: list[list[float]] | None = None,
                  filter: list[dict] | None = None, metric: str | None = None, collection: str | None = None) -> list[dict]:
        return self._feature("recommend", k=k, positive_ids=[] if positive_ids is None else positive_ids,
                             negative_ids=[] if negative_ids is None else negative_ids,
                             positive_vectors=[] if positive_vectors is None else positive_vectors,
                             negative_vectors=[] if negative_vectors is None else negative_vectors,
                             filter=[] if filter is None else filter, **_given(metric=metric, collection=collection))

    def facets(self, field: str, *, filter: list[dict] | None = None, collection: str | None = None) -> list[dict]:
        return self._feature("facets", field=field, filter=[] if filter is None else filter, **_given(collection=collection))

    def numeric_stats(self, field: str, *, filter: list[dict] | None = None, collection: str | None = None) -> dict:
        return self._feature("numeric_stats", field=field, filter=[] if filter is None else filter, **_given(collection=collection))

    def search_geo(self, vector: list[float], k: int, field: str, latitude: float, longitude: float,
                   radius_meters: float, *, sort: str = "vector_distance", filter: list[dict] | None = None,
                   metric: str | None = None, collection: str | None = None) -> list[dict]:
        return self._feature("search_geo", vector=vector, k=k, field=field, latitude=latitude, longitude=longitude,
                             radius_meters=radius_meters, sort=sort, filter=[] if filter is None else filter,
                             **_given(metric=metric, collection=collection))

    def configure_vector(self, name: str, dimensions: int, *, active_version: str, metric: str = "squared_l2",
                         history: int = 0, collection: str | None = None) -> None:
        self._feature("configure_vector", name=name, dimensions=dimensions, active_version=active_version,
                      metric=metric, history=history, **_given(collection=collection))

    def vector_specs(self, *, collection: str | None = None) -> dict:
        return self._feature("vector_specs", **_given(collection=collection))

    def record_vectors(self, id: str, *, collection: str | None = None) -> dict | None:
        return self._feature("record_vectors", id=id, **_given(collection=collection))

    def put_vectors(self, id: str, *, dense: dict | None = None, sparse: dict | None = None,
                    collection: str | None = None) -> None:
        """Replace all additional vectors: dense name -> [{version, vector}], sparse name -> [(index, weight)]."""
        self._feature("put_vectors", id=id, dense={} if dense is None else dense, sparse={} if sparse is None else sparse,
                      **_given(collection=collection))

    def put_vector_version(self, id: str, name: str, version: str, vector: list[float], *, collection: str | None = None) -> None:
        self._feature("put_vector_version", id=id, name=name, version=version, vector=vector, **_given(collection=collection))

    def prune_vector_history(self, name: str, history: int, *, collection: str | None = None) -> None:
        self._feature("prune_vector_history", name=name, history=history, **_given(collection=collection))

    def search_named(self, name: str, vector: list[float], k: int, *, version: str | None = None,
                     collection: str | None = None) -> list[dict]:
        return self._feature("search_named", name=name, vector=vector, k=k, **_given(version=version, collection=collection))

    def search_sparse(self, name: str, vector: list[tuple[int, float]], k: int, *, collection: str | None = None) -> list[dict]:
        return self._feature("search_sparse", name=name, vector=vector, k=k, **_given(collection=collection))

    def search_dense_sparse(self, dense_name: str, dense: list[float], sparse_name: str, sparse: list[tuple[int, float]],
                            k: int, *, version: str | None = None, fusion: dict | None = None,
                            collection: str | None = None) -> list[dict]:
        return self._feature("search_dense_sparse", dense_name=dense_name, dense=dense, sparse_name=sparse_name,
                             sparse=sparse, k=k, **_given(version=version, fusion=fusion, collection=collection))

    def rerank(self, vector: list[float], candidates: list[dict], limit: int, *, method: str = "exact",
               metric: str | None = None, lambda_: float | None = None, collection: str | None = None) -> list[dict]:
        return self._feature("rerank", vector=vector, candidates=candidates, limit=limit, method=method,
                             **_given(metric=metric, collection=collection, **{"lambda": lambda_}))

    def query(self, query: str, parameters: dict[str, list[float]] | None = None, *, collection: str | None = None) -> dict:
        return self._feature("query", query=query, parameters={} if parameters is None else parameters, **_given(collection=collection))

    def feed_cursor(self) -> str:
        return self._feature("feed_cursor")

    def changes_after(self, cursor: str | None = None, max_events: int = 1000) -> dict:
        """Read commits after cursor, or from the beginning when cursor is None. Does not split batch commits."""
        return self._feature("changes_after", cursor=cursor, max_events=max_events)

    def encryption_info(self) -> dict:
        return self._feature("encryption_info")

    def is_encrypted(self) -> bool:
        return self.encryption_info()["is_encrypted"]

    def encryption_key_id(self) -> str | None:
        return self.encryption_info()["key_id"]

    def build_disk_ann(self, path: str | os.PathLike[str], *, metric: str | None = None, collection: str | None = None) -> dict:
        return self._feature("build_disk_ann", path=str(Path(path)), **_given(metric=metric, collection=collection))

    def search_disk_ann(self, path: str | os.PathLike[str], vector: list[float], k: int, *, ef_search: int = 100,
                        cache_bytes: int = 8 * 1024 * 1024) -> list[dict]:
        return self._feature("search_disk_ann", path=str(Path(path)), vector=vector, k=k, ef_search=ef_search, cache_bytes=cache_bytes)

    def to_bytes(self) -> bytes:
        return base64.b64decode(self._feature("to_bytes")["bytes_base64"], validate=True)


def _given(**fields) -> dict:
    """Drop omitted options so the native layer applies the collection's declared defaults."""
    return {name: value for name, value in fields.items() if value is not None}


def _bytes_base64(data: bytes) -> str:
    if not isinstance(data, (bytes, bytearray, memoryview)):
        raise TypeError("expected bytes-like data")
    return base64.b64encode(data).decode("ascii")


def _credentials(key: bytes | None, passphrase: str | None) -> dict:
    if key is not None and passphrase is not None:
        raise ValueError("provide key or passphrase, not both")
    return _given(key_base64=None if key is None else _bytes_base64(key), passphrase=passphrase)
