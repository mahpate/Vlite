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

    def create(self, path: str | os.PathLike[str], dimensions: int) -> "Database":
        result = self._execute(0, {"op": "create", "path": str(Path(path)), "dimensions": dimensions})
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

    def get(self, id: str) -> dict:
        return self._execute("get", id=id)

    def get_many(self, ids: list[str]) -> list[dict | None]:
        return self._execute("get_many", ids=ids)["records"]

    def put(self, id: str, vector: list[float], *, payload_base64: str = "", metadata: dict | None = None) -> None:
        self._execute("put", id=id, vector=vector, payload_base64=payload_base64,
                      metadata={} if metadata is None else metadata)

    def delete(self, id: str) -> bool:
        return self._execute("delete", id=id)["deleted"]

    def batch(self, operations: list[dict]) -> None:
        self._execute("batch", operations=operations)

    def search(self, vector: list[float], k: int, *, filter: list[dict] | None = None) -> list[dict]:
        return self._execute("search", vector=vector, k=k, filter=[] if filter is None else filter)["hits"]

    def build_index(self) -> None:
        self._execute("build_index")

    def search_approximate_ready(self, vector: list[float], k: int, *, ef_search: int = 100) -> list[dict]:
        return self._execute("search_approximate_ready", vector=vector, k=k, ef_search=ef_search)["hits"]
