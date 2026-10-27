"""Real-library tests for the property-graph API (compiled into every library since 0.5.1).

Rejection tests only run against an older library built without graph. Set VLITE_TEST_EXPECT_GRAPH=1 or 0 to assert which
variant VLITE_TEST_LIBRARY is, so a wrong build cannot silently skip a suite.
"""
import os
from pathlib import Path
import shutil
import sys
import unittest
import uuid

sys.path.insert(0, os.environ.get("VLITE_TEST_PACKAGE", str(Path(__file__).parent)))
from vlite_embedded import NativeLibrary, EmbeddedError

ROOT = Path(__file__).resolve().parents[2]
LIBRARY = Path(os.environ.get("VLITE_TEST_LIBRARY", str(ROOT / "target" / "debug" / (
    "vlite_ffi.dll" if sys.platform == "win32" else "libvlite_ffi.dylib" if sys.platform == "darwin" else "libvlite_ffi.so"))))
NATIVE = NativeLibrary(LIBRARY)
GRAPH = NATIVE.supports_graph


def person(name):
    return {"name": {"type": "string", "value": name}}


class GraphBase(unittest.TestCase):
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

    def error(self, code, action):
        with self.assertRaises(EmbeddedError) as caught:
            action()
        self.assertEqual(caught.exception.code, code, str(caught.exception))


class Capability(unittest.TestCase):
    def test_capability_matches_expected_build(self):
        self.assertEqual(NATIVE.capabilities(), {"graph": GRAPH})
        expected = os.environ.get("VLITE_TEST_EXPECT_GRAPH")
        if expected is not None:
            self.assertEqual(GRAPH, expected == "1")

    def test_libraries_predating_graph_report_unsupported(self):
        from vlite_embedded import _graph_call

        def old(_):
            raise EmbeddedError("invalid_input", "unknown variant `graph`, expected one of `features`, `create`")

        with self.assertRaises(EmbeddedError) as caught:
            _graph_call(old, {"op": "list_graphs"})
        self.assertEqual(caught.exception.code, "unsupported")

        def bad(_):
            raise EmbeddedError("invalid_input", "unknown field `x`")

        with self.assertRaises(EmbeddedError) as caught:
            _graph_call(bad, {"op": "list_graphs"})
        self.assertEqual(caught.exception.code, "invalid_input")


