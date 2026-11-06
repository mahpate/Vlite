"""Real-library tests for the key-value API (kv_get, kv_put, kv_delete, kv_list).

tests/fixtures/kv/kv_dialect_goldens.json, the file the C ABI, Node, WASM and the MCP/shell tools replay too, runs against
a fresh database per sequence twice: through the raw JSON dialect of the native ``kv`` op (every step) and through the
typed wrappers (the steps they can express), so the wrappers cannot drift from the other surfaces.
"""
import base64
import json
import os
from pathlib import Path
import re
import shutil
import sys
import unittest
import uuid

sys.path.insert(0, os.environ.get("VLITE_TEST_PACKAGE", str(Path(__file__).parent)))
from vlite_embedded import NativeLibrary, EmbeddedError, _kv_call

ROOT = Path(__file__).resolve().parents[2]
LIBRARY = Path(os.environ.get("VLITE_TEST_LIBRARY", str(ROOT / "target" / "debug" / (
    "vlite_ffi.dll" if sys.platform == "win32" else "libvlite_ffi.dylib" if sys.platform == "darwin" else "libvlite_ffi.so"))))
NATIVE = NativeLibrary(LIBRARY)
GOLDENS = json.loads((ROOT / "tests" / "fixtures" / "kv" / "kv_dialect_goldens.json").read_text(encoding="utf-8"))
U64_MAX = 2 ** 64 - 1


def expand(template, bound):
    """`$ref:NAME` becomes the bound version and {"$repeat": [text, count], "$suffix": text} a long string."""
    if isinstance(template, str):
        return bound[template[len("$ref:"):]] if template.startswith("$ref:") else template
    if isinstance(template, list):
        return [expand(item, bound) for item in template]
    if isinstance(template, dict):
        if "$repeat" in template:
            text, count = template["$repeat"]
            return text * count + template.get("$suffix", "")
        return {name: expand(item, bound) for name, item in template.items()}
    return template


def matches(expected, actual, bound):
    """Exact equality, except that `$bind:NAME[>OTHER]` binds a commit version (a canonical decimal u64 above zero)."""
    if isinstance(expected, str) and expected.startswith("$bind:"):
        name, _, floor = expected[len("$bind:"):].partition(">")
        if not (isinstance(actual, str) and re.fullmatch(r"[1-9][0-9]{0,19}", actual) and int(actual) <= U64_MAX):
            return False
        if floor and int(actual) <= int(bound[floor]):
            return False
        bound[name] = actual
        return True
    if isinstance(expected, dict):
        return (isinstance(actual, dict) and expected.keys() == actual.keys()
                and all(matches(expected[name], actual[name], bound) for name in expected))
    if isinstance(expected, list):
        return (isinstance(actual, list) and len(expected) == len(actual)
                and all(matches(item, other, bound) for item, other in zip(expected, actual)))
    return type(expected) is type(actual) and expected == actual


def skipped(node):
    return "python" in node.get("skip", [])


def dialect_int(value):
    assert value is None or type(value) is int, f"{value!r} must be an int"
    return None if value is None else str(value)


def dialect_entry(entry):
    assert type(entry["value"]) is bytes, f"{entry['value']!r} must be bytes"
    assert type(entry["version"]) is int, f"{entry['version']!r} must be an int"
    return {"key": entry["key"], "value_base64": base64.b64encode(entry["value"]).decode("ascii"),
            "version": str(entry["version"]), "expires_at_ms": dialect_int(entry["expires_at_ms"])}


def typed_expected(expected):
    return {"revision": int(expected["revision"])} if isinstance(expected, dict) else expected


def raw_call(db, request):
    return db._execute("kv", request=request)


def typed_call(db, request):
    """Run a golden request through the typed wrappers; the result comes back in the dialect's JSON shape."""
    fields = dict(request)
    op = fields.pop("op")
    address = {"collection": fields.pop("collection"), "namespace": fields.pop("namespace")}
    if op == "get":
        return dialect_entry(db.kv_get(**address, key=fields["key"]))
    if op == "put":
        deadline = fields.get("expires_at_ms")
        result = db.kv_put(**address, key=fields["key"], value=base64.b64decode(fields["value_base64"], validate=True),
                           expected=typed_expected(fields.get("expected", "any")),
                           expires_at_ms=None if deadline is None else int(deadline))
        return {"version": dialect_int(result["version"]), "expires_at_ms": dialect_int(result["expires_at_ms"])}
    if op == "delete":
        deleted = db.kv_delete(**address, key=fields["key"], expected=typed_expected(fields.get("expected", "any")))
        assert type(deleted) is bool, f"{deleted!r} must be a bool"
        return {"deleted": deleted}
    page = db.kv_list(**address, prefix=fields.get("prefix", ""), after=fields.get("after"), limit=fields.get("limit", 100))
    return {"items": [dialect_entry(item) for item in page["items"]], "next_after": page["next_after"]}


