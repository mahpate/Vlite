"""Embedded VLite using ctypes and an ABI-v1 native shared library."""
import ctypes
import json
import os
from pathlib import Path
import threading
import sys


class EmbeddedError(Exception):
    def __init__(self, code: str, message: str):
        self.code = code
        super().__init__(f"{code}: {message}")


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

    def _execute(self, handle: int, request: dict):
        if os.getpid() != self._pid:
            raise RuntimeError("VLite cannot be used after fork; use a fresh spawned process")
        data = json.dumps(request, allow_nan=False).encode("utf-8")
        buffer = ctypes.create_string_buffer(data)
        pointer = self._lib.vlite_execute(handle, buffer, len(data))
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

    def create(self, path: str | os.PathLike[str], dimensions: int, *, format: int | None = None) -> "Database":
        """Create a database. ``format`` defaults to 3; pass 4 for named collections and imports."""
        request = {"op": "create", "path": str(Path(path)), "dimensions": dimensions}
        if format is not None:
            request["format"] = format
        result = self._execute(0, request)
        return Database(self, int(result["handle"]))

    def open(self, path: str | os.PathLike[str]) -> "Database":
        result = self._execute(0, {"op": "open", "path": str(Path(path))})
        return Database(self, int(result["handle"]))


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
            collection: str | None = None, expected_revision: str | None = None) -> str | None:
        """Write a record. With ``expected_revision`` (``"absent"`` or a decimal revision string)
        the write is compare-and-set and returns the new revision; otherwise it returns None."""
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


def _given(**fields) -> dict:
    """Drop omitted options so the native layer applies the collection's declared defaults."""
    return {name: value for name, value in fields.items() if value is not None}