@unittest.skipUnless(GRAPH, "library built without the graph feature")
class GraphEnabled(GraphBase):
    def populate(self):
        path = self.directory / "graph.vlite"
        db = self.keep(self.native.create(path, 2))
        self.assertTrue(db.is_graph_enabled())
        self.assertEqual(db.list_graphs(), [])
        self.assertEqual(db.create_graph("g"), {"name": "g", "node_count": 0, "edge_count": 0})
        db.put("7", [1.0, 0.0])
        result = db.graph_write_batch("g", [
            {"op": "create_node", "key": "a", "labels": ["Person"], "properties": person("Ada")},
            {"op": "create_node", "key": "b", "labels": ["Person"], "properties": person("Bob")},
            {"op": "create_node", "key": "c", "labels": ["City"]},
            {"op": "create_edge", "key": "ab", "source": "a", "relationship": "KNOWS", "destination": "b"},
            {"op": "create_edge", "source": "b", "relationship": "LIVES_IN", "destination": "c"},
            {"op": "attach", "node": "a", "collection": "default", "record": "7"},
            {"op": "put_vector", "collection": "default", "id": "8", "vector": [0.0, 1.0], "payload_base64": "AP8="},
        ])
        return path, db, result

    def test_create_write_read_explain_and_reopen(self):
        path, db, result = self.populate()
        self.assertEqual(result["graph"], "g")
        self.assertIsInstance(result["revision"], str)
        self.assertEqual(len(result["outcomes"]), 7)
        self.assertTrue(result["outcomes"][0]["node"]["created"])
        self.assertEqual(result["outcomes"][6], "vector")
        self.assertEqual(db.get("8")["payload_base64"], "AP8=")
        node_a = result["outcomes"][0]["node"]["id"]

        got = db.graph_get_node("g", int(node_a))
        self.assertEqual(got["node"]["key"], "a")
        self.assertEqual(got["node"]["properties"], person("Ada"))
        self.assertTrue(got["complete"])
        self.assertEqual(db.graph_get_node("g", "a", projection="none")["node"]["properties"], {})
        self.assertEqual(db.graph_get_node("g", {"id": node_a})["node"]["key"], "a")
        self.assertIsNone(db.graph_get_node("g", "missing")["node"])
        self.assertEqual(db.graph_get_edge("g", "ab")["edge"]["target_key"], "b")

        page = db.graph_find_nodes("g", label="Person", limit=1)
        self.assertEqual(len(page["nodes"]), 1)
        following = db.graph_find_nodes("g", label="Person", limit=1, cursor=page["cursor"])
        self.assertNotEqual(following["nodes"][0]["key"], page["nodes"][0]["key"])
        matched = db.graph_find_nodes("g", conditions=[
            {"op": "eq", "field": "name", "value": {"type": "string", "value": "Bob"}}])
        self.assertEqual([node["key"] for node in matched["nodes"]], ["b"])

        self.assertEqual(len(db.graph_neighbors("g", "b", direction="both")["edges"]), 2)
        self.assertEqual(len(db.graph_neighbors("g", "a", types=["LIVES_IN"])["edges"]), 0)
        traversal = db.graph_traverse("g", ["a"], include_paths=True, limits={"max_hops": 2})
        self.assertEqual(len(traversal["nodes"]), 3)
        self.assertTrue(traversal["paths"])
        returned = db.graph_traverse("g", ["a"], return_filter={"labels": ["City"]})
        self.assertEqual([item["node"]["key"] for item in returned["nodes"]], ["c"])
        found = db.graph_shortest_path("g", "a", "c")["shortest_path"]
        self.assertEqual(found["outcome"], "found")
        self.assertEqual(len(found["path"]["edges"]), 2)
        self.assertEqual(db.graph_shortest_path("g", "c", "a")["shortest_path"]["outcome"], "no_path_within_hops")
        self.assertEqual(db.graph_shortest_path("g", "c", "a", direction="in")["shortest_path"]["outcome"], "found")

        plan = db.graph_explain({"op": "neighbors", "graph": "g", "node": {"key": "a"}, "types": ["KNOWS"]})
        self.assertEqual(plan["query"]["op"], "neighbors")
        self.assertEqual(plan["projection"], "all")
        self.assertTrue(plan["indexes"])

        db.create_graph("empty")
        db.drop_graph("empty")
        self.assertEqual([graph["name"] for graph in db.list_graphs()], ["g"])
        db.close()

        reader = self.keep(self.native.open(path, read_only=True))
        self.assertEqual(reader.list_graphs(), [{"name": "g", "node_count": 3, "edge_count": 2}])
        self.assertEqual(reader.graph_get_node("g", "b")["node"]["properties"], person("Bob"))
        self.error("read_only", lambda: reader.graph_write_batch("g", [{"op": "create_node", "key": "z"}]))

    def test_stable_error_codes_and_atomic_batches(self):
        path, db, _ = self.populate()
        self.error("io_error", lambda: self.native.create(path, 2))
        self.error("graph_exists", lambda: db.create_graph("g"))
        self.error("invalid_graph_name", lambda: db.create_graph("Bad"))
        self.error("graph_not_found", lambda: db.graph_get_node("nope", "a"))
        self.error("graph_not_empty", lambda: db.drop_graph("g"))
        self.error("graph_key_exists", lambda: db.graph_write_batch("g", [
            {"op": "create_node", "key": "d"}, {"op": "create_node", "key": "a"}]))
        self.assertIsNone(db.graph_get_node("g", "d")["node"])
        self.error("invalid_input", lambda: db.graph_write_batch("g", [{"op": "create_node", "key": "x", "extra": 1}]))
        self.error("invalid_input", lambda: db.graph_find_nodes("g", cursor="XYZ"))
        self.error("invalid_input", lambda: db.graph_get_node("g", {"id": "07"}))
        self.error("invalid_input", lambda: db.graph_explain({"op": "list_graphs"}))
        self.error("graph_limit_exceeds_ceiling",
                   lambda: db.graph_traverse("g", ["a"], limits={"max_hops": 1000000}))
        with self.assertRaises(TypeError):
            db.graph_get_node("g", 1.5)

        plain = self.keep(self.native.create(self.directory / "plain.vlite", 2, format=4))
        self.assertFalse(plain.is_graph_enabled())
        self.error("graphs_not_enabled", plain.list_graphs)
        self.error("graphs_not_enabled", lambda: plain.create_graph("g"))
        db.close()
        self.error("invalid_handle", db.list_graphs)

    def test_change_feed_reports_graph_events(self):
        _, db, result = self.populate()
        db.create_graph("empty")
        db.drop_graph("empty")
        changes = db.changes_after()["changes"]
        self.assertEqual([change["kind"] for change in changes],
                         ["graph_created", "put", "graph_batch", "put",
                          "graph_created", "graph_dropped"])
        self.assertEqual((changes[0]["graph"], changes[0]["collection"]), ("g", ""))
        first = changes[2]
        self.assertEqual(first["graph"], "g")
        self.assertEqual(first["revision"], result["revision"])
        self.assertEqual([op["op"] for op in first["operations"]],
                         ["node_upserted"] * 3 + ["edge_upserted"] * 2 + ["attached"])
        self.assertEqual(first["operations"][0]["properties"], person("Ada"))
        self.assertIsNone(first["operations"][4]["key"])
        self.assertEqual(changes[3]["id"], "8")
        self.assertEqual(changes[2]["sequence"], changes[3]["sequence"])
        self.assertEqual(changes[5]["graph"], "empty")

    def test_encrypted_graph_feed_decrypts_with_key(self):
        path = self.directory / "secret-feed.vlite"
        key = bytes(range(32))
        db = self.keep(self.native.create(path, 2, key=key))
        db.create_graph("g")
        db.graph_write_batch("g", [{"op": "create_node", "key": "a"}])
        changes = db.changes_after()["changes"]
        self.assertEqual([change["kind"] for change in changes], ["graph_created", "graph_batch"])
        self.assertEqual(changes[1]["operations"][0]["key"], "a")

    def test_encrypted_graph_file_requires_credentials(self):
        path = self.directory / "secret.vlite"
        key = bytes(range(32))
        db = self.keep(self.native.create(path, 2, key=key, commit_timestamps=True))
        db.create_graph("g")
        db.graph_write_batch("g", [{"op": "create_node", "key": "a"}])
        self.assertTrue(db.is_encrypted())
        db.close()
        self.error("encryption_key_required", lambda: self.native.open(path))
        reopened = self.keep(self.native.open(path, key=key))
        self.assertEqual(reopened.list_graphs()[0]["node_count"], 1)


@unittest.skipIf(GRAPH, "library built with the graph feature")
class GraphUnsupported(GraphBase):
    def test_every_graph_method_raises_unsupported(self):
        db = self.keep(self.native.create(self.directory / "plain.vlite", 2))
        for action in [
            db.is_graph_enabled, db.list_graphs, lambda: db.create_graph("g"), lambda: db.drop_graph("g"),
            lambda: db.graph_write_batch("g", [{"op": "create_node", "key": "a"}]),
            lambda: db.graph_get_node("g", "a"), lambda: db.graph_get_edge("g", 1),
            lambda: db.graph_find_nodes("g"), lambda: db.graph_neighbors("g", "a"),
            lambda: db.graph_traverse("g", ["a"]), lambda: db.graph_shortest_path("g", "a", "b"),
            lambda: db.graph_explain({"op": "get_node", "graph": "g", "node": {"key": "a"}}),
        ]:
            self.error("unsupported", action)
        db.put("1", [1.0, 0.0])
        self.assertEqual(db.get("1")["id"], "1")


if __name__ == "__main__":
    unittest.main()