class KvBase(unittest.TestCase):
    def setUp(self):
        self.native = NATIVE
        self.directory = Path(__file__).parent / ("test-data-" + uuid.uuid4().hex)
        self.directory.mkdir()
        self.handles = []

    def tearDown(self):
        for db in reversed(self.handles):
            db.close()
        shutil.rmtree(self.directory)

    def keep(self, db):
        self.handles.append(db)
        return db

    def memory(self):
        return self.keep(self.native.create_in_memory(1))

    def error(self, code, action):
        with self.assertRaises(EmbeddedError) as caught:
            action()
        self.assertEqual(caught.exception.code, code, str(caught.exception))


class Goldens(KvBase):
    def database(self, sequence):
        if sequence["mode"] == "read_only":
            path = self.directory / (sequence["name"] + ".vlite")
            writer = self.native.create(path, 1)
            for seed in sequence["seed"]:
                raw_call(writer, expand(seed, {}))
            writer.close()
            return self.keep(self.native.open(path, read_only=True))
        db = self.memory()
        for collection in sequence.get("collections", []):
            db.create_collection(collection["name"], collection["dimensions"])
        return db

    def replay(self, call, typed):
        replayed = 0
        for sequence in GOLDENS["sequences"]:
            if skipped(sequence) or (typed and sequence.get("typed") is False):
                continue
            with self.subTest(sequence=sequence["name"], typed=typed):
                db = self.database(sequence)
                bound = {}
                for step in sequence["steps"]:
                    if skipped(step) or (typed and step.get("typed") is False):
                        continue
                    request = expand(step["request"], bound)
                    context = f"{sequence['name']}/{step['name']}: {json.dumps(request)[:300]}"
                    replayed += 1
                    try:
                        result = call(db, request)
                    except EmbeddedError as error:
                        self.assertEqual(step.get("error"), error.code, f"{context} -> {error}"[:600])
                        continue
                    self.assertIn("result", step, f"{context} -> unexpected success {str(result)[:300]}")
                    expected = expand(step["result"], bound)
                    self.assertTrue(matches(expected, result, bound), f"{context} -> {str(result)[:300]}")
        return replayed

    def test_raw_dialect_replays_every_golden_step(self):
        expected = sum(1 for sequence in GOLDENS["sequences"] if not skipped(sequence)
                       for step in sequence["steps"] if not skipped(step))
        self.assertEqual(self.replay(raw_call, typed=False), expected)

    def test_typed_wrappers_replay_every_expressible_golden_step(self):
        self.assertGreater(self.replay(typed_call, typed=True), 100)


