"""Exercise the C ABI and Python embedded binding without a server."""
import ctypes
import base64
import json
from pathlib import Path
import subprocess
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "bindings" / "python"))
from vlite_embedded import EmbeddedError, NativeLibrary  # noqa: E402


def fails(code, action):
    try:
        action()
        raise AssertionError("Expected " + code)
    except EmbeddedError as error:
        assert error.code == code, str(error)


def main():
    library_path = str(Path(sys.argv[1]).resolve())
    native = NativeLibrary(library_path)
    # Direct ABI invalid-pointer/length checks use null only, never dangling memory.
    for pointer, size in [(None, 0), (None, 1), (None, 8 * 1024 * 1024 + 1)]:
        result = native._lib.vlite_execute(0, pointer, size)
        try:
            assert json.loads(ctypes.string_at(result))["error"]["code"] == "invalid_input"
        finally:
            native._lib.vlite_response_free(result)
    native._lib.vlite_response_free(None)
    with tempfile.TemporaryDirectory(prefix="vlite-embedded-") as directory:
        path = Path(directory) / "unicode-\u03bb.vlite"
        db = native.create(path, 3)
        fails("locked", lambda: native.open(path))
        fails("io_error", lambda: native.create(path, 3))
        assert db.info() == {"dimensions": 3, "format_version": 3, "records": 0}
        metadata = {"n": {"type": "integer", "value": "-9223372036854775808"}}
        db.put("18446744073709551615", [1, 0, 0], payload_base64="AP8=", metadata=metadata)
        assert db.get("18446744073709551615")["metadata"] == metadata
        assert db.get("18446744073709551615")["payload_base64"] == "AP8="
        assert db.get_many(["18446744073709551615", "0", "18446744073709551615"]) == [
            db.get("18446744073709551615"), None, db.get("18446744073709551615")]
        assert db.get_many([]) == []
        fails("invalid_input", lambda: db.get_many(["1"] * 101))
        fails("invalid_input", lambda: db.get_many(["18446744073709551615", "01"]))
        db.put("8", [0, 0, 1], payload_base64=base64.b64encode(bytes(1024 * 1024)).decode())
        fails("response_too_large", lambda: db.get_many(["8"] * 8))
        assert db.delete("8")
        fails("index_not_ready", lambda: db.search_approximate_ready([1, 0, 0], 1))
        fails("invalid_input", lambda: db.get("01"))
        fails("invalid_input", lambda: db.put("1", [1, 2]))
        fails("invalid_input", lambda: db.put("1", [1, 0, 0], payload_base64="!"))
        fails("invalid_input", lambda: db.batch([
            {"op": "put", "id": "2", "vector": [0, 1, 0]},
            {"op": "put", "id": "3", "vector": [1]},
        ]))
        assert db.info()["records"] == 1
        db.batch([{"op": "put", "id": "2", "vector": [0, 1, 0]},
                  {"op": "delete", "id": "0"}])
        hits = db.search([1, 0, 0], 2, filter=[{"op": "eq", "field": "n", "value": metadata["n"]}])
        assert hits == [{"id": "18446744073709551615", "distance": 0}]
        db.build_index()
        assert db.search_approximate_ready([1, 0, 0], 1)[0]["distance"] == 0
        with ThreadPoolExecutor(max_workers=4) as pool:
            assert all(pool.map(lambda _: db.info()["records"] == 2, range(32)))
        handle = db._handle
        db.close()
        db.close()
        fails("invalid_handle", lambda: db.info())
        fails("invalid_handle", lambda: native._execute(handle, {"op": "info"}))
        with native.open(path) as reopened:
            assert reopened.get("2")["vector"] == [0, 1, 0]
            assert reopened.delete("2") and not reopened.delete("2")
            fails("not_found", lambda: reopened.get("2"))
        # A new interpreter uses the same file and library, not a network service.
        code = """
import sys
from vlite_embedded import NativeLibrary
with NativeLibrary(sys.argv[1]).open(sys.argv[2]) as db:
    assert db.info()['records']==1
    assert db.get('18446744073709551615')['payload_base64']=='AP8='
"""
        import os
        env = dict(os.environ, PYTHONPATH=str(ROOT / "bindings" / "python"))
        subprocess.run([sys.executable, "-c", code, library_path, str(path)],
                       env=env, check=True, timeout=20)
        # Damaged complete frames fail closed, and open never changes the file.
        damaged = bytearray(path.read_bytes())
        damaged[-1] ^= 1
        path.write_bytes(damaged)
        fails("corrupt", lambda: native.open(path))
        assert path.read_bytes() == damaged
    print("PASS C ABI and embedded Python: persistence, locks, Unicode, CRUD, atomicity, metadata, search, threads and corruption")


if __name__ == "__main__":
    main()
