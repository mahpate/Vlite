import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import tempfile
from threading import Event, Thread
import unittest

from vlite_client import Client, ImportRejectedError, TransportError, UnsupportedError, VLiteError

TOKEN = "test_" + "a" * 32
MAX_ID = "18446744073709551615"
TAG = '"vlite-1:Zm9v_bar-:18446744073709551615:42"'
CAPS = {"version": "0.1.2", "api_version": 1, "role": "admin", "features": {"conditional_writes": True}, "routes": []}
CAP = ("GET", "/v1/capabilities", None)
RECORD = {"id": MAX_ID, "vector": [1.0], "payload_base64": "", "metadata": {}}


class Handler(BaseHTTPRequestHandler):
    calls = []
    requests = []
    mode = "ok"
    etags = [TAG]
    caps = CAPS
    timeout_release = Event()
    timeout_finished = Event()

    def log_message(self, *_):
        pass

    def do_GET(self):
        self.calls.append((self.path, self.headers.get("Authorization")))
        self.requests.append((self.command, self.path, self.headers.get_all("If-Match")))
        if self.path == "/v1/capabilities":
            status, body = {404: (404, b'{"error":{"code":"not_found","message":"No route"}}'),
                            401: (401, b'{"error":{"code":"unauthorized","message":"Bad token"}}'),
                            "bad-json": (200, b"not json")}.get(self.caps if isinstance(self.caps, (int, str)) else None,
                                                                (200, None))
            self.send_response(status)
            self.send_header("X-Request-ID", "test-caps")
            self.end_headers()
            self.wfile.write(body if body is not None else json.dumps(self.caps).encode())
        elif self.mode == "versioned":
            self.send_response(200)
            for tag in self.etags:
                self.send_header("ETag", tag)
            self.end_headers()
            self.wfile.write(json.dumps(RECORD).encode())
        elif self.mode == "precondition":
            self.send_response(412)
            self.send_header("X-Request-ID", "test-412")
            self.end_headers()
            self.wfile.write(b'{"error":{"code":"precondition_failed","message":"Record changed"}}')
        elif self.command == "DELETE":
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b'{"deleted":true}')
        elif self.mode == "redirect":
            self.send_response(302)
            self.send_header("Location", "/stolen")
            self.end_headers()
        elif self.mode == "error":
            self.send_response(409)
            self.send_header("X-Request-ID", "test-42")
            self.end_headers()
            self.wfile.write(b'{"error":{"code":"index_not_ready","message":"Build first"}}')
        elif self.mode == "bad-json":
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b'not json')
        elif self.mode == "timeout":
            try:
                self.timeout_release.wait(60)
            finally:
                self.timeout_finished.set()
        else:
            self.send_response(200)
            self.end_headers()
            self.wfile.write(json.dumps({"items": [], "next_after": None}).encode())

    def do_PUT(self):
        self.rfile.read(int(self.headers.get("Content-Length", "0")))
        self.do_GET()

    do_DELETE = do_GET

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        self.requests.append((self.command, self.path, body))
        if self.mode == "filter-rejected":
            self.send_response(422)
            self.send_header("X-Request-ID", "test-422")
            self.end_headers()
            self.wfile.write(b'{"error":{"code":"invalid_input","message":"filters require mode exact"}}')
        else:
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b'{"hits":[{"id":"7","distance":0.5}]}')


class ClientTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        cls.thread = Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        cls.base = f"http://127.0.0.1:{cls.server.server_port}"

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join()

    def setUp(self):
        Handler.calls = []
        Handler.requests = []
        Handler.mode = "ok"
        Handler.etags = [TAG]
        Handler.caps = CAPS
        self.client = Client(self.base, TOKEN)

    def test_cursor_and_auth(self):
        self.assertEqual(self.client.list_vectors(after="18446744073709551615")["next_after"], None)
        self.assertEqual(Handler.calls, [("/v1/vectors?limit=25&after=18446744073709551615", "Bearer " + TOKEN)])
        self.assertNotIn(TOKEN, repr(self.client))

    def test_errors_and_redirects_are_not_retried(self):
        Handler.mode = "error"
        with self.assertRaises(VLiteError) as result:
            self.client.index_status()
        self.assertEqual((result.exception.status, result.exception.code, result.exception.request_id),
                         (409, "index_not_ready", "test-42"))
        Handler.mode = "redirect"
        with self.assertRaises(VLiteError) as result:
            self.client.database()
        self.assertEqual(result.exception.status, 302)
        self.assertEqual(len(Handler.calls), 2)

    def test_bad_response_is_explicit(self):
        Handler.mode = "bad-json"
        with self.assertRaises(TransportError):
            self.client.database()
        self.assertEqual(len(Handler.calls), 1)

    def test_mutation_timeout_is_not_retried(self):
        Handler.mode = "timeout"
        Handler.timeout_release.clear()
        Handler.timeout_finished.clear()
        try:
            with self.assertRaises(TransportError):
                Client(self.base, TOKEN, timeout=5).put("1", {"vector": [1]})
            self.assertEqual(len(Handler.calls), 1)
        finally:
            Handler.timeout_release.set()
            self.assertTrue(Handler.timeout_finished.wait(60), "timeout handler did not finish")

    def test_filtered_search_forwards_request_unchanged_in_every_mode(self):
        flt = [{"op": "eq", "field": "tag", "value": {"type": "string", "value": "a"}},
               {"op": "range", "field": "n", "min": {"type": "integer", "value": "-9223372036854775808"}, "max": None}]
        requests = [{"vector": [1.0, 0.0], "k": 3, "mode": mode, "metric": metric, "filter": flt,
                     **({} if mode == "exact" else {"ef_search": 50})}
                    for mode in ("exact", "approximate", "approximate_ready") for metric in ("squared_l2", "cosine")]
        for request in requests:
            self.assertEqual(self.client.search(request), [{"id": "7", "distance": 0.5}])
        self.assertEqual(Handler.requests, [("POST", "/v1/search", request) for request in requests])
        Handler.requests = []
        self.client.search({"vector": [1.0, 0.0], "k": 1, "mode": "approximate"})
        self.assertEqual(Handler.requests, [("POST", "/v1/search", {"vector": [1.0, 0.0], "k": 1, "mode": "approximate"})])

    def test_old_server_filter_rejection_propagates_without_fallback(self):
        Handler.mode = "filter-rejected"
        flt = [{"op": "eq", "field": "tag", "value": {"type": "boolean", "value": True}}]
        for mode in ("approximate", "approximate_ready"):
            with self.assertRaises(VLiteError) as result:
                self.client.search({"vector": [1.0], "k": 1, "mode": mode, "filter": flt})
            self.assertEqual((result.exception.status, result.exception.code, result.exception.request_id),
                             (422, "invalid_input", "test-422"))
        # Exactly one request per call: no retry and no silent exact-mode resend.
        self.assertEqual([(r[2]["mode"], r[2]["filter"]) for r in Handler.requests],
                         [("approximate", flt), ("approximate_ready", flt)])

    def test_invalid_inputs_never_send(self):
        for value in [1, "", "01", "-1", "18446744073709551616", "1/health"]:
            with self.assertRaises(ValueError):
                self.client.get(value)
        with self.assertRaises(ValueError):
            self.client.put("1", {"vector": [float("nan")]})
        for limit in [0, 101, True, 1.5]:
            with self.assertRaises(ValueError):
                self.client.list_vectors(limit=limit)
        for base in ["file:///tmp/a", self.base + "/path", "http://user:pass@localhost"]:
            with self.assertRaises(ValueError):
                Client(base, TOKEN)
        self.assertEqual(Handler.calls, [])


    def test_get_versioned_returns_record_and_exact_opaque_tag(self):
        Handler.mode = "versioned"
        self.assertEqual(self.client.get_versioned(MAX_ID), {"record": RECORD, "etag": TAG})
        self.assertEqual(self.client.get(MAX_ID), RECORD)
        self.assertEqual(Handler.requests, [("GET", "/v1/vectors/" + MAX_ID, None)] * 2)

    def test_get_versioned_rejects_missing_or_invalid_response_tags(self):
        Handler.mode = "versioned"
        for tags in [[], ['W/"abc"'], ["*"], ['"a", "b"'], ['"a"', '"b"'], ["abc"], ['"a b"']]:
            Handler.etags = tags
            with self.assertRaises(TransportError):
                self.client.get_versioned("1")
        self.assertEqual(len(Handler.requests), 7)

    def test_conditional_put_and_delete_forward_exact_tag(self):
        self.assertIsNone(self.client.put(MAX_ID, {"vector": [1]}, if_match=TAG))
        self.assertIs(self.client.delete(MAX_ID, if_match=TAG), True)
        self.assertEqual(Handler.requests, [CAP, ("PUT", "/v1/vectors/" + MAX_ID, [TAG]),
                                            CAP, ("DELETE", "/v1/vectors/" + MAX_ID, [TAG])])

    def test_unconditional_put_and_delete_send_no_if_match(self):
        self.assertIsNone(self.client.put("1", {"vector": [1]}))
        self.assertIs(self.client.delete("1"), True)
        self.assertIsNone(self.client.put("2", {"vector": [1]}, if_match=None))
        self.assertEqual([r[:2] for r in Handler.requests], [("PUT", "/v1/vectors/1"), ("DELETE", "/v1/vectors/1"),
                                                            ("PUT", "/v1/vectors/2")])
        self.assertEqual([r[2] for r in Handler.requests], [None, None, None])

    def test_precondition_failure_propagates_without_retry(self):
        Handler.mode = "precondition"
        for call in [lambda: self.client.put("1", {"vector": [1]}, if_match=TAG),
                     lambda: self.client.delete("1", if_match=TAG)]:
            with self.assertRaises(VLiteError) as result:
                call()
            self.assertEqual((result.exception.status, result.exception.code, result.exception.request_id),
                             (412, "precondition_failed", "test-412"))
        self.assertEqual([r[0] for r in Handler.requests], ["GET", "PUT", "GET", "DELETE"])

    def test_invalid_if_match_never_sends(self):
        bad = ["", "abc", "*", 'W/"abc"', '"a", "b"', '"a","b"', '"a b"', '"a\tb"', '"a\x7fb"',
               '"a\nb"', ' "a"', '"a" ', '"\u00e9"', '"a"b"', b'"a"', 1, True]
        for value in bad:
            with self.assertRaises(ValueError, msg=repr(value)):
                self.client.put("1", {"vector": [1]}, if_match=value)
            with self.assertRaises(ValueError, msg=repr(value)):
                self.client.delete("1", if_match=value)
        with self.assertRaises(ValueError):
            self.client.get_versioned("18446744073709551616")
        with self.assertRaises(TypeError):
            self.client.put("1", {"vector": [1]}, TAG)
        self.assertEqual(Handler.requests, [])


    def test_conditional_writes_refuse_servers_without_capability(self):
        unsupported = [404, {"api_version": 1, "features": {}}, {"api_version": 1},
                       {"api_version": 1, "features": {"conditional_writes": "true"}},
                       {"api_version": 1, "features": {"conditional_writes": 1}},
                       {"api_version": 2, "features": {"conditional_writes": True}},
                       {"api_version": True, "features": {"conditional_writes": True}},
                       {"api_version": "1", "features": {"conditional_writes": True}},
                       {"api_version": 1, "features": None}, [], None]
        for caps in unsupported:
            Handler.caps = caps
            with self.assertRaises(UnsupportedError, msg=repr(caps)):
                self.client.put("1", {"vector": [1]}, if_match=TAG)
            with self.assertRaises(UnsupportedError, msg=repr(caps)):
                self.client.delete("1", if_match=TAG)
        self.assertEqual(Handler.requests, [CAP] * 2 * len(unsupported))

    def test_capability_errors_propagate_without_mutation(self):
        Handler.caps = 401
        with self.assertRaises(VLiteError) as result:
            self.client.delete("1", if_match=TAG)
        self.assertEqual((result.exception.status, result.exception.code, result.exception.request_id),
                         (401, "unauthorized", "test-caps"))
        Handler.caps = "bad-json"
        with self.assertRaises(TransportError):
            self.client.put("1", {"vector": [1]}, if_match=TAG)
        self.assertEqual(Handler.requests, [CAP, CAP])

    def test_capability_is_checked_before_every_conditional_write(self):
        self.client.put("1", {"vector": [1]}, if_match=TAG)
        Handler.caps = 404  # e.g. restarted as an older server: never reuse the earlier positive answer
        with self.assertRaises(UnsupportedError):
            self.client.put("1", {"vector": [1]}, if_match=TAG)
        self.assertEqual(Handler.requests, [CAP, ("PUT", "/v1/vectors/1", [TAG]), CAP])


