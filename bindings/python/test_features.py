"""Real-library tests for the additive embedded feature API (no server)."""
import base64
import os
from pathlib import Path
import shutil
import subprocess
import sys
import unittest
import uuid

sys.path.insert(0, os.environ.get("VLITE_TEST_PACKAGE", str(Path(__file__).parent)))
from vlite_embedded import NativeLibrary, EmbeddedError

ROOT = Path(__file__).resolve().parents[2]
LIBRARY = Path(os.environ.get("VLITE_TEST_LIBRARY", str(ROOT / "target" / "debug" / (
    "vlite_ffi.dll" if sys.platform == "win32" else "libvlite_ffi.dylib" if sys.platform == "darwin" else "libvlite_ffi.so"))))


def metadata(group="a", score="2", latitude=0):
    return {"group": {"type": "string", "value": group}, "score": {"type": "integer", "value": score},
            "place": {"type": "geopoint", "value": {"latitude": latitude, "longitude": 0}}}


class Features(unittest.TestCase):
    def setUp(self):
        self.native = NativeLibrary(LIBRARY)
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
        return self.keep(self.native.create_in_memory(2, format=5))

    def error(self, code, action):
        with self.assertRaises(EmbeddedError) as caught:
            action()
        self.assertEqual(caught.exception.code, code)

    def test_ttl_metadata_and_aggregated_geo_retrieval(self):
        db = self.memory()
        db.create_collection("docs", 2)
        scope = {"collection": "docs"}
        revision = db.put_with_expiry("1", [1, 0], expires_at_ms="18446744073709551615", metadata=metadata(), payload_base64="AP8=", **scope)
        self.assertIsInstance(revision, str)
        db.put("2", [0, 1], metadata=metadata("b", "4", 1), **scope)
        self.assertNotEqual(db.update_metadata("1", metadata("b"), **scope), revision)
        self.assertEqual(db.get("1", **scope)["payload_base64"], "AP8=")
        self.assertEqual(len(db.search_grouped([1, 0], 2, 2, "group", **scope)), 1)
        self.assertEqual(db.facets("group", **scope)[0]["count"], 2)
        self.assertEqual(db.numeric_stats("score", **scope), {"count": 2, "min": 2, "max": 4, "sum": 6, "average": 3})
        self.assertEqual(db.recommend(2, positive_ids=["1"], **scope)[0]["id"], "2")
        self.assertEqual(db.search_geo([1, 0], 2, "place", 0, 0, 1000, sort="geo_distance", **scope)[0]["id"], "1")
        self.assertEqual(len(db.search_geo([1, 0], 2, "place", 0, 0, 1000, **scope)), 1)
        db.put("3", [1, 1], expires_at_ms=1, **scope)
        self.error("not_found", lambda: db.get("3", **scope))
        self.error("invalid_input", lambda: db.put_with_expiry("4", [1], **scope))
        legacy = self.keep(self.native.create_in_memory(2))
        self.error("requires_format_v5", lambda: legacy.put_with_expiry("1", [1, 0]))
        with self.assertRaises(ValueError):
            db.put("4", [1, 1], expires_at_ms=2, expected_revision="absent")

    def test_named_sparse_hybrid_versions_and_bytes_persistence(self):
        db = self.memory()
        for id in ["1", "2"]:
            db.put(id, [1, 0])
        db.configure_vector("text", 2, active_version="v1", metric="cosine", history=1)
        self.assertEqual(db.vector_specs()["text"]["active_version"], "v1")
        db.put_vectors("1", dense={"text": [{"version": "v1", "vector": [1, 0]}]}, sparse={"lexical": [(1, 2.0)]})
        db.put_vector_version("2", "text", "v1", [0, 1])
        db.put_vector_version("1", "text", "v0", [0, 1])
        self.assertEqual(db.search_named("text", [1, 0], 1)[0]["id"], "1")
        self.assertEqual(db.search_named("text", [0, 1], 1, version="v0")[0]["id"], "1")
        self.assertEqual(db.search_sparse("lexical", [(1, 1)], 2)[0]["distance"], -2)
        self.assertEqual(db.search_dense_sparse("text", [1, 0], "lexical", [(1, 1)], 1, fusion={"rrf_k": 10})[0]["id"], "1")
        self.assertEqual(len(db.record_vectors("1")["dense"]["text"]), 2)
        self.assertIsNone(db.record_vectors("123"))
        self.error("invalid_input", lambda: db.put_vectors("1", sparse={"lexical": [(2, 1), (1, 1)]}))
        image = db.to_bytes()
        reopened = self.keep(self.native.open_from_bytes(image))
        self.assertEqual(reopened.search_named("text", [1, 0], 1)[0]["id"], "1")
        db.prune_vector_history("text", 0)
        self.assertEqual(db.search_named("text", [0, 1], 1, version="v0"), [])
        db.update_metadata("1", {})
        self.assertTrue(db.record_vectors("1"))

    def test_rerank_and_query(self):
        db = self.memory()
        db.put("1", [1, 0], metadata=metadata())
        db.put("2", [0, 1], metadata=metadata("b"))
        candidates = [{"id": "2", "distance": 0}, {"id": "1", "distance": 1}]
        self.assertEqual(db.rerank([1, 0], candidates, 2)[0]["id"], "1")
        self.assertEqual(db.rerank([1, 0], candidates, 2, method="mmr", lambda_=1)[0]["id"], "1")
        self.error("invalid_input", lambda: db.rerank([1, 0], candidates, 1, method="mmr", lambda_=2))
        result = db.query("SEARCH default NEAR $q K 2 GROUP BY group FACET group", {"q": [1, 0]})
        self.assertEqual(result["hits"][0]["id"], "1")
        self.assertEqual(len(result["groups"]), 2)
        self.assertEqual(len(result["facets"]), 2)
        self.error("query_parse_error", lambda: db.query("DELETE everything"))

    def test_feed_events_and_cursors(self):
        db = self.memory()
        cursor = db.feed_cursor()
        db.put_with_expiry("1", [1, 0], metadata=metadata(), expires_at_ms="18446744073709551615")
        db.update_metadata("1", metadata("b"))
        db.delete("1")
        batch = db.changes_after(cursor, 1)
        self.assertEqual(batch["changes"][0]["kind"], "put")
        self.assertEqual(batch["changes"][0]["expires_at_ms"], "18446744073709551615")
        self.assertTrue(batch["has_more"])
        rest = db.changes_after(batch["next"])
        self.assertEqual([c["kind"] for c in rest["changes"]], ["metadata_update", "delete"])
        self.assertEqual(len(db.changes_after()["changes"]), 3)
        self.error("invalid_cursor", lambda: db.changes_after("bad"))
        other = self.memory()
        self.error("cursor_compacted", lambda: other.changes_after(cursor))
        db.batch([{"op": "put", "id": "2", "vector": [1, 0]}, {"op": "put", "id": "3", "vector": [0, 1]}])
        self.assertEqual(len(db.changes_after(rest["next"], 1)["changes"]), 2)

    def test_read_only_handles_and_spawned_process(self):
        for credentials in [{}, {"key": bytes([7] * 32)}, {"passphrase": "reader test secret"}]:
            with self.subTest(credentials=list(credentials)):
                path = self.directory / (next(iter(credentials), "plain") + ".vlite")
                writer = self.keep(self.native.create(path, 2, **credentials))
                writer.put("1", [1, 0])
                self.error("locked", lambda: self.native.open(path, read_only=True, **credentials))
                writer.close()
                first = self.keep(self.native.open(path, read_only=True, **credentials))
                second = self.keep(self.native.open(path, read_only=True, **credentials))
                self.error("locked", lambda: self.native.open(path, **credentials))
                for action in [lambda: first.put("2", [0, 1]), lambda: first.delete("1"),
                               lambda: first.batch([{"op": "put", "id": "2", "vector": [0, 1]},
                                                    {"op": "delete", "id": "1"}])]:
                    self.error("read_only", action)
                    for reader in [first, second]:
                        self.assertEqual(reader.get("1")["vector"], [1, 0])
                        self.assertEqual(reader.search([1, 0], 1)[0]["id"], "1")
                        self.assertEqual(reader.info()["records"], 1)
                script = """
import sys
from vlite_embedded import NativeLibrary, EmbeddedError
native = NativeLibrary(sys.argv[1])
credentials = {} if sys.argv[3] == 'plain' else ({'key': bytes([7] * 32)} if sys.argv[3] == 'key' else {'passphrase': 'reader test secret'})
with native.open(sys.argv[2], read_only=True, **credentials) as reader:
    assert reader.get('1')['vector'] == [1, 0]
    assert reader.search([1, 0], 1)[0]['id'] == '1'
    try:
        native.open(sys.argv[2], **credentials)
    except EmbeddedError as error:
        assert error.code == 'locked', error
    else:
        raise AssertionError('writer unexpectedly opened')
print('shared reader ok')
"""
                env = os.environ.copy()
                env["PYTHONPATH"] = str(Path(sys.path[0]).resolve()) + os.pathsep + env.get("PYTHONPATH", "")
                result = subprocess.run([sys.executable, "-c", script, str(LIBRARY.resolve()),
                                         str(path.resolve()), next(iter(credentials), "plain")],
                                        env=env, capture_output=True, text=True, timeout=60)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn("shared reader ok", result.stdout)
                first.close()
                self.error("locked", lambda: self.native.open(path, **credentials))
                second.close()
                writer = self.keep(self.native.open(path, **credentials))
                writer.put("2", [0, 1])
                self.error("locked", lambda: self.native.open(path, read_only=True, **credentials))
                writer.close()
                reopened = self.keep(self.native.open(path, read_only=True, **credentials))
                self.assertEqual(reopened.get("2")["vector"], [0, 1])

    def test_encryption_raw_passphrase_and_file_reopen(self):
        for credentials in [{"key": bytes([7] * 32)}, {"passphrase": "test-only secret passphrase"}]:
            path = self.directory / ("raw.vlite" if "key" in credentials else "pass.vlite")
            db = self.keep(self.native.create(path, 2, **credentials))
            self.assertTrue(db.is_encrypted())
            self.assertIsInstance(db.encryption_key_id(), str)
            db.put("1", [1, 0], payload_base64=base64.b64encode(b"sensitive").decode())
            db.close()
            self.assertNotIn(b"sensitive", path.read_bytes())
            self.error("encryption_key_required", lambda: self.native.open(path))
            self.error("encryption_key_mismatch", lambda: self.native.open(path, key=bytes([8] * 32)))
            reopened = self.keep(self.native.open(path, **credentials))
            self.assertEqual(reopened.get("1")["vector"], [1, 0])
        db = self.keep(self.native.create_in_memory(2, key=bytes([7] * 32)))
        image = db.to_bytes()
        self.error("encryption_key_required", lambda: self.native.open_from_bytes(image))
        self.assertTrue(self.keep(self.native.open_from_bytes(image, key=bytes([7] * 32))).is_encrypted())
        self.error("invalid_input", lambda: self.native.create_in_memory(2, key=b"bad"))
        with self.assertRaises(ValueError):
            self.native.create_in_memory(2, key=bytes(32), passphrase="x")
        self.assertFalse(self.memory().is_encrypted())
        self.assertIsNone(self.memory().encryption_key_id())

    def test_restore_sequence_timestamp_and_disk_ann(self):
        path = self.directory / "log.vlite"
        db = self.keep(self.native.create(path, 2, format=5, commit_timestamps=True))
        db.put("1", [1, 0])
        first = db.revision("1")
        db.put("2", [0, 1])
        timestamp = db.changes_after()["changes"][0]["committed_at_ms"]
        sidecar = self.directory / "index.dann"
        self.assertEqual(db.build_disk_ann(sidecar)["live_records"], 2)
        self.assertEqual(db.search_disk_ann(sidecar, [1, 0], 1, ef_search=10, cache_bytes=1)[0]["id"], "1")
        db.put("3", [1, 1])
        self.error("stale_index", lambda: db.search_disk_ann(sidecar, [1, 0], 1))
        self.error("invalid_input", db.to_bytes)
        db.close()
        destination = self.directory / "restored.vlite"
        self.assertEqual(self.native.restore_to(path, destination, sequence=first)["restored_sequence"], first)
        restored = self.keep(self.native.open(destination))
        self.assertEqual(restored.info()["records"], 1)
        self.native.restore_to(path, self.directory / "time.vlite", timestamp_ms=timestamp)
        self.error("invalid_input", lambda: self.native.restore_to(path, self.directory / "bad.vlite"))
        self.error("invalid_restore_target", lambda: self.native.restore_to(path, self.directory / "beyond.vlite", sequence="1000"))
        secret = self.keep(self.native.create_in_memory(2, key=bytes(32)))
        self.error("sidecar_unavailable", lambda: secret.build_disk_ann(self.directory / "secret.dann"))

    def test_ann_snapshot_skips_rebuild_after_reopen(self):
        path = self.directory / "ann.vlite"
        sidecar = self.directory / "ann.vlite.ann"
        db = self.native.create(path, 2)
        db.batch([{"op": "put", "id": str(i), "vector": [i, 1]} for i in range(20)])
        db.build_index()
        self.assertEqual(db.save_ann_snapshot(sidecar)["indexes"], 1)
        db.close()
        db = self.keep(self.native.open(path))
        self.assertEqual(db.load_ann_snapshot(self.directory / "absent.ann"), {"status": "missing", "indexes": 0})
        self.assertEqual(db.load_ann_snapshot(sidecar), {"status": "loaded", "indexes": 1})
        self.assertEqual(db.search_approximate_ready([3, 1], 1, ef_search=10)[0]["id"], "3")
        db.put("99", [0.5, 0.5])
        self.assertNotEqual(db.load_ann_snapshot(sidecar)["status"], "loaded")
        secret = self.keep(self.native.create_in_memory(2, key=bytes(32)))
        self.error("sidecar_unavailable", lambda: secret.save_ann_snapshot(self.directory / "secret.ann"))

    def test_feature_boundaries_and_filtered_retrieval(self):
        db = self.memory()
        db.put("1", [1, 0], metadata=metadata("a"))
        db.put("2", [0, 1], metadata=metadata("b"))
        filter = [{"op": "eq", "field": "group", "value": {"type": "string", "value": "a"}}]
        self.assertEqual(db.facets("group", filter=filter)[0]["count"], 1)
        self.assertEqual(db.numeric_stats("score", filter=filter)["count"], 1)
        self.assertEqual(len(db.search_grouped([1, 0], 2, 2, "group", filter=filter)), 1)
        self.assertEqual(db.recommend(2, positive_vectors=[[1, 0]], filter=filter)[0]["id"], "1")
        self.assertEqual(len(db.search_geo([1, 0], 2, "place", 0, 0, 1000, filter=filter)), 1)
        self.error("invalid_input", lambda: db.search_grouped([1, 0], 1001, 1, "group"))
        self.error("invalid_input", lambda: db.rerank([1, 0], [{"id": "1"}], 1, method="exact", lambda_=0.5))
        self.error("invalid_input", lambda: db.rerank([1, 0], [{"id": "1"}], 1, method="mmr", metric="cosine"))
        self.error("corrupt", lambda: self.native.open_from_bytes(b"not a database"))
        self.error("not_encrypted", lambda: self.native.open_from_bytes(db.to_bytes(), key=bytes(32)))
        cursor = db.feed_cursor()
        db.create_collection("extra", 2)
        db.set_collection_settings("extra", {})
        db.drop_collection("extra")
        self.assertEqual([c["kind"] for c in db.changes_after(cursor)["changes"]],
                         ["collection_created", "settings_updated", "collection_dropped"])
        db.configure_vector("text", 2, active_version="v1")
        self.assertEqual(db.changes_after()["changes"][-1]["kind"], "vectors_updated")
        db.close()
        self.error("invalid_handle", db.feed_cursor)


if __name__ == "__main__":
    unittest.main()
