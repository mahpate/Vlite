"""Float32 entry-point parity, validation, ownership and ABI-1 fallback tests."""
from array import array
import ctypes
import os
from pathlib import Path
import shutil
import sys
import threading
import unittest
from unittest.mock import patch
import uuid

sys.path.insert(0, str(Path(__file__).parent))
from vlite_embedded import NativeLibrary, EmbeddedError

ROOT = Path(__file__).resolve().parents[2]
LIBRARY = Path(os.environ.get("VLITE_TEST_LIBRARY", str(
    ROOT / "target" / "debug" / (
        "vlite_ffi.dll" if sys.platform == "win32" else
        "libvlite_ffi.dylib" if sys.platform == "darwin" else "libvlite_ffi.so"))))


class TypedVectors(unittest.TestCase):
    def setUp(self):
        self.native = NativeLibrary(LIBRARY)
        self.db = self.native.create_in_memory(2, format=5)

    def tearDown(self):
        self.db.close()

    def test_search_and_atomic_bulk_parity(self):
        self.assertTrue(self.native.supports_float32)
        rows = [{"id": "18446744073709551615", "payload_base64": "AP8="},
                {"id": "2", "metadata": {"label": {"type": "string", "value": "a"}}}]
        self.db.batch_put_f32(rows, array("f", [1, 0, 0, 1]), 2, require_native=True)
        self.assertEqual(self.db.get(rows[0]["id"])["payload_base64"], "AP8=")
        for metric in ("squared_l2", "cosine", "inner_product"):
            for explain in (False, True):
                expected = self.db.search([1, 0], 2, metric=metric, explain=explain)
                self.assertEqual(self.db.search_f32(array("f", [1, 0]), 2, metric=metric,
                                                   explain=explain), expected)
        self.assertEqual(self.db.search_f32(array("f", [1, 0]), 2, mode="auto"),
                         self.db.search_auto([1, 0], 2))
        self.db.build_index()
        self.assertEqual(self.db.search_f32(array("f", [1, 0]), 2, mode="auto", explain=True),
                         self.db.search_auto([1, 0], 2, explain=True))
        self.db.create_collection("other", 2)
        self.db.batch_put_f32([{"id": "9"}], array("f", [3, 4]), 2, collection="other")
        self.assertEqual(self.db.search_f32(array("f", [3, 4]), 1, collection="other")[0]["id"], "9")
        self.db.batch_put_f32([], array("f"), 2)
        self.assertEqual(self.db.search_f32(memoryview(array("f", [1, 0])).toreadonly(), 2),
                         self.db.search([1, 0], 2))
        self.assertEqual(self.db.search_f32((ctypes.c_float * 2)(1, 0), 2),
                         self.db.search([1, 0], 2))
        matrix = memoryview(array("f", [1, 0, 0, 1])).cast("B").cast("f", shape=[2, 2])
        self.db.batch_put_f32([{"id": "7"}, {"id": "8"}], matrix, 2)
        self.assertEqual(self.db.get("8")["vector"], [0, 1])

    def test_invalid_buffers_and_recovery(self):
        foreign = ctypes.c_float.__ctype_be__ if sys.byteorder == "little" else ctypes.c_float.__ctype_le__
        for value in (b"12345678", array("d", [1, 0]), (foreign * 2)(1, 0),
                      memoryview(array("f", [1, 2, 3, 4]))[::2],
                      array("f", [float("nan"), 0]), array("f", [float("inf"), 0]),
                      array("f", [0]) * 2097153):
            with self.assertRaises(ValueError):
                self.db.search_f32(value, 1)
        for dims, rows, values in [(0, [], []), (True, [], []), (2, [{"id": "1"}], [1]),
                                   (2, [{"id": "1", "vector": []}], [1, 0]),
                                   (2, [{"id": "1"}] * 4097, [0] * 8194)]:
            with self.assertRaises(ValueError):
                self.db.batch_put_f32(rows, array("f", values), dims)
        for rows in ([{"id": "3"}, {"id": "02"}],
                     [{"id": "3"}, {"id": "4", "payload_base64": "!"}]):
            with self.assertRaises(EmbeddedError):
                self.db.batch_put_f32(rows, array("f", [1, 0, 0, 1]), 2)
            with self.assertRaises(EmbeddedError):
                self.db.get("3")
        for _ in range(50):
            with self.assertRaises(EmbeddedError):
                self.db.search_f32(array("f", [1]), 1)
        self.db.batch_put_f32([{"id": "1"}], array("f", [1, 0]), 2)
        self.assertEqual(self.db.search_f32(array("f", [1, 0]), 1)[0]["id"], "1")

    def test_snapshot_and_response_free_on_decode_failure(self):
        source = array("f", [1, 0])
        native = self.native._float32_functions["vlite_search_f32"]
        def mutate(handle, buffer, length, snapshot, count):
            source[0] = 99
            self.assertEqual(snapshot[0], 1)
            return native(handle, buffer, length, snapshot, count)
        self.native._float32_functions["vlite_search_f32"] = mutate
        self.db.search_f32(source, 1)
        free = self.native._lib.vlite_response_free
        with patch.object(self.native._lib, "vlite_response_free", wraps=free) as freed:
            with patch("vlite_embedded.json.loads", side_effect=ValueError("decode failure")):
                with self.assertRaises(ValueError):
                    self.db.search_f32(array("f", [1, 0]), 1)
            self.assertEqual(freed.call_count, 1)
        with patch.dict(self.native._float32_functions, {"vlite_search_f32": lambda *_: None}):
            with self.assertRaisesRegex(RuntimeError, "null response"):
                self.db.search_f32(array("f", [1, 0]), 1)

    def test_legacy_capability_detection_and_fallback(self):
        real = self.native._lib
        class Legacy:
            def __getattr__(self, name):
                if name in ("vlite_search_f32", "vlite_batch_put_f32"):
                    raise AttributeError(name)
                return getattr(real, name)
        with patch("vlite_embedded.ctypes.CDLL", return_value=Legacy()):
            old = NativeLibrary(LIBRARY)
        self.assertFalse(old.supports_float32)
        with old.create_in_memory(2) as db:
            db.batch_put_f32([{"id": "1"}], array("f", [1, 0]), 2)
            self.assertEqual(db.search_f32(array("f", [1, 0]), 1), db.search([1, 0], 1))
            for action in (lambda: db.search_f32(array("f", [1, 0]), 1, require_native=True),
                           lambda: db.batch_put_f32([], array("f"), 2, require_native=True)):
                with self.assertRaises(EmbeddedError) as caught:
                    action()
                self.assertEqual(caught.exception.code, "unsupported")

    def test_read_only_closed_fork_and_serialization(self):
        directory = Path(__file__).parent / ("typed-test-" + uuid.uuid4().hex)
        directory.mkdir()
        try:
            path = directory / "data.vlite"
            with self.native.create(path, 2) as db:
                db.batch_put_f32([{"id": "1"}], array("f", [1, 0]), 2)
            with self.native.open(path, read_only=True) as db:
                self.assertEqual(db.search_f32(array("f", [1, 0]), 1)[0]["id"], "1")
                with self.assertRaises(EmbeddedError) as caught:
                    db.batch_put_f32([{"id": "2"}], array("f", [0, 1]), 2)
                self.assertEqual(caught.exception.code, "read_only")
            with self.assertRaises(EmbeddedError):
                db.search_f32(array("f", [1, 0]), 1)
            with patch("vlite_embedded.os.getpid", return_value=-1):
                with self.assertRaises(RuntimeError):
                    self.db.search_f32(array("f", [1, 0]), 1)
            errors = []
            def read():
                try:
                    for _ in range(20):
                        self.db.search_f32(array("f", [1, 0]), 1)
                except Exception as error:
                    errors.append(error)
            threads = [threading.Thread(target=read) for _ in range(3)]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join()
            self.assertEqual(errors, [])
        finally:
            shutil.rmtree(directory)


if __name__ == "__main__":
    unittest.main()