class Scripted(BaseHTTPRequestHandler):
    """Records (method, path, body) and answers from ``respond``."""

    log = []
    headers_seen = []
    respond = None

    def log_message(self, *_):
        pass

    def _handle(self):
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length) if length else b""
        body = json.loads(raw) if raw else None
        self.log.append((self.command, self.path, body))
        self.headers_seen.append(dict(self.headers))
        status, value, headers = (Scripted.respond or (lambda *_: (200, {}, {})))(self.command, self.path, body)
        self.send_response(status)
        for key, header in (headers or {}).items():
            self.send_header(key, header)
        if not any(key.lower() == "content-type" for key in (headers or {})):
            self.send_header("Content-Type", "application/json")
        self.end_headers()
        if value is not None:
            self.wfile.write(value.encode() if isinstance(value, str) else json.dumps(value).encode())

    do_GET = do_PUT = do_POST = do_DELETE = _handle


def error(status, code, extra=None):
    return status, dict(extra or {}, error={"code": code, "message": "m"}), {"X-Request-ID": f"rid-{status}"}


class CapabilityTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), Scripted)
        cls.thread = Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        cls.client = Client(f"http://127.0.0.1:{cls.server.server_port}", TOKEN)

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join()

    def setUp(self):
        Scripted.log, Scripted.headers_seen, Scripted.respond = [], [], None

    def answer(self, table):
        """Route (method, path) to a fixed response; anything else is a 599."""
        Scripted.respond = lambda method, path, body: table.get((method, path), (599, None, {}))

    def test_scoped_record_routes_and_bodies(self):
        tag = '"r2-0123456789abcdef-1-7-9-' + "a" * 64 + '"'
        record = {"id": "7", "vector": [1.0], "payload_base64": "", "metadata": {}, "revision": "9"}
        base = "/v1/collections/docs"
        self.answer({("GET", "/v1/capabilities"): (200, CAPS, {}),
                     ("GET", base + "/vectors/7"): (200, record, {"ETag": tag}),
                     ("PUT", base + "/vectors/7"): (204, None, {}),
                     ("DELETE", base + "/vectors/7"): (200, {"deleted": True}, {}),
                     ("GET", base + "/vectors?limit=2&after=" + MAX_ID): (200, {"items": [], "next_after": None}, {}),
                     ("POST", base + "/batches"): (204, None, {}),
                     ("POST", base + "/search"): (200, {"hits": [{"id": "7", "distance": 0.0}],
                                                       "plan": None, "stats": {"records_scanned": 1}}, {}),
                     ("GET", base + "/index/hnsw"): (200, {"ready": False, "index": None}, {}),
                     ("POST", base + "/index/hnsw"): (204, None, {}),
                     ("GET", base + "/index/hnsw/rebuild"): (200, {"state": "idle"}, {}),
                     ("POST", base + "/index/hnsw/rebuild"): (202, {"outcome": "running"}, {}),
                     ("POST", base + "/index/hnsw/rebuild/poll"): (200, {"outcome": "running"}, {}),
                     ("POST", base + "/index/hnsw/rebuild/cancel"): (200, {"cancelled": True}, {}),
                     ("POST", base + "/index/hnsw/quality"): (200, {"recall_at_k": 1.0, "samples_used": 2,
                                                                    "distance_evaluations": 9, "skipped_reason": None}, {}),
                     ("GET", base + "/index/hnsw/maintenance"): (200, {"policy": {}, "status": {}, "identity": {}}, {}),
                     ("POST", base + "/index/hnsw/maintenance/tick"): (200, {"outcome": "idle"}, {})})
        docs = self.client.collection("docs")
        self.assertEqual(docs.name, "docs")
        self.assertEqual(docs.get("7"), record)
        self.assertEqual(docs.get_versioned("7"), {"record": record, "etag": tag})
        self.assertIsNone(docs.put("7", {"vector": [1]}, if_match=tag))
        self.assertIs(docs.delete("7", if_match=tag), True)
        self.assertIsNone(docs.put("7", {"vector": [1]}))
        docs.list_vectors(limit=2, after=MAX_ID)
        docs.batch([{"op": "delete", "id": "7"}])
        request = {"vector": [1.0], "k": 1, "mode": "auto", "explain": True}
        self.assertEqual(docs.search(request), [{"id": "7", "distance": 0.0}])
        self.assertEqual(docs.search_detailed(request)["stats"], {"records_scanned": 1})
        docs.index_status()
        docs.build_index()
        docs.build_index(metric="squared_l2", quantization="scalar_u8", rerank_multiplier=4)
        docs.rebuild_status()
        docs.start_rebuild({"quantization": "scalar_u8"})
        docs.start_rebuild()
        docs.poll_rebuild()
        docs.cancel_rebuild()
        self.assertEqual(docs.ann_quality({"samples": 2, "k": 1, "seed": "5"})["recall_at_k"], 1.0)
        docs.ann_quality()
        docs.maintenance_status()
        docs.maintenance_tick()
        self.assertEqual(Scripted.log, [
            ("GET", base + "/vectors/7", None), ("GET", base + "/vectors/7", None),
            ("GET", "/v1/capabilities", None), ("PUT", base + "/vectors/7", {"vector": [1]}),
            ("GET", "/v1/capabilities", None), ("DELETE", base + "/vectors/7", None),
            ("PUT", base + "/vectors/7", {"vector": [1]}),
            ("GET", base + "/vectors?limit=2&after=" + MAX_ID, None),
            ("POST", base + "/batches", {"operations": [{"op": "delete", "id": "7"}]}),
            ("POST", base + "/search", request), ("POST", base + "/search", request),
            ("GET", base + "/index/hnsw", None),
            # Collection routes omit unset fields so the collection's settings apply.
            ("POST", base + "/index/hnsw", {}),
            ("POST", base + "/index/hnsw", {"metric": "squared_l2", "quantization": "scalar_u8", "rerank_multiplier": 4}),
            ("GET", base + "/index/hnsw/rebuild", None),
            ("POST", base + "/index/hnsw/rebuild", {"quantization": "scalar_u8"}),
            ("POST", base + "/index/hnsw/rebuild", {}),
            ("POST", base + "/index/hnsw/rebuild/poll", None), ("POST", base + "/index/hnsw/rebuild/cancel", None),
            ("POST", base + "/index/hnsw/quality", {"samples": 2, "k": 1, "seed": "5"}),
            ("POST", base + "/index/hnsw/quality", {}),
            ("GET", base + "/index/hnsw/maintenance", None), ("POST", base + "/index/hnsw/maintenance/tick", None)])
        if_match = [h.get("If-Match") for h in Scripted.headers_seen]
        self.assertEqual(if_match[3], tag)
        self.assertEqual(if_match[5], tag)
        self.assertIsNone(if_match[6])

    def test_legacy_build_index_body_is_unchanged(self):
        self.answer({("POST", "/v1/index/hnsw"): (204, None, {})})
        self.client.build_index()
        self.client.build_index(metric="squared_l2")
        self.client.build_index(metric="cosine")
        self.client.build_index(quantization="scalar_u8", rerank_multiplier=16)
        self.assertEqual([entry[2] for entry in Scripted.log],
                         [{}, {}, {"metric": "cosine"}, {"quantization": "scalar_u8", "rerank_multiplier": 16}])

    def test_collection_management(self):
        info = {"id": 1, "name": "docs", "dimensions": 3, "live_records": 0, "settings": {}, "settings_persisted": True}
        self.answer({("GET", "/v1/collections"): (200, {"items": [info], "format_version": 4}, {}),
                     ("POST", "/v1/collections"): (201, info, {}),
                     ("GET", "/v1/collections/docs"): (200, dict(info, text_index={}), {}),
                     ("PUT", "/v1/collections/docs/settings"): (200, {"collection": info, "persisted": True}, {}),
                     ("DELETE", "/v1/collections/docs"): (200, {"deleted": True}, {})})
        settings = {"metric": "cosine", "metadata_indexes": [{"field": "tag", "kind": "eq"}], "text_fields": ["title"]}
        self.assertEqual(self.client.list_collections()["format_version"], 4)
        self.assertEqual(self.client.create_collection("docs", 3), info)
        self.client.create_collection("docs", 768, settings)
        self.assertIn("text_index", self.client.get_collection("docs"))
        self.assertIn("text_index", self.client.collection("docs").info())
        self.assertIs(self.client.set_collection_settings("docs", {"quantization": "scalar_u8"})["persisted"], True)
        self.assertIs(self.client.drop_collection("docs"), True)
        self.assertEqual(Scripted.log, [
            ("GET", "/v1/collections", None),
            ("POST", "/v1/collections", {"name": "docs", "dimensions": 3}),
            ("POST", "/v1/collections", {"name": "docs", "dimensions": 768, "settings": settings}),
            ("GET", "/v1/collections/docs", None), ("GET", "/v1/collections/docs", None),
            ("PUT", "/v1/collections/docs/settings", {"quantization": "scalar_u8"}),
            ("DELETE", "/v1/collections/docs", None)])

    def test_invalid_arguments_never_send(self):
        client = self.client
        for name in ["", "Docs", "1docs", "a/b", "..", "docs?x", "d" * 64, "_docs", None, 1]:
            with self.assertRaises(ValueError, msg=repr(name)):
                client.collection(name)
            with self.assertRaises(ValueError):
                client.get_collection(name)
            with self.assertRaises(ValueError):
                client.begin_import(name)
        for dims in [0, True, 1.5, "3", 2**32]:
            with self.assertRaises(ValueError):
                client.create_collection("docs", dims)
        bad_calls = [
            lambda: client.create_collection("docs", 3, []),
            lambda: client.set_collection_settings("docs", None),
            lambda: client.collection("docs").get("01"),
            lambda: client.collection("docs").build_index(rerank_multiplier=17),
            lambda: client.collection("docs").ann_quality([]),
            lambda: client.collection("docs").start_rebuild("x"),
            lambda: client.import_chunk("1", "-1", []),
            lambda: client.import_chunk("01", "1", []),
            lambda: client.import_chunk("1", "1", [], policy="skip"),
            lambda: client.import_status("18446744073709551616"),
            lambda: client.finish_import(1),
            lambda: client.verify_backup("../data.vlite"),
            lambda: client.drill_backup("a/b.vlite"),
            lambda: client.verify_backup(""),
            lambda: client.rotate_credential("ABCDEFGHIJKL"),
            lambda: client.rotate_credential("abcdefghijk1"),
            lambda: client.rotate_credential("abcdefghijkl", grace_seconds=86401),
            lambda: client.rotate_credential("abcdefghijkl", grace_seconds=-1),
            lambda: client.revoke_credential("abc"),
            lambda: client.persistent_audit(limit=0),
            lambda: client.persistent_audit(limit=1001),
            lambda: client.set_auto_maintenance([]),
            lambda: client.set_backup_schedule(None),
            lambda: client.create_credential("x"),
            lambda: client.search({"vector": [float("inf")], "k": 1, "mode": "auto"}),
        ]
        for call in bad_calls:
            with self.assertRaises(ValueError):
                call()
        self.assertEqual(Scripted.log, [])

    def test_search_modes_forward_unchanged_and_errors_propagate_once(self):
        hybrid = {"text": "red fox", "k": 2, "mode": "hybrid", "vector_leg": "approximate_ready",
                  "fusion": {"rrf_k": 60, "vector_weight": 1.0, "text_weight": 0.5, "candidates": 50},
                  "filter": [{"op": "eq", "field": "tag", "value": {"type": "string", "value": "a"}}]}
        hits = [{"id": "3", "score": 0.03, "distance": None, "bm25": 1.2, "vector_rank": None, "text_rank": 1}]
        plan = {"strategy": "exact_indexed", "estimated_candidates": 3, "live_records": 10,
                "index_field": "tag", "reason": "estimate within exact limit"}
        responses = iter([(200, {"hits": hits}, {}),
                          (200, {"hits": [], "plan": plan, "stats": {"index_candidates": 3}}, {}),
                          error(409, "index_not_ready"), error(422, "invalid_input"),
                          error(403, "forbidden_scope"), error(401, "credential_expired"),
                          error(404, "collection_not_found")])
        Scripted.respond = lambda *_: next(responses)
        self.assertEqual(self.client.search(hybrid), hits)
        auto = {"vector": [1.0], "k": 2, "mode": "auto", "explain": True, "quantization": "scalar_u8"}
        self.assertEqual(self.client.collection("docs").search_detailed(auto)["plan"], plan)
        for status, code in [(409, "index_not_ready"), (422, "invalid_input"), (403, "forbidden_scope"),
                             (401, "credential_expired"), (404, "collection_not_found")]:
            with self.assertRaises(VLiteError) as result:
                self.client.collection("docs").search({"vector": [1.0], "k": 1, "mode": "approximate_ready"})
            self.assertEqual((result.exception.status, result.exception.code, result.exception.request_id),
                             (status, code, f"rid-{status}"))
        # One request per call: no retry, no exact fallback, no fallback to the legacy route.
        self.assertEqual([entry[:2] for entry in Scripted.log],
                         [("POST", "/v1/search"), ("POST", "/v1/collections/docs/search")]
                         + [("POST", "/v1/collections/docs/search")] * 5)
        self.assertEqual([entry[2] for entry in Scripted.log[:2]], [hybrid, auto])

    def test_import_calls_and_explicit_outcomes(self):
        report = {"outcome": "already_committed", "job": "5", "chunk": 2, "rows_committed": 2, "rows_skipped": 0,
                  "invalid_rows": 0, "invalid": [], "revision": None}
        rejected = {"job": "5", "chunk": 3, "rows_committed": 0, "rows_skipped": 0, "invalid_rows": 1,
                    "invalid": [{"row": 0, "code": "dimension_mismatch", "message": "dimension mismatch"}],
                    "revision": None}
        status = {"job": "5", "collection": "docs", "collection_dropped": False, "state": "open",
                  "last_chunk": 2, "rows_committed": 2, "rows_skipped": 0, "durable": True}
        responses = iter([(201, dict(status, last_chunk=0, durable=False), {}), (200, report, {}),
                          error(422, "import_rows_invalid", rejected), error(409, "import_conflict"),
                          (200, status, {}), (200, dict(status, state="finished"), {}),
                          (200, dict(status, state="aborted"), {}), error(404, "import_not_found")])
        Scripted.respond = lambda *_: next(responses)
        c = self.client
        self.assertIs(c.begin_import("docs")["durable"], False)
        ops = [{"op": "put", "id": "1", "vector": [1.0]}]
        self.assertEqual(c.import_chunk("5", "2", ops)["outcome"], "already_committed")
        with self.assertRaises(ImportRejectedError) as result:
            c.import_chunk("5", "3", ops, policy="reject_chunk")
        error_ = result.exception
        self.assertIsInstance(error_, VLiteError)
        self.assertEqual((error_.status, error_.code, error_.request_id), (422, "import_rows_invalid", "rid-422"))
        self.assertEqual(error_.report, dict(rejected, outcome="rejected"))
        with self.assertRaises(VLiteError) as result:
            c.import_chunk("5", "3", ops, policy="skip_invalid")
        self.assertNotIsInstance(result.exception, ImportRejectedError)
        self.assertEqual(result.exception.code, "import_conflict")
        self.assertEqual(c.import_status("5"), status)
        self.assertEqual(c.finish_import("5")["state"], "finished")
        self.assertEqual(c.abort_import("5")["state"], "aborted")
        with self.assertRaises(VLiteError) as result:
            c.import_status(MAX_ID)
        self.assertEqual(result.exception.code, "import_not_found")
        self.assertEqual(Scripted.log, [
            ("POST", "/v1/collections/docs/imports", None),
            ("PUT", "/v1/imports/5/chunks/2", {"operations": ops}),
            ("PUT", "/v1/imports/5/chunks/3", {"operations": ops, "policy": "reject_chunk"}),
            ("PUT", "/v1/imports/5/chunks/3", {"operations": ops, "policy": "skip_invalid"}),
            ("GET", "/v1/imports/5", None), ("POST", "/v1/imports/5/finish", None),
            ("POST", "/v1/imports/5/abort", None), ("GET", "/v1/imports/" + MAX_ID, None)])

    def ndjson(self, rows, blank=False):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        path = Path(directory.name) / "rows.ndjson"
        lines = [json.dumps({"op": "put", "id": str(i), "vector": [float(i)]}) for i in range(rows)]
        if blank:
            lines.insert(1, "   ")
        path.write_text("\n".join(lines) + "\n", encoding="utf-8")
        return path

    def import_server(self, last_chunk=0, state="open", collection="docs", replay="already_committed", reject=None):
        def respond(method, path, body):
            if (method, path) == ("POST", "/v1/collections/docs/imports"):
                return 201, {"job": "9", "collection": "docs", "state": "open", "last_chunk": 0, "durable": False}, {}
            if (method, path) == ("GET", "/v1/imports/9"):
                return 200, {"job": "9", "collection": collection, "collection_dropped": False, "state": state,
                             "last_chunk": last_chunk, "durable": True}, {}
            if method == "PUT":
                chunk = int(path.rsplit("/", 1)[1])
                if chunk == reject:
                    return error(422, "import_rows_invalid", {"job": "9", "chunk": chunk, "invalid_rows": 1,
                                                              "invalid": [{"row": 0, "code": "non_finite_vector",
                                                                           "message": "m"}]})
                outcome = replay if str(chunk) == str(last_chunk) else "committed"
                return 200, {"outcome": outcome, "job": "9", "chunk": chunk, "rows_committed": len(body["operations"])}, {}
            if (method, path) == ("POST", "/v1/imports/9/finish"):
                return 200, {"job": "9", "state": "finished"}, {}
            return 599, None, {}
        Scripted.respond = respond

    def test_import_ndjson_new_job(self):
        self.import_server()
        path = self.ndjson(5, blank=True)
        self.assertEqual(self.client.import_ndjson(path, "docs", chunk_size=2, policy="skip_invalid")["state"], "finished")
        self.assertEqual([entry[:2] for entry in Scripted.log], [
            ("POST", "/v1/collections/docs/imports"), ("PUT", "/v1/imports/9/chunks/1"),
            ("PUT", "/v1/imports/9/chunks/2"), ("PUT", "/v1/imports/9/chunks/3"), ("POST", "/v1/imports/9/finish")])
        self.assertEqual([[op["id"] for op in entry[2]["operations"]] for entry in Scripted.log[1:4]],
                         [["0", "1"], ["2", "3"], ["4"]])
        self.assertEqual({entry[2]["policy"] for entry in Scripted.log[1:4]}, {"skip_invalid"})

    def test_import_ndjson_resume_replays_last_chunk_explicitly(self):
        self.import_server(last_chunk=2)
        self.client.import_ndjson(str(self.ndjson(7)), "docs", chunk_size=2, job="9")
        self.assertEqual([entry[:2] for entry in Scripted.log], [
            ("GET", "/v1/imports/9"), ("PUT", "/v1/imports/9/chunks/2"), ("PUT", "/v1/imports/9/chunks/3"),
            ("PUT", "/v1/imports/9/chunks/4"), ("POST", "/v1/imports/9/finish")])
        self.assertEqual([op["id"] for op in Scripted.log[1][2]["operations"]], ["2", "3"])

    def test_import_ndjson_stops_without_finishing(self):
        cases = [
            (dict(last_chunk=2, replay="committed"), TransportError, 2),  # replay not confirmed
            (dict(reject=2), ImportRejectedError, 3),
            (dict(last_chunk=9), ValueError, 1),  # file shorter than the committed job
        ]
        for server, exception, requests in cases:
            Scripted.log = []
            self.import_server(**server)
            with self.assertRaises(exception, msg=repr(server)) as raised:
                self.client.import_ndjson(self.ndjson(6), "docs", chunk_size=2,
                                          job="9" if "last_chunk" in server else None)
            self.assertEqual(raised.exception.import_job, "9")  # never aborted; resumable or abortable explicitly
            self.assertEqual(len(Scripted.log), requests, server)
            self.assertNotIn("/v1/imports/9/finish", [entry[1] for entry in Scripted.log])
            self.assertNotIn("/v1/imports/9/abort", [entry[1] for entry in Scripted.log])

    def test_import_ndjson_exact_u64_counters(self):
        for last_chunk in [MAX_ID, 2**64 - 1, 2**53 + 1]:
            Scripted.log = []
            self.import_server(last_chunk=last_chunk)
            with self.assertRaises(ValueError) as raised:
                self.client.import_ndjson(self.ndjson(6), "docs", chunk_size=2, job="9")
            self.assertEqual(raised.exception.import_job, "9")
            self.assertEqual([entry[:2] for entry in Scripted.log], [("GET", "/v1/imports/9")])
        self.import_server(last_chunk="2")  # a decimal string is accepted exactly
        self.client.import_ndjson(self.ndjson(6), "docs", chunk_size=2, job="9")
        for last_chunk in [1.5, True, -1, "x"]:
            self.import_server(last_chunk=last_chunk)
            with self.assertRaises(TransportError):
                self.client.import_ndjson(self.ndjson(6), "docs", chunk_size=2, job="9")

    def test_import_ndjson_existing_job_states(self):
        self.import_server(state="finished", last_chunk=3)
        self.assertEqual(self.client.import_ndjson(self.ndjson(2), "docs", job="9")["state"], "finished")
        for server in [dict(state="aborted"), dict(collection="other")]:
            self.import_server(**server)
            with self.assertRaises(ValueError):
                self.client.import_ndjson(self.ndjson(2), "docs", job="9")
        self.assertEqual([entry[:2] for entry in Scripted.log], [("GET", "/v1/imports/9")] * 3)
        Scripted.log = []
        for kwargs in [dict(chunk_size=0), dict(chunk_size=4097), dict(policy="skip"), dict(job="01")]:
            with self.assertRaises(ValueError):
                self.client.import_ndjson(self.ndjson(2), "docs", **kwargs)
        bad = self.ndjson(0)
        bad.write_text('{"op":"put","id":"1","vector":[1]}\n[1]\n', encoding="utf-8")
        self.import_server()
        with self.assertRaises(ValueError):
            self.client.import_ndjson(bad, "docs")
        with self.assertRaises(ValueError):
            self.client.import_ndjson(bad, "docs", job="9")
        bad.write_text("\n  \n", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "no operations"):
            self.client.import_ndjson(bad, "docs")
        with self.assertRaises(FileNotFoundError) as missing:
            self.client.import_ndjson(bad.with_name("missing.ndjson"), "docs")
        self.assertFalse(hasattr(missing.exception, "import_job"))
        # The file is validated first: no job is reserved or resumed for a bad file.
        self.assertEqual(Scripted.log, [])

    def test_admin_operations(self):
        Scripted.respond = lambda *_: (200, {"ok": True}, {})
        c = self.client
        auto = {"enabled": True, "interval_secs": 10, "stale_ratio": 0.2, "rebuild_on_low_recall": False, "min_recall": 0.9}
        schedule = {"enabled": True, "interval_secs": 300, "retain_last": 7, "retain_days": 0, "retention_enabled": False}
        new = {"name": "reader", "collections": ["docs"], "operations": ["read", "search"], "expires_unix_ms": 1}
        calls = [c.auto_maintenance, lambda: c.set_auto_maintenance(auto), c.tick_auto_maintenance, c.backup_schedule,
                 lambda: c.set_backup_schedule(schedule), c.run_backup_schedule, c.list_backups, c.create_backup,
                 lambda: c.verify_backup("backup-1-2-3.vlite"), lambda: c.drill_backup("backup-1-2-3.vlite"),
                 c.slow_queries, c.capabilities, c.list_credentials, lambda: c.create_credential(new),
                 lambda: c.rotate_credential("abcdefghijk2"), lambda: c.rotate_credential("abcdefghijk2", grace_seconds=0),
                 lambda: c.revoke_credential("abcdefghijk2"), c.persistent_audit, lambda: c.persistent_audit(limit=1000)]
        for call in calls:
            self.assertEqual(call(), {"ok": True})
        self.assertEqual(Scripted.log, [
            ("GET", "/v1/admin/maintenance/auto", None), ("PUT", "/v1/admin/maintenance/auto", auto),
            ("POST", "/v1/admin/maintenance/auto/tick", None),
            ("GET", "/v1/admin/backups/schedule", None), ("PUT", "/v1/admin/backups/schedule", schedule),
            ("POST", "/v1/admin/backups/schedule/run", None),
            ("GET", "/v1/admin/backups", None), ("POST", "/v1/admin/backups", None),
            ("POST", "/v1/admin/backups/backup-1-2-3.vlite/verify", None),
            ("POST", "/v1/admin/backups/backup-1-2-3.vlite/drill", None),
            ("GET", "/v1/admin/diagnostics/slow-queries", None), ("GET", "/v1/capabilities", None),
            ("GET", "/v1/admin/credentials", None), ("POST", "/v1/admin/credentials", new),
            ("POST", "/v1/admin/credentials/abcdefghijk2/rotate", {}),
            ("POST", "/v1/admin/credentials/abcdefghijk2/rotate", {"grace_seconds": 0}),
            ("POST", "/v1/admin/credentials/abcdefghijk2/revoke", None),
            ("GET", "/v1/admin/audit/persistent?limit=100", None),
            ("GET", "/v1/admin/audit/persistent?limit=1000", None)])
        for status, code in [(422, "verification_failed"), (422, "drill_failed"), (403, "forbidden_scope")]:
            Scripted.respond = lambda *_: error(status, code)
            with self.assertRaises(VLiteError) as result:
                c.drill_backup("backup-1-2-3.vlite")
            self.assertEqual((result.exception.status, result.exception.code), (status, code))

    def test_scoped_token_is_accepted_and_sent_as_bearer(self):
        scoped = "vlk_abcdefghijk2_" + "A" * 43
        Scripted.respond = lambda *_: (200, {"role": "scoped"}, {})
        Client(f"http://127.0.0.1:{self.server.server_port}", scoped).capabilities()
        self.assertEqual(Scripted.headers_seen[0]["Authorization"], "Bearer " + scoped)

    def test_prometheus_text(self):
        text = "# TYPE vlite_records gauge\nvlite_records 3\n"
        Scripted.respond = lambda *_: (200, text, {"Content-Type": "text/plain; version=0.0.4"})
        self.assertEqual(self.client.prometheus_metrics(), text)
        self.assertEqual(Scripted.log, [("GET", "/metrics?format=prometheus", None)])
        self.assertEqual(Scripted.headers_seen[0]["Accept"], "text/plain")
        Scripted.respond = lambda *_: (200, {"records": 3}, {})
        with self.assertRaises(TransportError):
            self.client.prometheus_metrics()
        Scripted.respond = lambda *_: error(403, "forbidden_scope")
        with self.assertRaises(VLiteError):
            self.client.prometheus_metrics()


if __name__ == "__main__":
    unittest.main()