class Wrappers(KvBase):
    def test_values_are_bytes_and_versions_and_deadlines_are_ints(self):
        db = self.memory()
        address = {"collection": "default", "namespace": "n"}
        put = db.kv_put(**address, key="a", value=b"\x00\xff", expires_at_ms=2 ** 53 + 1)
        self.assertEqual(put["expires_at_ms"], 2 ** 53 + 1)
        self.assertIs(type(put["version"]), int)
        entry = db.kv_get(**address, key="a")
        self.assertEqual(entry, {"key": "a", "value": b"\x00\xff", "version": put["version"], "expires_at_ms": 2 ** 53 + 1})
        self.assertIs(type(entry["value"]), bytes)
        self.assertEqual(db.kv_put(**address, key="b", value=bytearray(b"x"), expires_at_ms=U64_MAX)["expires_at_ms"], U64_MAX)
        self.assertEqual(db.kv_put(**address, key="c", value=memoryview(b"yz"), expires_at_ms=0)["expires_at_ms"], 0)
        self.assertEqual(db.kv_get(**address, key="b")["value"], b"x")
        self.error("kv_not_found", lambda: db.kv_get(**address, key="c"))
        page = db.kv_list(**address)
        self.assertEqual([item["key"] for item in page["items"]], ["a", "b"])
        self.assertIsNone(page["next_after"])
        self.assertEqual(db.kv_put(**address, key="a", value=b"", expires_at_ms=None)["expires_at_ms"], None)
        self.assertEqual(db.kv_get(**address, key="a")["value"], b"")

    def test_compare_and_set_takes_ints(self):
        db = self.memory()
        address = {"collection": "default", "namespace": "n", "key": "a"}
        first = db.kv_put(**address, value=b"1", expected="absent")["version"]
        self.error("precondition_failed", lambda: db.kv_put(**address, value=b"2", expected="absent"))
        second = db.kv_put(**address, value=b"2", expected={"revision": first})["version"]
        self.assertGreater(second, first)
        self.error("precondition_failed", lambda: db.kv_delete(**address, expected={"revision": first}))
        self.assertTrue(db.kv_delete(**address, expected={"revision": second}))
        self.assertFalse(db.kv_delete(**address))
        self.assertFalse(db.kv_delete(**address, expected="absent"))

    def test_paging_follows_next_after(self):
        db = self.memory()
        address = {"collection": "default", "namespace": "n"}
        for index in range(25):
            db.kv_put(**address, key=f"k{index:02}", value=bytes([index]))
        seen, after = [], None
        while True:
            page = db.kv_list(**address, prefix="k", after=after, limit=10)
            self.assertLessEqual(len(page["items"]), 10)
            seen += [(item["key"], item["value"]) for item in page["items"]]
            after = page["next_after"]
            if after is None:
                break
        self.assertEqual(seen, [(f"k{index:02}", bytes([index])) for index in range(25)])

    def test_arguments_are_checked_before_the_native_call(self):
        db = self.memory()
        address = {"collection": "default", "namespace": "n"}
        for action, error in [
            (lambda: db.kv_put(**address, key="a", value="text"), TypeError),
            (lambda: db.kv_put(**address, key="a", value=[1, 2]), TypeError),
            (lambda: db.kv_put(**address, key=b"a", value=b""), TypeError),
            (lambda: db.kv_get(collection=None, namespace="n", key="a"), TypeError),
            (lambda: db.kv_put(**address, key="a", value=b"", expires_at_ms=1.5), TypeError),
            (lambda: db.kv_put(**address, key="a", value=b"", expires_at_ms="1"), TypeError),
            (lambda: db.kv_put(**address, key="a", value=b"", expires_at_ms=True), TypeError),
            (lambda: db.kv_put(**address, key="a", value=b"", expires_at_ms=-1), ValueError),
            (lambda: db.kv_put(**address, key="a", value=b"", expires_at_ms=2 ** 64), ValueError),
            (lambda: db.kv_put(**address, key="a", value=b"", expected="present"), ValueError),
            (lambda: db.kv_put(**address, key="a", value=b"", expected={"revision": "1"}), TypeError),
            (lambda: db.kv_put(**address, key="a", value=b"", expected={"revision": 1, "x": 1}), ValueError),
            (lambda: db.kv_put(**address, key="a", value=b"", expected=None), ValueError),
            (lambda: db.kv_delete(**address, key="a", expected={"revision": -1}), ValueError),
            (lambda: db.kv_list(**address, limit=True), TypeError),
            (lambda: db.kv_list(**address, limit=1.0), TypeError),
            (lambda: db.kv_list(**address, limit=-1), ValueError),
            (lambda: db.kv_list(**address, prefix=None), TypeError),
            (lambda: db.kv_list(**address, after=1), TypeError),
        ]:
            self.assertRaises(error, action)
        with self.assertRaises(TypeError):
            db.kv_get("default", "n", "a")
        self.error("kv_not_found", lambda: db.kv_get(**address, key="a"))

    def test_bounds_and_missing_collections_come_from_the_native_layer(self):
        db = self.memory()
        address = {"collection": "default", "namespace": "n"}
        self.error("invalid_input", lambda: db.kv_get(collection="default", namespace="", key="a"))
        self.error("invalid_input", lambda: db.kv_list(**address, limit=0))
        self.error("invalid_input", lambda: db.kv_put(**address, key="a", value=bytes(1047291)))
        self.assertEqual(db.kv_put(**address, key="a", value=bytes(1047290))["expires_at_ms"], None)
        self.assertEqual(len(db.kv_get(**address, key="a")["value"]), 1047290)
        self.error("collection_not_found", lambda: db.kv_put(collection="missing", namespace="n", key="a", value=b""))
        self.error("collection_not_found", lambda: db.kv_list(collection="missing", namespace="n"))
        self.assertEqual([item["name"] for item in db.list_collections()], ["default"])
        db.create_collection("wide", 2)
        self.error("not_kv_collection", lambda: db.kv_get(collection="wide", namespace="n", key="a"))

    def test_a_closed_database_is_invalid(self):
        db = self.native.create_in_memory(1)
        db.close()
        self.error("invalid_handle", lambda: db.kv_get(collection="default", namespace="n", key="a"))

    def test_values_survive_reopen_and_read_only_handles_refuse_writes(self):
        path = self.directory / "kv.vlite"
        db = self.native.create(path, 1)
        address = {"collection": "default", "namespace": "n", "key": "a"}
        version = db.kv_put(**address, value=b"kept", expires_at_ms=U64_MAX)["version"]
        db.close()
        reader = self.keep(self.native.open(path, read_only=True))
        self.assertEqual(reader.kv_get(**address), {"key": "a", "value": b"kept", "version": version, "expires_at_ms": U64_MAX})
        self.error("read_only", lambda: reader.kv_put(**address, value=b"x", expected="absent"))
        self.error("read_only", lambda: reader.kv_delete(**address))
        self.assertEqual(reader.kv_list(collection="default", namespace="n")["items"][0]["version"], version)

    def test_a_format_4_file_stores_values_but_expiry_needs_an_upgrade(self):
        db = self.keep(self.native.create(self.directory / "v4.vlite", 1, format=4))
        address = {"collection": "default", "namespace": "n", "key": "a"}
        self.assertEqual(db.kv_put(**address, value=b"v")["expires_at_ms"], None)
        self.error("upgrade_required", lambda: db.kv_put(**address, value=b"v", expires_at_ms=1))
        self.assertEqual(db.kv_get(**address)["value"], b"v")


class Unsupported(unittest.TestCase):
    def test_libraries_predating_the_kv_op_report_unsupported(self):
        def old(_):
            raise EmbeddedError("invalid_input", "unknown variant `kv`, expected one of `features`, `graph`, `create`")

        for operation in ("get", "put", "delete", "list"):
            with self.assertRaises(EmbeddedError) as caught:
                _kv_call(old, {"op": operation})
            self.assertEqual(caught.exception.code, "unsupported")

    def test_libraries_predating_one_operation_report_unsupported(self):
        def old(_):
            raise EmbeddedError("invalid_request", "unknown variant `batch`, expected one of `get`, `put`, `delete`, `list`")

        with self.assertRaises(EmbeddedError) as caught:
            _kv_call(old, {"op": "batch"})
        self.assertEqual(caught.exception.code, "unsupported")

    def test_other_errors_pass_through(self):
        for error in (EmbeddedError("invalid_input", "unknown field `x`"),
                      EmbeddedError("invalid_request", "unknown variant `batch`, expected `any` or `absent`"),
                      EmbeddedError("kv_not_found", "key is absent or expired")):
            def failing(_, error=error):
                raise error

            with self.assertRaises(EmbeddedError) as caught:
                _kv_call(failing, {"op": "get"})
            self.assertIs(caught.exception, error)

    def test_the_real_library_words_unknown_operations_as_detected(self):
        with self.assertRaises(EmbeddedError) as caught:
            NATIVE._execute(0, {"op": "zzz"})
        self.assertEqual(caught.exception.code, "invalid_input")
        self.assertIn("unknown variant `zzz`", str(caught.exception))
        db = NATIVE.create_in_memory(1)
        try:
            with self.assertRaises(EmbeddedError) as caught:
                raw_call(db, {"op": "zzz"})
            self.assertEqual(caught.exception.code, "invalid_request")
            self.assertIn("unknown variant `zzz`", str(caught.exception))
            with self.assertRaises(EmbeddedError) as caught:
                db._kv("zzz")
            self.assertEqual(caught.exception.code, "unsupported")
        finally:
            db.close()


if __name__ == "__main__":
    unittest.main()
