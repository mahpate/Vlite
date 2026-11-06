import copy
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import socket
import tempfile
from threading import Event, Thread
import time
from typing import get_args, get_type_hints
import unittest

import vlite_client
from vlite_client import (
    Client, EmbedResponse, Hit, HybridHit, ImportRejectedError, Ranking,
    RerankOptions, Search, SearchResponse, TransportError, UnsupportedError, VLiteError,
)

TOKEN = "test_" + "a" * 32
MAX_ID = "18446744073709551615"
TAG = '"vlite-1:Zm9v_bar-:18446744073709551615:42"'
CAPS = {"version": "0.1.2", "api_version": 1, "role": "admin", "features": {"conditional_writes": True}, "routes": []}
CAP = ("GET", "/v1/capabilities", None)
RECORD = {"id": MAX_ID, "vector": [1.0], "payload_base64": "", "metadata": {}}


class Handler(BaseHTTPRequestHandler):
    calls = []
    requests = []
    request_bodies = []
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
        length = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(length)) if length else None
        self.calls.append((self.path, self.headers.get("Authorization")))
        self.requests.append((self.command, self.path, self.headers.get_all("If-Match")))
        self.request_bodies.append(body)
        if self.mode == "timeout":
            try:
                self.timeout_release.wait(60)
            finally:
                self.timeout_finished.set()
            self.send_response(204)
            self.end_headers()
        elif self.mode == "precondition":
            self.send_response(412)
            self.send_header("X-Request-ID", "test-412")
            self.end_headers()
            self.wfile.write(b'{"error":{"code":"precondition_failed","message":"Record changed"}}')
        else:
            self.send_response(204)
            self.end_headers()

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
        Handler.request_bodies = []
        Handler.mode = "ok"
        Handler.etags = [TAG]
        Handler.caps = CAPS
        self.client = Client(self.base, TOKEN)

    def test_change_feed_request_and_expiry_write_shape(self):
        self.client.changes(cursor="vlc1.0000000000000001.0000000000000002.00000000", limit=20)
        self.assertEqual(Handler.calls[-1][0], "/v1/changes?limit=20&cursor=vlc1.0000000000000001.0000000000000002.00000000")
        self.client.collection("docs").grouped_search({"vector": [1.0], "field": "category", "groups": 2, "hits_per_group": 3})
        self.assertEqual(Handler.requests[-1][1], "/v1/collections/docs/search/grouped")
        self.client.collection("docs").embed_text("query text")
        self.assertEqual(Handler.requests[-1][1], "/v1/collections/docs/embed")
        self.client.put("7", {"vector": [1.0], "ttl_ms": 30000})
        self.assertEqual(Handler.requests[-1][0], "PUT")
        self.assertEqual(Handler.requests[-1][1], "/v1/vectors/7")
        self.assertEqual(Handler.request_bodies[-1]["ttl_ms"], 30000)

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

    def test_provider_typing_preserves_required_and_optional_fields(self):
        self.assertEqual(RerankOptions.__required_keys__, {"mode"})
        self.assertEqual(RerankOptions.__optional_keys__, {"candidates", "lambda", "provider"})
        hints = get_type_hints(RerankOptions)
        self.assertEqual(set(get_args(hints["mode"])), {"exact", "mmr", "custom"})
        self.assertIs(hints["lambda"], float)
        self.assertIs(hints["provider"], str)
        self.assertIn("rerank", Search.__optional_keys__)
        self.assertIs(get_type_hints(Search)["rerank"], RerankOptions)
        self.assertEqual(EmbedResponse.__required_keys__, {"vector", "dimensions", "provider"})
        self.assertEqual(get_type_hints(EmbedResponse),
                         {"vector": list[float], "dimensions": int, "provider": str})
        self.assertIs(get_type_hints(Client.embed_text)["return"], EmbedResponse)
        self.assertEqual(Ranking.__required_keys__, {"provider", "order"})
        self.assertEqual(get_args(get_type_hints(Ranking)["order"]), ("score_desc_id_asc",))
        self.assertIn("ranking", SearchResponse.__optional_keys__)
        self.assertIs(get_type_hints(SearchResponse)["ranking"], Ranking)
        for hit_type in (Hit, HybridHit):
            self.assertIn("rerank_score", hit_type.__optional_keys__)
            self.assertIs(get_type_hints(hit_type)["rerank_score"], float)

    def test_embed_response_and_provider_aliases_on_existing_routes(self):
        response: EmbedResponse = {
            "vector": [0.1, 0.2, 0.3], "dimensions": 3, "provider": "local-semantic",
        }
        paths = ("/v1/collections/default/embed", "/v1/collections/docs/embed")
        self.answer({("POST", path): (200, response, {}) for path in paths})
        for records, path in zip((self.client, self.client.collection("docs")), paths):
            self.assertEqual(records.embed_text("hello"), response)
            for provider in ("hashing", "http", "local-semantic", "Rank_1.v2", "a" * 256):
                with self.subTest(path=path, provider=provider):
                    self.assertEqual(records.embed_text("hello", provider=provider), response)
                    self.assertEqual(Scripted.log[-1],
                                     ("POST", path, {"text": "hello", "provider": provider}))
        self.assertEqual(len(Scripted.log), 12)
        self.assertEqual(Scripted.log[0][2], {"text": "hello", "provider": "hashing"})

    def test_embed_invalid_aliases_and_text_never_send(self):
        for provider in ("", "a" * 257, "https://provider", "has space", "é", "a\n", None, 1, []):
            with self.subTest(provider=provider), self.assertRaises(ValueError):
                self.client.embed_text("hello", provider=provider)
        for text in (None, 1, "é" * 8193):
            with self.subTest(text_type=type(text)), self.assertRaises(ValueError):
                self.client.embed_text(text)
        self.assertEqual(Scripted.log, [])

    def test_rerank_options_forward_unchanged_on_both_search_routes(self):
        response = {
            "hits": [{"id": "7", "distance": 0.4, "rerank_score": 0.8}],
            "ranking": {"provider": "rank-a", "order": "score_desc_id_asc"},
        }
        paths = ("/v1/search", "/v1/collections/docs/search")
        self.answer({("POST", path): (200, response, {}) for path in paths})
        options: list[RerankOptions] = [
            {"mode": "exact"},
            {"mode": "mmr", "candidates": 50, "lambda": 0.25},
            {"mode": "custom", "provider": "rank-a"},
            {"mode": "custom", "provider": "rank-a", "candidates": 1000},
        ]
        expected = []
        for records, path in zip((self.client, self.client.collection("docs")), paths):
            for rerank in options:
                request: Search = {"vector": [0.1, 0.2, 0.3], "k": 1, "rerank": rerank}
                original = json.loads(json.dumps(request))
                self.assertEqual(records.search(request), response["hits"])
                self.assertEqual(records.search_detailed(request), response)
                self.assertEqual(request, original)
                expected.extend([("POST", path, original)] * 2)
        self.assertEqual(Scripted.log, expected)

    def test_optional_ranking_and_scores_are_not_synthesized(self):
        distance_hits = [{"id": "7", "distance": 0.4}]
        hybrid_hits = [{"id": "7", "score": 0.5, "distance": None,
                        "bm25": 1.0, "vector_rank": None, "text_rank": 1}]
        for records in (self.client, self.client.collection("docs")):
            for hits in (distance_hits, hybrid_hits):
                for custom in (False, True):
                    response = {"hits": hits}
                    if custom:
                        response = {
                            "hits": [dict(hits[0], rerank_score=-0.2)],
                            "ranking": {"provider": "rank-a", "order": "score_desc_id_asc"},
                        }
                    Scripted.respond = lambda *_, response=response: (200, response, {})
                    request: Search = {"k": 1, "vector": [1.0]}
                    self.assertEqual(records.search_detailed(request), response)
                    self.assertEqual(records.search(request), response["hits"])
                    self.assertEqual(Scripted.log[-1][2], request)
        self.assertEqual(len(Scripted.log), 16)

    def test_provider_failures_propagate_once_without_fallback(self):
        request: Search = {
            "vector": [1.0], "k": 1, "rerank": {"mode": "custom", "provider": "rank-a"},
        }
        failures = [
            (409, "provider_not_configured"), (422, "invalid_provider_input"),
            (502, "provider_invalid_response"), (503, "provider_unavailable"),
            (504, "provider_timeout"), (429, "provider_busy"),
        ]
        for records in (self.client, self.client.collection("docs")):
            for status, code in failures:
                Scripted.respond = lambda *_, status=status, code=code: error(status, code)
                calls = (lambda: records.embed_text("hello", provider="local-semantic"),
                         lambda: records.search(request), lambda: records.search_detailed(request))
                for call in calls:
                    before = len(Scripted.log)
                    with self.assertRaises(VLiteError) as result:
                        call()
                    self.assertEqual((result.exception.status, result.exception.code,
                                      result.exception.request_id), (status, code, f"rid-{status}"))
                    self.assertEqual(len(Scripted.log), before + 1)

    def test_provider_snapshot_conflict_propagates_once_without_fallback(self):
        request: Search = {
            "vector": [1.0], "k": 1, "rerank": {"mode": "custom", "provider": "rank-a"},
        }
        message = "provider snapshot conflict"
        response = (409, {"error": {"code": "provider_snapshot_conflict", "message": message}},
                    {"X-Request-ID": "rid-snapshot"})
        self.assertNotIn("Retry-After", response[2])
        Scripted.respond = lambda *_: response
        for records, prefix in ((self.client, "/v1"),
                                (self.client.collection("docs"), "/v1/collections/docs")):
            embed_path = "/v1/collections/default/embed" if prefix == "/v1" else prefix + "/embed"
            calls = (
                (lambda: records.embed_text("hello", provider="local-semantic"),
                 embed_path, {"text": "hello", "provider": "local-semantic"}),
                (lambda: records.search(request), prefix + "/search", request),
                (lambda: records.search_detailed(request), prefix + "/search", request),
            )
            for call, path, body in calls:
                with self.subTest(path=path):
                    before = len(Scripted.log)
                    with self.assertRaises(VLiteError) as result:
                        call()
                    self.assertEqual((result.exception.status, result.exception.code,
                                      result.exception.request_id),
                                     (409, "provider_snapshot_conflict", "rid-snapshot"))
                    self.assertEqual(str(result.exception),
                                     f"HTTP 409: provider_snapshot_conflict: {message}")
                    self.assertEqual(Scripted.log[before:], [("POST", path, body)])

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


class KvTests(unittest.TestCase):
    setUpClass = classmethod(CapabilityTests.setUpClass.__func__)
    tearDownClass = classmethod(CapabilityTests.tearDownClass.__func__)
    setUp = CapabilityTests.setUp
    def test_kv_wire_and_results(self):
        entry = {"key": "雪 /+?%", "value_base64": "AP8=", "version": MAX_ID,
                 "expires_at_ms": "0", "future": True}
        Scripted.respond = lambda *_: (200, entry, {})
        self.assertEqual(self.client.kv_get(collection="cache", namespace="名 +", key="雪 /+?%"),
                         {"key": "雪 /+?%", "value": b"\x00\xff", "version": 18446744073709551615,
                          "expires_at_ms": 0})
        self.assertEqual(Scripted.log[-1], ("GET",
            "/v1/collections/cache/kv/entry?namespace=%E5%90%8D%20%2B&key=%E9%9B%AA%20%2F%2B%3F%25", None))
        for expected, wire in [("any", "any"), ("absent", "absent"),
                               ({"revision": 0}, {"revision": "0"}),
                               ({"revision": 18446744073709551615}, {"revision": MAX_ID})]:
            Scripted.respond = lambda *_: (200, {"version": MAX_ID, "expires_at_ms": None}, {})
            self.assertEqual(self.client.kv_put(collection="cache", namespace="n", key="k",
                value=b"\xff", expected=expected), {"version": 18446744073709551615, "expires_at_ms": None})
            self.assertEqual(Scripted.log[-1], ("PUT", "/v1/collections/cache/kv/entry",
                {"namespace": "n", "key": "k", "value_base64": "/w==", "expected": wire, "expires_at_ms": None}))
        for expected, wire in [("any", "any"), ("absent", "absent"), ({"revision": 0}, "0"),
                               ({"revision": 18446744073709551615}, MAX_ID)]:
            for deleted in (True, False):
                Scripted.respond = lambda *_, deleted=deleted: (200, {"deleted": deleted}, {})
                self.assertIs(self.client.kv_delete(collection="cache", namespace="名 +", key="雪 /+?%",
                    expected=expected), deleted)
                self.assertEqual(Scripted.log[-1], ("DELETE",
                    "/v1/collections/cache/kv/entry?namespace=%E5%90%8D%20%2B&key=%E9%9B%AA%20%2F%2B%3F%25&expected=" + wire, None))
        Scripted.respond = lambda *_: (200, {"items": [entry], "next_after": "雪 /+?%"}, {})
        page = self.client.kv_list(collection="cache", namespace="名 +", prefix="雪 ", after="a/+", limit=10000)
        self.assertEqual(page, {"items": [{"key": "雪 /+?%", "value": b"\x00\xff",
            "version": 18446744073709551615, "expires_at_ms": 0}], "next_after": "雪 /+?%"})
        self.assertEqual(Scripted.log[-1], ("GET",
            "/v1/collections/cache/kv/entries?namespace=%E5%90%8D%20%2B&prefix=%E9%9B%AA%20&after=a%2F%2B&limit=10000", None))
        Scripted.respond = lambda *_: (200, {"items": [], "next_after": None}, {})
        self.assertEqual(self.client.kv_list(collection="cache", namespace="n"), {"items": [], "next_after": None})
        self.assertEqual(Scripted.log[-1][1], "/v1/collections/cache/kv/entries?namespace=n&prefix=&limit=100")
        for (_, _, body), headers in zip(Scripted.log, Scripted.headers_seen):
            self.assertEqual(headers["Authorization"], "Bearer " + TOKEN)
            self.assertEqual(headers["Accept"], "application/json")
            self.assertEqual(headers.get("Content-Type"), "application/json" if body is not None else None)

    def test_kv_value_bounds_and_deadlines(self):
        for value, encoded in [(b"", ""), (b"\x00" * 1047290, "A" * 1396386 + "A=")]:
            Scripted.respond = lambda *_: (200, {"version": "1", "expires_at_ms": MAX_ID}, {})
            self.assertEqual(self.client.kv_put(collection="cache", namespace="n", key="k", value=value,
                expires_at_ms=18446744073709551615), {"version": 1, "expires_at_ms": 18446744073709551615})
            self.assertEqual(Scripted.log[-1][2]["value_base64"], encoded)
            self.assertEqual(Scripted.log[-1][2]["expires_at_ms"], MAX_ID)
        before = len(Scripted.log)
        with self.assertRaises(ValueError):
            self.client.kv_put(collection="cache", namespace="n", key="k", value=b"\0" * 1047291)
        self.assertEqual(len(Scripted.log), before)
        self.client.kv_put(collection="cache", namespace="n", key="k", value=b"", expires_at_ms=0)
        self.assertEqual(Scripted.log[-1][2]["expires_at_ms"], "0")

    def test_kv_text_boundaries_and_empty_entry(self):
        Scripted.respond = lambda *_: (200, {"key": "k", "value_base64": "", "version": "1", "expires_at_ms": None}, {})
        self.assertEqual(self.client.kv_get(collection="cache", namespace="n" * 255, key="é" * 512)["value"], b"")
        Scripted.respond = lambda *_: (200, {"items": [], "next_after": None}, {})
        self.client.kv_list(collection="cache", namespace="n", prefix="é" * 512, after="é" * 512, limit=1)
        self.assertEqual(len(Scripted.log), 2)

    def test_kv_maximum_entry_and_required_response_shapes(self):
        Scripted.respond = lambda *_: (200, {"key": "k", "value_base64": "A" * 1396387 + "=",
            "version": "1", "expires_at_ms": None}, {})
        self.assertEqual(self.client.kv_get(collection="cache", namespace="n", key="k")["value"], b"\0" * 1047290)
        for response, call in [
            ({"version": "1"}, lambda: self.client.kv_put(collection="cache", namespace="n", key="k", value=b"")),
            ({"deleted": 1}, lambda: self.client.kv_delete(collection="cache", namespace="n", key="k")),
            ({"items": [], "next_after": 0}, lambda: self.client.kv_list(collection="cache", namespace="n")),
            ({"items": {}}, lambda: self.client.kv_list(collection="cache", namespace="n")),
            (None, lambda: self.client.kv_get(collection="cache", namespace="n", key="k"))]:
            Scripted.respond = lambda *_, response=response: (200, response, {})
            with self.assertRaises(TransportError):
                call()

    def test_kv_errors_and_unsupported_are_not_retried(self):
        for status, code in [(404, "kv_not_found"), (412, "precondition_failed"),
            (409, "not_kv_collection"), (409, "kv_slots_exhausted"), (422, "invalid_input"),
            (404, "collection_not_found"), (503, "commit_indeterminate"), (404, "not_found"), (409, "not_found")]:
            Scripted.respond = lambda *_, status=status, code=code: error(status, code)
            for call in [lambda: self.client.kv_get(collection="cache", namespace="n", key="k"),
                         lambda: self.client.kv_put(collection="cache", namespace="n", key="k", value=b""),
                         lambda: self.client.kv_delete(collection="cache", namespace="n", key="k"),
                         lambda: self.client.kv_list(collection="cache", namespace="n")]:
                before = len(Scripted.log)
                unsupported = status == 404 and code == "not_found"
                with self.assertRaises(UnsupportedError if unsupported else VLiteError) as raised:
                    call()
                if not unsupported:
                    self.assertEqual((raised.exception.status, raised.exception.code, raised.exception.request_id),
                                     (status, code, f"rid-{status}"))
                self.assertEqual(len(Scripted.log), before + 1)

    def test_kv_invalid_inputs_and_responses(self):
        for kwargs in [{"namespace": ""}, {"namespace": "é" * 128}, {"key": ""},
                       {"key": "é" * 513}, {"key": "\ud800"}, {"collection": ""}]:
            with self.assertRaises(ValueError):
                self.client.kv_get(**({"collection": "cache", "namespace": "n", "key": "k"} | kwargs))
        for expected in [None, {"revision": True}, {"revision": -1}, {"revision": 2**64},
                         {"revision": "1"}, {"revision": 1, "extra": 0}, "wrong"]:
            with self.assertRaises(ValueError):
                self.client.kv_delete(collection="cache", namespace="n", key="k", expected=expected)
        for kwargs in [{"limit": 0}, {"limit": 10001}, {"limit": True}, {"prefix": "é" * 513}, {"after": ""}]:
            with self.assertRaises(ValueError):
                self.client.kv_list(collection="cache", namespace="n", **kwargs)
        for expiry in [True, -1, 2**64, "1"]:
            with self.assertRaises(ValueError):
                self.client.kv_put(collection="cache", namespace="n", key="k", value=b"", expires_at_ms=expiry)
        with self.assertRaises(ValueError):
            self.client.kv_put(collection="cache", namespace="n", key="k", value="not bytes")
        self.assertEqual(Scripted.log, [])
        valid = {"key": "k", "value_base64": "", "version": "1", "expires_at_ms": None}
        for patch in [{"version": "0"}, {"version": 1}, {"version": "01"}, {"version": str(2**64)},
                      {"expires_at_ms": 0}, {"expires_at_ms": "01"}, {"value_base64": "/x=="},
                      {"value_base64": "/w"}, {"value_base64": "_w=="}, {"value_base64": " /w=="}]:
            Scripted.respond = lambda *_, patch=patch: (200, valid | patch, {})
            with self.assertRaises(TransportError):
                self.client.kv_get(collection="cache", namespace="n", key="k")

    def test_kv_timeout_does_not_retry(self):
        admitted, release = Event(), Event()
        def respond(*_):
            admitted.set()
            release.wait(5)
            return 200, None, {}
        Scripted.respond = respond
        try:
            with self.assertRaises(TransportError):
                Client(f"http://127.0.0.1:{self.server.server_port}", TOKEN, timeout=0.1).kv_put(
                    collection="cache", namespace="n", key="k", value=b"")
            self.assertTrue(admitted.is_set())
            self.assertEqual(len(Scripted.log), 1)
        finally:
            release.set()

    def test_kv_delete_and_reads_do_not_retry_after_timeout(self):
        admitted, release = Event(), Event()
        def respond(*_):
            admitted.set()
            release.wait(5)
            return 200, None, {}
        Scripted.respond = respond
        slow = Client(f"http://127.0.0.1:{self.server.server_port}", TOKEN, timeout=0.1)
        try:
            for call in [lambda: slow.kv_delete(collection="cache", namespace="n", key="k"),
                         lambda: slow.kv_get(collection="cache", namespace="n", key="k"),
                         lambda: slow.kv_list(collection="cache", namespace="n")]:
                admitted.clear()
                before = len(Scripted.log)
                with self.assertRaises(TransportError):
                    call()
                self.assertTrue(admitted.is_set())
                self.assertEqual(len(Scripted.log), before + 1)
        finally:
            release.set()

    def test_kv_caller_interruption_does_not_retry(self):
        from unittest.mock import patch
        Scripted.respond = lambda *_: (200, {"version": "1", "expires_at_ms": None}, {})
        original_open = self.client._opener.open
        def interrupted(*args, **kwargs):
            with original_open(*args, **kwargs):
                raise KeyboardInterrupt("caller cancelled")
        with patch.object(self.client._opener, "open", side_effect=interrupted):
            with self.assertRaises(KeyboardInterrupt):
                self.client.kv_put(collection="cache", namespace="n", key="k", value=b"")
        self.assertEqual(len(Scripted.log), 1)


# --- Agent memory (Gap 1) -------------------------------------------------------
# Expected values come from the serving memory contract (sections 3, 4 and 8) and
# the independent wire fixtures; nothing here is derived from the client under test.
REPO_ROOT = Path(__file__).resolve().parents[2]
WIRE = REPO_ROOT / "tests" / "fixtures" / "agent_memory" / "wire"
MIN_I64, MAX_I64 = "-9223372036854775808", "9223372036854775807"

# (fixture operation, facade method, route suffix): the section 4 table.
MEMORY_OPERATIONS = (
    ("remember", "remember", "/remember"),
    ("recall", "recall", "/recall"),
    ("facts", "find_facts", "/facts/query"),
    ("correct_facts", "correct_facts", "/facts/correct"),
    ("consolidate", "consolidate", "/consolidate"),
    ("forget", "forget", "/forget"),
    ("receipt", "get_receipt", "/receipts/lookup"),
    ("erasure", "get_erasure", "/erasures/get"),
    ("resume_erasure", "resume_erasure", "/erasures/resume"),
)
METHODS = tuple(method for _, method, _ in MEMORY_OPERATIONS)
_FIXTURES = {}


def memory_fixture(operation):
    if operation not in _FIXTURES:
        _FIXTURES[operation] = json.loads((WIRE / f"{operation}.json").read_text(encoding="utf-8"))
    return copy.deepcopy(_FIXTURES[operation])


def has_directive(value):
    """Fixture input compression ($text, $repeat, ...) is notation, not a wire shape."""
    if isinstance(value, dict):
        return any(key.startswith("$") or has_directive(item) for key, item in value.items())
    return isinstance(value, list) and any(has_directive(item) for item in value)


def inventory_body(operation):
    structural = memory_fixture(operation)["structural"]
    return next(c for c in structural if c["name"] == f"http-inventory-{operation}")["value"]


def exchange_responses(operation):
    return [(c["name"], c["status"], c["value"]) for c in memory_fixture(operation)["structural"]
            if c["schema"] == "exchange" and c["valid"] and not c["request"] and c["operation"] == operation]


def wire(value):
    """Type-exact JSON text: True is not 1, "1" is not 1 and 1 is not 1.0."""
    return json.dumps(value, sort_keys=True, allow_nan=False)


def memory_error(status, code, details=None, headers=None):
    body = {"error": {"code": code, "message": "m"}}
    if details is not None:
        body["error"]["details"] = details
    return status, body, {"X-Request-ID": f"rid-{status}", **(headers or {})}


def with_unknown_fields(value):
    if isinstance(value, dict):
        return {**{key: with_unknown_fields(item) for key, item in value.items()},
                "zz_future": {"nested": [1, None, "x"]}}
    if isinstance(value, list):
        return [with_unknown_fields(item) for item in value]
    return value


SCOPE = {"agent": "a", "user": "u", "session": "s"}
SLOT = {"subject_id": "i0", "predicate": "role", "slot_key": "primary"}
REF = {"id": "r1", "revision": MAX_ID}
INTERVAL = {"valid_from_ms": MIN_I64, "valid_to_ms": "-1"}
SOURCE = {"source_id": "src", "source_kind": "caller", "uri": "https://example.test/doc", "transformation_id": "xf"}
EVIDENCE_REF = {"collection": "docs", "record": MAX_ID}
EVIDENCE = {"collection": "docs", "record": "0", "vector": [0.5, 0.25], "text": "t", "payload_base64": "AP8=",
            "metadata": {"k": {"type": "string", "value": "v"}, "n": {"type": "integer", "value": "-1"}}}
ASSERTION = {"subject_id": "i0", "predicate": "role", "slot_key": "primary", "destination_id": "i2",
             "interval": INTERVAL, "slot_expected": {"revision": MAX_ID}, "disagreement": "disputed"}
ITEM = {"id": "i1", "kind": "fact", "content": "雪 😀", "observed_at_ms": MIN_I64, "importance": 0.5, "confidence": 1,
        "expires_at_ms": MAX_ID, "source": SOURCE,
        "provenance": [{"source_id": "p", "source_kind": "caller", "uri": "u", "transformation_id": "t"}],
        "references": [REF, {"id": "r2", "revision": "0"}], "evidence": [EVIDENCE_REF],
        "metadata": {"s": {"type": "string", "value": "v"}, "i": {"type": "integer", "value": MIN_I64},
                     "f": {"type": "float", "value": 1.5}, "b": {"type": "boolean", "value": False}},
        "assertion": ASSERTION, "expected": {"absent": True}, "merge": "merge_provenance"}
MUTATION = {"scope": SCOPE, "idempotency_key": "k1", "max_references": 256, "deadline_ms": 5000,
            "require_complete_feed": True}
LIMITS = {"candidates": 1000, "vector_components": 4000000, "evidence_bytes": 4194304, "mmr_components": 100000000,
          "deadline_ms": 5000, "max_hops": 4, "max_visited_nodes": 10000, "max_edges_examined": 100000,
          "max_edges_per_node": 10000, "max_output_nodes": 1000, "max_output_edges": 2000, "max_output_paths": 100}
PROJECTION = {"snippets": False, "provenance": False, "paths": True, "metadata": ["team"]}
RANKING = {"selection": "mmr", "weights": [0.5, 0.25, 0.25], "half_life_ms": 10, "future_observations": "clamp",
           "lambda": 0.25}
FILTER = {"all": [{"field": "team", "op": "eq", "value": {"type": "string", "value": "x"}},
                  {"field": "n", "op": "gte", "value": {"type": "integer", "value": MIN_I64}},
                  {"field": "f", "op": "lt", "value": {"type": "float", "value": 0.5}},
                  {"field": "b", "op": "eq", "value": {"type": "boolean", "value": True}}],
          "any": [{"field": "x", "op": "exists"},
                  {"field": "m", "op": "lte", "value": {"type": "integer", "value": MAX_I64}}]}
INPUT = {"id": "m1", "revision": MAX_ID, "disposition": "logical_delete"}
FORGET_TARGETS = ({"ids": [REF, {"id": "r3", "revision": "0"}]}, {"series": REF}, {"entity": REF},
                  {"session": True}, {"scope": True})

# Every field of every request once, in canonical string form. These are form-complete,
# not semantically valid: the server owns the conditional rules.
FULL = {
    "remember": {**MUTATION, "items": [ITEM], "evidence": [EVIDENCE]},
    "recall": {"scope": SCOPE, "scopes": [{"agent": "a"}, SCOPE], "vector": [1, 0.5], "text": "Ada",
               "embedding_provider": "p", "entity_seeds": ["e1", "e2"],
               "kinds": ["entity", "episodic", "fact", "relation"], "filter": FILTER, "as_of_ms": MIN_I64,
               "ranking": RANKING, "limits": LIMITS, "projection": PROJECTION, "mode": "approximate",
               "limit": 100, "allow_partial": True},
    "find_facts": {"scope": SCOPE, "slot": SLOT, "as_of_ms": MAX_I64, "history": True, "limit": 100,
                   "limits": LIMITS, "projection": PROJECTION, "allow_partial": True},
    "correct_facts": {**MUTATION, "slot": SLOT, "series": REF, "slot_revision": MAX_ID,
                      "interval": {"valid_from_ms": None, "valid_to_ms": "-1"}, "action": "replace",
                      "at_ms": MIN_I64, "correction_interval": {"valid_from_ms": "-1", "valid_to_ms": None},
                      "replacement": ITEM, "evidence": [EVIDENCE]},
    "consolidate": {**MUTATION, "inputs": [INPUT, {"id": "m2", "revision": "1"}],
                    "output": {**ITEM, "kind": "episodic"}, "evidence": [EVIDENCE], "dry_run": True},
    "forget": {**MUTATION, "target": FORGET_TARGETS[0], "mode": "expire", "expires_at_ms": MAX_ID,
               "allow_job": True},
    "get_receipt": {"scope": SCOPE, "operation": "resume_erasure", "idempotency_key": "k1"},
    "get_erasure": {"scope": SCOPE, "job_id": "j1"},
    "resume_erasure": {**MUTATION, "job_id": "j1", "revision": MAX_ID, "generation": MAX_ID,
                       "resume_token": "opaque"},
}
MIN_ITEM = {"id": "e1", "kind": "entity", "content": "Ada", "observed_at_ms": "90",
            "source": {"source_id": "s1", "source_kind": "caller"}}
MIN = {
    "remember": {"scope": {"agent": "a"}, "idempotency_key": "k1", "items": [MIN_ITEM]},
    "recall": {"scope": {"agent": "a"}, "text": "Ada"},
    "find_facts": {"scope": {"agent": "a"}, "slot": SLOT},
    "correct_facts": {"scope": {"agent": "a"}, "idempotency_key": "k1", "slot": SLOT, "series": REF,
                      "slot_revision": "7", "interval": {"valid_from_ms": "0", "valid_to_ms": None},
                      "action": "remove"},
    "consolidate": {"scope": {"agent": "a"}, "idempotency_key": "k1", "inputs": [{"id": "m1", "revision": "1"}],
                    "output": {**MIN_ITEM, "kind": "episodic"}},
    "forget": {"scope": {"agent": "a"}, "idempotency_key": "k1", "target": {"scope": True},
               "mode": "logical_delete"},
    "get_receipt": {"scope": {"agent": "a"}, "operation": "remember", "idempotency_key": "k1"},
    "get_erasure": {"scope": {"agent": "a"}, "job_id": "j1"},
    "resume_erasure": {"scope": {"agent": "a"}, "idempotency_key": "k1", "job_id": "j1", "revision": "8",
                       "generation": "1", "resume_token": "opaque1"},
}
ITEM_INTEGERS = (("observed_at_ms",), ("expires_at_ms",), ("references", 0, "revision"),
                 ("references", 1, "revision"), ("evidence", 0, "record"), ("metadata", "i", "value"),
                 ("assertion", "interval", "valid_from_ms"), ("assertion", "interval", "valid_to_ms"),
                 ("assertion", "slot_expected", "revision"))
EVIDENCE_INTEGERS = (("record",), ("metadata", "n", "value"))


def under(prefix, paths):
    return tuple(prefix + path for path in paths)


# Where FULL holds a U64 or I64 as a canonical string (and is not null).
INTEGER_POSITIONS = {
    "remember": under(("items", 0), ITEM_INTEGERS) + under(("evidence", 0), EVIDENCE_INTEGERS),
    "recall": (("as_of_ms",), ("filter", "all", 1, "value", "value"), ("filter", "any", 1, "value", "value")),
    "find_facts": (("as_of_ms",),),
    "correct_facts": ((("series", "revision"), ("slot_revision",), ("interval", "valid_to_ms"), ("at_ms",),
                       ("correction_interval", "valid_from_ms")) + under(("replacement",), ITEM_INTEGERS)
                      + under(("evidence", 0), EVIDENCE_INTEGERS)),
    "consolidate": ((("inputs", 0, "revision"), ("inputs", 1, "revision")) + under(("output",), ITEM_INTEGERS)
                    + under(("evidence", 0), EVIDENCE_INTEGERS)),
    "forget": (("target", "ids", 0, "revision"), ("target", "ids", 1, "revision"), ("expires_at_ms",)),
    "get_receipt": (),
    "get_erasure": (),
    "resume_erasure": (("revision",), ("generation",)),
}


def integer_twin(method):
    request = copy.deepcopy(FULL[method])
    for path in INTEGER_POSITIONS[method]:
        parent = request
        for step in path[:-1]:
            parent = parent[step]
        parent[path[-1]] = int(parent[path[-1]])
    return request


def hosted(method, base, path):
    """Build ``method`` requests that carry a piece at ``path`` of a minimal request."""
    def build(piece):
        request = copy.deepcopy(base)
        parent = request
        for step in path[:-1]:
            parent = parent[step]
        parent[path[-1]] = piece
        return method, request
    return build


def remember_with(**item):
    return {**MIN["remember"], "items": [{**MIN_ITEM, **item}]}


FILTER_HOST = {**MIN["recall"], "filter": {"all": [{"field": "x", "op": "exists"}]}}
SERIES_HOST = {**MIN["forget"], "target": {"series": REF}}
METADATA_HOST = remember_with(metadata={"m": {"type": "string", "value": "v"}})
# name: (piece, required keys, nullable keys, builder). Every key of every object is
# probed for removal, null, a foreign type and an unknown sibling.
PIECES = {
    "scope": (SCOPE, {"agent"}, set(), hosted("recall", MIN["recall"], ("scope",))),
    "source": (SOURCE, {"source_id", "source_kind"}, set(), hosted("remember", MIN["remember"], ("items", 0, "source"))),
    "ref": (REF, {"id", "revision"}, set(), hosted("forget", SERIES_HOST, ("target", "series"))),
    "interval": (INTERVAL, {"valid_from_ms", "valid_to_ms"}, {"valid_from_ms", "valid_to_ms"},
                 hosted("correct_facts", MIN["correct_facts"], ("interval",))),
    "slot": (SLOT, set(SLOT), set(), hosted("find_facts", MIN["find_facts"], ("slot",))),
    "evidence_ref": (EVIDENCE_REF, {"collection", "record"}, set(),
                     hosted("remember", remember_with(evidence=[EVIDENCE_REF]), ("items", 0, "evidence", 0))),
    "evidence": (EVIDENCE, {"collection", "record"}, set(),
                 hosted("remember", {**MIN["remember"], "evidence": [EVIDENCE]}, ("evidence", 0))),
    "assertion": (ASSERTION, {"subject_id", "predicate", "slot_key", "destination_id", "interval", "slot_expected"},
                  set(), hosted("remember", MIN["remember"], ("items", 0, "assertion"))),
    "item": (ITEM, {"id", "kind", "content", "observed_at_ms", "source"}, set(),
             hosted("remember", MIN["remember"], ("items", 0))),
    "condition": ({"field": "n", "op": "gte", "value": {"type": "integer", "value": MIN_I64}}, {"field", "op"},
                  set(), hosted("recall", FILTER_HOST, ("filter", "all", 0))),
    "filter": (FILTER, set(), set(), hosted("recall", MIN["recall"], ("filter",))),
    "ranking": (RANKING, set(), set(), hosted("recall", MIN["recall"], ("ranking",))),
    "limits": (LIMITS, set(), set(), hosted("recall", MIN["recall"], ("limits",))),
    "projection": (PROJECTION, set(), set(), hosted("recall", MIN["recall"], ("projection",))),
    "input": (INPUT, {"id", "revision"}, set(), hosted("consolidate", MIN["consolidate"], ("inputs", 0))),
    "expected-absent": ({"absent": True}, {"absent"}, set(), hosted("remember", MIN["remember"], ("items", 0, "expected"))),
    "expected-revision": ({"revision": MAX_ID}, {"revision"}, set(),
                          hosted("remember", MIN["remember"], ("items", 0, "expected"))),
}
for _name, _scalar in (("string", {"type": "string", "value": "v"}), ("integer", {"type": "integer", "value": MIN_I64}),
                       ("float", {"type": "float", "value": 1.5}), ("boolean", {"type": "boolean", "value": True})):
    PIECES["scalar-" + _name] = (_scalar, {"type", "value"}, set(), hosted("remember", METADATA_HOST, ("items", 0, "metadata", "m")))
for _index, _target in enumerate(FORGET_TARGETS):
    PIECES[f"target-{_index}"] = (_target, set(_target), set(), hosted("forget", MIN["forget"], ("target",)))
for _method, _required in (
        ("remember", {"scope", "idempotency_key", "items"}), ("recall", {"scope"}), ("find_facts", {"scope", "slot"}),
        ("correct_facts", {"scope", "idempotency_key", "slot", "series", "slot_revision", "interval", "action"}),
        ("consolidate", {"scope", "idempotency_key", "inputs", "output"}),
        ("forget", {"scope", "idempotency_key", "target", "mode"}),
        ("get_receipt", {"scope", "operation", "idempotency_key"}), ("get_erasure", {"scope", "job_id"}),
        ("resume_erasure", {"scope", "idempotency_key", "job_id", "revision", "generation", "resume_token"})):
    PIECES["request-" + _method] = (FULL[_method], _required, set(), lambda piece, method=_method: (method, piece))

TEXT_KEYS = {"agent", "user", "session", "idempotency_key", "source_id", "source_kind", "uri", "transformation_id",
             "id", "kind", "content", "collection", "text", "payload_base64", "subject_id", "predicate",
             "slot_key", "destination_id", "field", "op", "embedding_provider", "job_id", "resume_token",
             "operation", "action", "mode", "merge", "disagreement", "selection", "future_observations",
             "disposition", "type"}
BOOLEAN_KEYS = {"require_complete_feed", "allow_partial", "history", "dry_run", "allow_job", "snippets",
                "provenance", "paths"}
LITERAL_TRUE_KEYS = {"absent", "session", "scope"}
INTEGER_KEYS = {"max_references", "deadline_ms", "limit", *LIMITS}
NUMBER_KEYS = {"importance", "confidence", "half_life_ms", "lambda"}
SCALAR_HOST = {**MIN["recall"], "filter": {"all": [{"field": "x", "op": "eq", "value": {"type": "string", "value": "v"}}]}}
# Where each schema of the fixtures' invalid structural requests sits in a minimal request.
FIXTURE_HOSTS = {
    "U64": hosted("forget", SERIES_HOST, ("target", "series", "revision")),
    "I64": hosted("remember", MIN["remember"], ("items", 0, "observed_at_ms")),
    "Source": PIECES["source"][3], "Scope": PIECES["scope"][3], "Interval": PIECES["interval"][3],
    "Expected": PIECES["expected-absent"][3], "Target": PIECES["target-0"][3], "Item": PIECES["item"][3],
    "Scalar": hosted("recall", SCALAR_HOST, ("filter", "all", 0, "value")),
    "Condition": PIECES["condition"][3], "Ranking": PIECES["ranking"][3],
    "RecallRequest": lambda value: ("recall", value),
    "CorrectionRequest": lambda value: ("correct_facts", value),
    "FactsRequest": lambda value: ("find_facts", value),
}
# Invalid-request fixtures the client refuses itself because they violate a form (names, JSON
# types, canonical integers, null) rather than a semantic rule; the integer-invalid-* ones join them.
LOCAL_REFUSALS = {"source-required-kind", "source-unknown-field", "old-validity-name-rejected", "expected-invalid-90",
                  "expected-invalid-91", "expected-invalid-92", "typed-boolean-not-integer",
                  "target-multiple-branches-rejected", "correction-close-requires-time"}
PENDING = {"receipt": {"id": "r2", "operation": "resume_erasure", "scope": {"tenant": "@root", "agent": "a"},
                       "sequence": "9", "digest_version": "memory-request-v1"},
           "replayed": False, "outcomes": [],
           "feed": {"mode": "ordinary_changes", "temporal_complete": False, "ttl_complete": False,
                    "consumer_purge_acknowledged": False},
           "remaining_work": None, "state": "pending",
           "erasure": {"id": "j1", "scope": {"tenant": "@root", "agent": "a"}, "target": {"scope": True},
                       "revision": "9", "generation": "1", "state": "pending", "logical_visibility": "hidden",
                       "deleted_count": "1", "remaining_count": None, "resume_token": "opaque2"},
           "erasure_notice": {"live_state_only": True, "secure_erasure": False, "external_copies": "operator_managed",
                              "restore_requires_manifest": True, "backup_history_purge": "not_verified",
                              "manifest_id": "manifest1"}}
# Contract section 8, first JSON exchange: every optional field absent.
REMEMBER_MINIMAL = {"receipt": {"id": "r1", "operation": "remember", "scope": {"tenant": "@root", "agent": "a"},
                                "sequence": "7", "digest_version": "memory-request-v1"},
                    "replayed": False, "outcomes": [{"id": "e1", "action": "created", "revision": "1"}],
                    "feed": {"mode": "ordinary_changes", "temporal_complete": False, "ttl_complete": False,
                             "consumer_purge_acknowledged": False}, "remaining_work": "0"}


class MemoryTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        CapabilityTests.setUpClass.__func__(cls)
        # Several tests abandon requests on purpose (timeouts, drops): keep the server quiet.
        cls.server.handle_error = lambda *_: None

    tearDownClass = classmethod(CapabilityTests.tearDownClass.__func__)
    setUp = CapabilityTests.setUp

    @property
    def base_url(self):
        return f"http://127.0.0.1:{self.server.server_port}"

    def reply(self, status, value, headers=None):
        Scripted.respond = lambda *_: (status, value, headers or {})

    def call(self, method, request, client=None, binding="notes"):
        return getattr((client or self.client).memory(binding), method)(request)

    def assertWire(self, actual, expected, message=None):
        self.assertEqual(wire(actual), wire(expected), message)

    def assertRefused(self, method, request, label=None):
        """Invalid input fails locally with ValueError and sends nothing."""
        before = len(Scripted.log)
        with self.assertRaises(ValueError, msg=label):
            self.call(method, request)
        self.assertEqual(len(Scripted.log), before, label)

    def test_memory_facade_is_binding_local_and_makes_no_request(self):
        notes = self.client.memory("notes")
        self.assertIsInstance(notes, vlite_client.Memory)
        self.assertEqual(notes.binding, "notes")
        for name in ("a", "a1", "a-b_c", "a" * 63, "z9"):
            self.assertEqual(self.client.memory(name).binding, name)
        for bad in ("", "A", "1a", "-a", "_a", "a" * 64, "a/b", "a b", "a.b", "a?x", "a#x", "a%2fb", "\u00e9",
                    "a\n", "../x", "a\u0660", None, 5, b"notes", ["notes"]):
            with self.subTest(binding=bad), self.assertRaises(ValueError):
                self.client.memory(bad)
        self.assertEqual(Scripted.log, [])

    def test_each_method_posts_its_route_with_exact_headers_and_body(self):
        urllib_headers = {"host", "user-agent", "accept-encoding", "connection", "content-length"}
        for operation, method, route in MEMORY_OPERATIONS:
            inventory = inventory_body(operation)
            self.assertEqual((inventory["method"], inventory["path"]), ("POST", "/v1/memory/notes" + route))
            name, status, body = next(r for r in exchange_responses(operation) if r[1] < 300)
            Scripted.log.clear()
            Scripted.headers_seen.clear()
            self.reply(status, body)
            result = self.call(method, copy.deepcopy(inventory["body"]))
            self.assertWire(result, body, operation)
            self.assertEqual(len(Scripted.log), 1, operation)
            self.assertEqual(Scripted.log[0][:2], ("POST", "/v1/memory/notes" + route), operation)
            self.assertWire(Scripted.log[0][2], inventory["body"], operation)
            headers = {key.lower(): value for key, value in Scripted.headers_seen[0].items()}
            self.assertEqual(headers["authorization"], "Bearer " + TOKEN)
            self.assertEqual(headers["accept"], "application/json")
            self.assertEqual(headers["content-type"], "application/json")
            # The key travels in the body only: no header alias, tenant or other extra header.
            self.assertEqual(set(headers) - urllib_headers, {"authorization", "accept", "content-type"}, operation)

    def test_facades_stay_bound_to_their_own_binding(self):
        first, second = self.client.memory("first"), self.client.memory("second-1")
        for facade in (first, second, first, second):
            facade.get_erasure(copy.deepcopy(MIN["get_erasure"]))
        self.assertEqual([entry[1] for entry in Scripted.log],
                         ["/v1/memory/first/erasures/get", "/v1/memory/second-1/erasures/get"] * 2)
        self.assertEqual((first.binding, second.binding), ("first", "second-1"))
        for _, _, body in Scripted.log:
            self.assertNotIn("binding", body)

    def test_fixture_success_exchanges_return_the_full_response_and_send_the_fixture_body(self):
        # Integer-form fixture requests must serialize like their canonical-string twins; the
        # one without a twin is patched by hand with the strings it must become.
        twins = {"remember-integer-observation": "remember-section8-created",
                 "remember-replay-typed-revision-integer": "remember-replay-typed-revision-string"}
        patches = {"consolidate-canonical-input-set-replay": ((("inputs", 0, "revision"), "1"),
                                                              (("inputs", 1, "revision"), "1"))}
        exercised = 0
        for operation, method, route in MEMORY_OPERATIONS:
            cases = {case["name"]: case for case in memory_fixture(operation)["cases"]}
            for name, case in cases.items():
                status, body = case["expected"]["status"], case["expected"]["body"]
                if status not in (200, 202) or has_directive(case["request"]["body"]) or has_directive(body):
                    continue
                expected = copy.deepcopy(cases[twins.get(name, name)]["request"]["body"])
                for path, value in patches.get(name, ()):
                    node = expected
                    for step in path[:-1]:
                        node = node[step]
                    node[path[-1]] = value
                self.reply(status, body)
                Scripted.log.clear()
                result = self.call(method, copy.deepcopy(case["request"]["body"]))
                self.assertWire(result, body, name)
                self.assertEqual(Scripted.log[0][:2], ("POST", "/v1/memory/notes" + route), name)
                self.assertWire(Scripted.log[0][2], expected, name)
                exercised += 1
        self.assertGreaterEqual(exercised, 46)

    def test_every_structural_response_shape_parses_including_pending_202(self):
        statuses, shapes = set(), 0
        for operation, method, _ in MEMORY_OPERATIONS:
            for name, status, body in exchange_responses(operation):
                if status >= 400:
                    continue
                self.reply(status, body)
                self.assertWire(self.call(method, copy.deepcopy(MIN[method])), body, name)
                statuses.add(status)
                shapes += 1
        self.assertEqual((statuses, shapes), ({200, 202}, 10))

    def test_pending_202_is_a_normal_response_for_forget_and_resume(self):
        for method in ("forget", "resume_erasure"):
            Scripted.log.clear()
            self.reply(202, PENDING)
            result = self.call(method, copy.deepcopy(MIN[method]))
            self.assertWire(result, PENDING, method)
            self.assertIsNone(result["remaining_work"])
            self.assertIsNone(result["erasure"]["remaining_count"])
            self.assertEqual((result["state"], result["erasure"]["revision"], result["erasure"]["resume_token"]),
                             ("pending", "9", "opaque2"))
            self.assertEqual(len(Scripted.log), 1)
        for status in (200, 202):
            self.reply(status, {**PENDING, "state": "retention_set"})
            self.assertEqual(self.call("forget", copy.deepcopy(MIN["forget"]))["state"], "retention_set")

    def test_consolidate_returns_either_union_variant_whole(self):
        mutation = exchange_responses("remember")[0][2]
        plan = exchange_responses("consolidate")[0][2]
        self.assertIn("dry_run", plan)
        for body in (mutation, plan):
            self.reply(200, body)
            result = self.call("consolidate", copy.deepcopy(MIN["consolidate"]))
            self.assertWire(result, body)
        self.assertEqual((plan["dry_run"], plan["user_operations"], plan["encoded_bytes"], plan["remaining_work"]),
                         (True, "4", "512", "0"))

    def test_unknown_response_fields_do_not_break_parsing_and_absent_fields_stay_absent(self):
        for operation, method, _ in MEMORY_OPERATIONS:
            name, status, body = next(r for r in exchange_responses(operation) if r[1] < 300)
            extended = with_unknown_fields(body)
            self.reply(status, extended)
            self.assertWire(self.call(method, copy.deepcopy(MIN[method])), extended, name)
        name, status, body = exchange_responses("remember")[0]
        self.reply(status, REMEMBER_MINIMAL)
        result = self.call("remember", copy.deepcopy(MIN["remember"]))
        self.assertWire(result, REMEMBER_MINIMAL)
        for key in ("state", "erasure", "erasure_notice"):
            self.assertNotIn(key, result)
        self.assertNotIn("retry_until_ms", result["receipt"])
        self.assertNotIn("expires_at_ms", result["outcomes"][0])

    def test_response_integers_stay_canonical_strings(self):
        body = copy.deepcopy(exchange_responses("remember")[0][2])
        body["receipt"]["sequence"] = MAX_ID
        body["receipt"]["retry_until_ms"] = "0"
        body["outcomes"][0]["revision"] = MAX_ID
        recall = copy.deepcopy(exchange_responses("recall")[0][2])
        recall["meta"]["as_of_ms"] = MIN_I64
        recall["meta"]["sequence"] = MAX_ID
        for method, value in (("remember", body), ("recall", recall)):
            self.reply(200, value)
            result = self.call(method, copy.deepcopy(MIN[method]))
            self.assertWire(result, value)
        self.assertEqual((result["meta"]["as_of_ms"], result["meta"]["sequence"]), (MIN_I64, MAX_ID))
        self.reply(200, body)
        result = self.call("remember", copy.deepcopy(MIN["remember"]))
        self.assertEqual((type(result["receipt"]["sequence"]), result["receipt"]["sequence"],
                          result["receipt"]["retry_until_ms"], result["outcomes"][0]["revision"]),
                         (str, MAX_ID, "0", MAX_ID))

    def test_a_success_body_that_is_not_a_json_object_is_a_transport_error(self):
        for raw in ("[]", "null", "1", '"x"', "true", "", "[{}]", "not json", "{"):
            for method in METHODS:
                with self.subTest(body=raw, method=method):
                    Scripted.respond = lambda *_, raw=raw: (200, raw, {})
                    before = len(Scripted.log)
                    with self.assertRaises(TransportError):
                        self.call(method, copy.deepcopy(MIN[method]))
                    self.assertEqual(len(Scripted.log), before + 1)

    def test_integer_inputs_serialize_as_canonical_decimal_strings_at_every_position(self):
        for method in METHODS:
            full, twin = copy.deepcopy(FULL[method]), integer_twin(method)
            if INTEGER_POSITIONS[method]:
                self.assertNotEqual(wire(twin), wire(full), method)
            for label, request in (("string", full), ("integer", twin)):
                Scripted.log.clear()
                self.call(method, copy.deepcopy(request))
                self.assertWire(Scripted.log[0][2], full, f"{method} {label}")
        twins = ({"ids": [{"id": "r1", "revision": 18446744073709551615}, {"id": "r3", "revision": 0}]},
                 {"series": {"id": "r1", "revision": 18446744073709551615}},
                 {"entity": {"id": "r1", "revision": 18446744073709551615}}, {"session": True}, {"scope": True})
        for target, twin in zip(FORGET_TARGETS, twins):
            for form in (target, twin):
                Scripted.log.clear()
                self.call("forget", {**MIN["forget"], "target": copy.deepcopy(form)})
                self.assertWire(Scripted.log[0][2], {**MIN["forget"], "target": target})
        for form in ({"revision": 1}, {"revision": "1"}):
            self.call("remember", remember_with(expected=form))
            self.assertEqual(Scripted.log[-1][2]["items"][0]["expected"], {"revision": "1"})
        for form in ({"type": "integer", "value": -5}, {"type": "integer", "value": "-5"}):
            self.call("recall", {**MIN["recall"], "filter": {"all": [{"field": "n", "op": "eq", "value": form}]}})
            self.assertWire(Scripted.log[-1][2]["filter"]["all"][0]["value"], {"type": "integer", "value": "-5"})

    def test_integer_forms_follow_the_contract_ranges(self):
        u64_positions = (
            ("forget", lambda v: {**MIN["forget"], "target": {"series": {"id": "m", "revision": v}}},
             lambda body: body["target"]["series"]["revision"]),
            ("resume_erasure", lambda v: {**MIN["resume_erasure"], "generation": v}, lambda body: body["generation"]),
            ("remember", lambda v: remember_with(expected={"revision": v}),
             lambda body: body["items"][0]["expected"]["revision"]),
            ("forget", lambda v: {**MIN["forget"], "mode": "expire", "expires_at_ms": v},
             lambda body: body["expires_at_ms"]))
        i64_positions = (
            ("remember", lambda v: remember_with(observed_at_ms=v), lambda body: body["items"][0]["observed_at_ms"]),
            ("recall", lambda v: {**MIN["recall"], "as_of_ms": v}, lambda body: body["as_of_ms"]),
            ("find_facts", lambda v: {**MIN["find_facts"], "as_of_ms": v}, lambda body: body["as_of_ms"]),
            ("correct_facts", lambda v: {**MIN["correct_facts"], "at_ms": v}, lambda body: body["at_ms"]),
            # Interval endpoints are the only nullable integers: null is sent, never dropped.
            *[("correct_facts", lambda v, name=name, end=end: {**MIN["correct_facts"], name: {
                "valid_from_ms": v if end == "valid_from_ms" else None,
                "valid_to_ms": v if end == "valid_to_ms" else None}},
               lambda body, name=name, end=end: body[name][end], True)
              for name in ("interval", "correction_interval") for end in ("valid_from_ms", "valid_to_ms")],
            *[("remember", lambda v, end=end: remember_with(assertion={**ASSERTION, "interval": {
                "valid_from_ms": v if end == "valid_from_ms" else None,
                "valid_to_ms": v if end == "valid_to_ms" else None}}),
               lambda body, end=end: body["items"][0]["assertion"]["interval"][end], True)
              for end in ("valid_from_ms", "valid_to_ms")])
        valid_u64 = [("0", "0"), (0, "0"), ("7", "7"), (7, "7"), (MAX_ID, MAX_ID), (18446744073709551615, MAX_ID),
                     (2 ** 63, "9223372036854775808"), ("9223372036854775808", "9223372036854775808")]
        invalid_u64 = ["-1", -1, "18446744073709551616", 2 ** 64, "00", "01", "+1", " 1", "1 ", "1\n", "1.0", 1.0,
                       True, False, None, "", "0x1", "1_0", "\uff11", "\u0661\u0662", "1\u0662", b"1", [1], {"a": 1},
                       float("nan"), "1e3", 1e3, "9" * 30, 10 ** 30]
        valid_i64 = [("0", "0"), (0, "0"), ("-1", "-1"), (-1, "-1"), ("5", "5"), (MIN_I64, MIN_I64),
                     (-2 ** 63, MIN_I64), (MAX_I64, MAX_I64), (2 ** 63 - 1, MAX_I64)]
        invalid_i64 = ["-0", "-01", "00", "+1", "-9223372036854775809", -2 ** 63 - 1, "9223372036854775808",
                       2 ** 63, MAX_ID, 2 ** 64, True, False, None, 1.0, "1.0", " 1", "-", "--1", "", "1e3",
                       "\u0661", "\u22121", "-1\u0662", float("inf"), b"1"]
        for positions, valid, invalid in ((u64_positions, valid_u64, invalid_u64), (i64_positions, valid_i64, invalid_i64)):
            for method, build, extract, *nullable in positions:
                nullable = bool(nullable)
                for given, canonical in valid + ([(None, None)] if nullable else []):
                    with self.subTest(method=method, given=given):
                        Scripted.log.clear()
                        self.call(method, build(given))
                        self.assertWire(extract(Scripted.log[0][2]), canonical)
                for given in invalid:
                    if given is None and nullable:
                        continue
                    with self.subTest(method=method, invalid=given):
                        self.assertRefused(method, build(given))

    def test_budgets_stay_json_integers_and_are_never_clamped_or_checked(self):
        limits = [c for c in memory_fixture("remember")["structural"] if c["schema"] == "Limits" and c["request"]]
        self.assertEqual((sum(c["valid"] for c in limits), sum(not c["valid"] for c in limits)), (24, 24))
        recall = exchange_responses("recall")[0][2]
        for case in limits:
            request = {**MIN["recall"], "limits": copy.deepcopy(case["value"])}
            if case["valid"]:
                self.reply(200, recall)
                self.call("recall", copy.deepcopy(request))
            else:
                Scripted.respond = lambda *_: memory_error(422, "invalid_input", {"field": "limits"})
                with self.assertRaises(VLiteError) as raised:
                    self.call("recall", copy.deepcopy(request))
                self.assertEqual((raised.exception.status, raised.exception.code), (422, "invalid_input"))
            self.assertWire(Scripted.log[-1][2], request, case["name"])
        for key, method, host in (("deadline_ms", "remember", MIN["remember"]), ("max_references", "remember", MIN["remember"]),
                                  ("limit", "recall", MIN["recall"]), ("limit", "find_facts", MIN["find_facts"])):
            for value in (0, 1, 250, 5000, 5001, -1, 2 ** 64):
                with self.subTest(key=key, value=value):
                    self.reply(200, {})
                    self.call(method, {**host, key: value})
                    self.assertWire(Scripted.log[-1][2], {**host, key: value})
        self.reply(200, {})
        self.call("find_facts", {**MIN["find_facts"], "limits": {"deadline_ms": 5001, "candidates": 0}})
        self.assertEqual(Scripted.log[-1][2]["limits"], {"deadline_ms": 5001, "candidates": 0})

    def test_every_object_checks_required_optional_unknown_and_foreign_values(self):
        for label, (piece, required, nullable, build) in PIECES.items():
            self.check_piece(label, piece, required, nullable, build)

    def check_piece(self, label, piece, required, nullable, build):
        with self.subTest(piece=label, check="identity"):
            method, request = build(copy.deepcopy(piece))
            Scripted.log.clear()
            self.call(method, copy.deepcopy(request))
            self.assertWire(Scripted.log[-1][2], request)
        for key in piece:
            with self.subTest(piece=label, key=key, check="omit"):
                method, request = build({name: value for name, value in piece.items() if name != key})
                if key in required:
                    self.assertRefused(method, request)
                else:
                    Scripted.log.clear()
                    self.call(method, copy.deepcopy(request))
                    self.assertWire(Scripted.log[-1][2], request)
            with self.subTest(piece=label, key=key, check="null"):
                method, request = build({**piece, key: None})
                if key in nullable:
                    Scripted.log.clear()
                    self.call(method, copy.deepcopy(request))
                    self.assertWire(Scripted.log[-1][2], request)
                else:
                    self.assertRefused(method, request)
            with self.subTest(piece=label, key=key, check="foreign"):
                self.assertRefused(*build({**piece, key: b"x"}))
                if isinstance(piece[key], list):
                    for foreign in ([b"x"], "", "s", (), {}):
                        self.assertRefused(*build({**piece, key: foreign}))
                if key == "metadata" and isinstance(piece[key], dict):
                    self.assertRefused(*build({**piece, key: {"m": b"x"}}))
                    self.assertRefused(*build({**piece, key: {1: {"type": "string", "value": "v"}}}))
                    self.assertRefused(*build({**piece, key: {"m": {"type": "string", "value": "v"}, "n": None}}))
                if key in TEXT_KEYS and isinstance(piece[key], str):
                    for foreign in (5, True, 1.5):
                        self.assertRefused(*build({**piece, key: foreign}))
                if key in BOOLEAN_KEYS and isinstance(piece[key], bool):
                    for foreign in (1, 0, "true", 1.0):
                        self.assertRefused(*build({**piece, key: foreign}))
                if key in LITERAL_TRUE_KEYS and piece[key] is True:
                    for foreign in (False, 1, 0, "true", 1.0):
                        self.assertRefused(*build({**piece, key: foreign}))
                if key in INTEGER_KEYS and isinstance(piece[key], int):
                    for foreign in (True, 1.0, "1", [1]):
                        self.assertRefused(*build({**piece, key: foreign}))
                if key in NUMBER_KEYS:
                    for foreign in (True, "1", float("nan"), float("inf"), float("-inf")):
                        self.assertRefused(*build({**piece, key: foreign}))
        with self.subTest(piece=label, check="unknown"):
            self.assertRefused(*build({**piece, "zz_unknown": 1}))
        for foreign in (None, b"x", [], "s", 1, True):
            with self.subTest(piece=label, check="shape", value=foreign):
                self.assertRefused(*build(foreign))

    def test_malformed_unions_never_send(self):
        expected = [{"absent": False}, {"absent": 1}, {"absent": "true"}, {"absent": None}, {}, {"revision": 1.5},
                    {"absent": True, "revision": "1"}, {"revision": True}, {"revision": "01"}, {"zz": 1}, None,
                    "absent", "any", True, [], [{"absent": True}]]
        for value in expected:
            self.assertRefused("remember", remember_with(expected=value), value)
            self.assertRefused("remember", remember_with(assertion={**ASSERTION, "slot_expected": value}), value)
        targets = [{}, {"scope": True, "session": True}, {"session": False}, {"scope": False}, {"scope": 1},
                   {"scope": "true"},
                   {"ids": [REF], "scope": True}, {"ids": REF}, {"ids": [True]}, {"series": [REF]},
                   {"entity": {"id": "x"}}, {"entity": {"revision": "1"}}, {"zz": True}, None, "scope", True, [],
                   [{"scope": True}], {"ids": None}, {"ids": [REF], "series": REF}]
        for value in targets:
            self.assertRefused("forget", {**MIN["forget"], "target": value}, value)
        scalars = [{"type": "decimal", "value": "1"}, {"type": ["x"], "value": "1"}, {"type": None, "value": 1},
                   {"type": 5, "value": 5}, {"type": "string", "value": 1}, {"type": "string", "value": None},
                   {"type": "integer", "value": True}, {"type": "integer", "value": 1.5},
                   {"type": "integer", "value": "01"}, {"type": "integer", "value": str(2 ** 63)},
                   {"type": "float", "value": "1.5"}, {"type": "float", "value": True},
                   {"type": "float", "value": float("nan")}, {"type": "boolean", "value": 1},
                   {"type": "boolean", "value": "true"}, {"type": "string"}, {"value": "x"}, {}, "string", None, 5,
                   {"type": "string", "value": "x", "zz": 1}]
        for value in scalars:
            self.assertRefused("recall", {**MIN["recall"], "filter": {"all": [{"field": "x", "op": "eq", "value": value}]}}, value)
            self.assertRefused("remember", remember_with(metadata={"m": value}), value)
            self.assertRefused("remember", {**MIN["remember"], "evidence": [{"collection": "c", "record": "1", "metadata": {"m": value}}]}, value)

    def test_foreign_and_legacy_names_are_never_sent(self):
        for method in METHODS:
            for extra in ({"tenant": "t"}, {"binding": "notes"}, {"Idempotency-Key": "k"}, {"idempotency-key": "k"},
                          {"from_ms": "0"}, {"to_ms": "1"}, {"collection": "c"}, {"graph": "g"}):
                with self.subTest(method=method, extra=extra):
                    self.assertRefused(method, {**copy.deepcopy(MIN[method]), **extra})
        for names in ({"from_ms": "0", "to_ms": "1"}, {"valid_from": "0", "valid_to": "1"},
                      {"valid_from_ms": "0", "valid_to_ms": "1", "from_ms": "0"}):
            self.assertRefused("correct_facts", {**MIN["correct_facts"], "interval": names}, names)
        self.assertRefused("recall", {**MIN["recall"], "scope": {"agent": "a", "tenant": "t"}})
        self.assertEqual(Scripted.log, [])

    def test_required_mutation_key_is_never_generated(self):
        for method in ("remember", "correct_facts", "consolidate", "forget", "resume_erasure", "get_receipt"):
            request = copy.deepcopy(MIN[method])
            del request["idempotency_key"]
            self.assertRefused(method, request, method)
        for method in METHODS:
            self.assertRefused(method, {}, method)
            for bad in (None, [], "request", 5, b"{}", [MIN[method]]):
                self.assertRefused(method, bad, method)

    def test_semantic_rules_belong_to_the_server_and_requests_are_forwarded_unchanged(self):
        filter_with = lambda *conditions: {**MIN["recall"], "filter": {"all": list(conditions)}}
        requests = [
            ("remember", remember_with(content="x" * 4097)),
            ("remember", {**MIN["remember"], "idempotency_key": "k" * 257}),
            ("remember", {**MIN["remember"], "idempotency_key": ""}),
            ("remember", {**MIN["remember"], "items": []}),
            ("remember", {**MIN["remember"], "items": [{**MIN_ITEM, "id": f"e{n}"} for n in range(4097)]}),
            ("remember", remember_with(kind="Entity")),
            ("remember", remember_with(kind="fact")),
            ("remember", remember_with(assertion=ASSERTION)),
            ("remember", remember_with(expected={"revision": "0"})),
            ("remember", remember_with(importance=2)),
            ("remember", remember_with(importance=-0.5)),
            ("remember", remember_with(merge="merge_provenance")),
            ("remember", {**MIN["remember"], "deadline_ms": 0}),
            ("remember", {**MIN["remember"], "max_references": 257}),
            ("recall", {**MIN["recall"], "kinds": []}),
            ("recall", {**MIN["recall"], "scopes": [{"agent": f"a{n}"} for n in range(17)]}),
            ("recall", {**MIN["recall"], "scope": {"agent": "a", "session": "s"}}),
            ("recall", {**MIN["recall"], "scope": {"agent": "a\u0000"}}),
            ("recall", {"scope": {"agent": "a"}, "vector": [3.5e+38]}),
            ("recall", {"scope": {"agent": "a"}, "text": "Ada", "embedding_provider": "p", "vector": [1]}),
            ("recall", {"scope": {"agent": "a"}, "entity_seeds": ["e1"], "embedding_provider": "p"}),
            ("recall", {"scope": {"agent": "a"}}),
            ("recall", {**MIN["recall"], "limit": 0}),
            ("recall", {**MIN["recall"], "limit": 101}),
            ("recall", {**MIN["recall"], "mode": "lazy"}),
            ("recall", {**MIN["recall"], "ranking": {"weights": [2, 0, 0]}}),
            ("recall", {**MIN["recall"], "ranking": {"weights": [0, 1, 0]}}),
            ("recall", filter_with({"field": "x", "op": "exists", "value": {"type": "boolean", "value": True}})),
            ("recall", filter_with({"field": "x", "op": "eq"})),
            ("recall", filter_with({"field": "x", "op": "gt", "value": {"type": "boolean", "value": True}})),
            ("recall", filter_with({"field": "x", "op": "like", "value": {"type": "string", "value": "a"}})),
            ("find_facts", {**MIN["find_facts"], "history": True, "as_of_ms": "100"}),
            ("correct_facts", {**MIN["correct_facts"], "action": "close"}),
            ("correct_facts", {**MIN["correct_facts"], "action": "close", "at_ms": "1", "replacement": MIN_ITEM}),
            ("correct_facts", {**MIN["correct_facts"], "interval": {"valid_from_ms": "0", "valid_to_ms": "0"}}),
            ("consolidate", {**MIN["consolidate"], "inputs": []}),
            ("consolidate", {**MIN["consolidate"], "inputs": [{"id": "m", "revision": "1", "disposition": "purge"}]}),
            ("forget", {**MIN["forget"], "target": {"ids": []}}),
            ("forget", {**MIN["forget"], "mode": "expire"}),
            ("forget", {**MIN["forget"], "expires_at_ms": "5"}),
            ("get_receipt", {**MIN["get_receipt"], "operation": "recall"}),
            ("get_erasure", {"scope": {"agent": "a"}, "job_id": ""}),
            ("resume_erasure", {**MIN["resume_erasure"], "revision": "0"}),
            ("resume_erasure", {**MIN["resume_erasure"], "resume_token": "t" * 4097}),
        ]
        Scripted.respond = lambda *_: memory_error(422, "invalid_input", {"field": "x"})
        for method, request in requests:
            Scripted.log.clear()
            with self.subTest(method=method), self.assertRaises(VLiteError) as raised:
                self.call(method, copy.deepcopy(request))
            self.assertEqual((raised.exception.status, raised.exception.code), (422, "invalid_input"))
            self.assertEqual(len(Scripted.log), 1)
            self.assertWire(Scripted.log[0][2], request)

    def test_fixture_invalid_requests_split_between_local_forms_and_server_semantics(self):
        cases = [c for operation, _, _ in MEMORY_OPERATIONS for c in memory_fixture(operation)["structural"]
                 if c["request"] and not c["valid"] and c["schema"] in FIXTURE_HOSTS and not has_directive(c["value"])]
        self.assertEqual(len(cases), 38)
        forms = {c["name"] for c in cases if c["name"].startswith(("integer-invalid-U64-", "integer-invalid-I64-"))}
        local = LOCAL_REFUSALS | forms
        self.assertEqual((len(forms), len(local - forms), local <= {c["name"] for c in cases}), (12, 9, True))
        Scripted.respond = lambda *_: memory_error(422, "invalid_input", {"field": "x"})
        for case in cases:
            method, request = FIXTURE_HOSTS[case["schema"]](copy.deepcopy(case["value"]))
            with self.subTest(case=case["name"]):
                if case["name"] in local:
                    self.assertRefused(method, request, case["name"])
                    continue
                Scripted.log.clear()
                with self.assertRaises(VLiteError) as raised:
                    self.call(method, copy.deepcopy(request))
                self.assertEqual((raised.exception.status, raised.exception.code, len(Scripted.log)),
                                 (422, "invalid_input", 1))
                self.assertWire(Scripted.log[0][2], request, case["name"])

    def test_numbers_strings_and_vectors_are_not_rewritten(self):
        vector = [0.1, 16777217.0, 3.5e38, 1, -0.0, 1e-45, 5e-324, 2 ** 70]
        request = {**MIN["recall"], "vector": vector,
                   "ranking": {"weights": [1, 0, 0], "half_life_ms": 10, "lambda": 0.1}}
        self.call("recall", copy.deepcopy(request))
        self.assertWire(Scripted.log[-1][2], request)
        self.assertEqual([type(v) for v in Scripted.log[-1][2]["vector"]], [float, float, float, int, float, float, float, int])

        class Real(float):
            """A float subclass (numpy.float64 is one) is still a JSON number."""

        self.call("recall", {**MIN["recall"], "vector": [Real(0.5), 1]})
        self.assertWire(Scripted.log[-1][2]["vector"], [0.5, 1])
        text = "\u96ea /+?%\n\t\u0000\"\\ \U0001f600 \u00e9\u0301"
        for method, request in (("remember", remember_with(content=text, id=text)),
                                ("recall", {**MIN["recall"], "text": text}),
                                ("get_erasure", {"scope": {"agent": text}, "job_id": text})):
            self.call(method, copy.deepcopy(request))
            self.assertWire(Scripted.log[-1][2], request)
        for number in (float("nan"), float("inf"), float("-inf")):
            self.assertRefused("remember", remember_with(importance=number))
            self.assertRefused("remember", remember_with(confidence=number))
            self.assertRefused("recall", {**MIN["recall"], "vector": [1, number]})
            self.assertRefused("recall", {**MIN["recall"], "ranking": {"weights": [number, 0, 0]}})
            self.assertRefused("recall", {**MIN["recall"], "ranking": {"half_life_ms": number}})
            self.assertRefused("recall", {**MIN["recall"], "ranking": {"lambda": number}})
            self.assertRefused("recall", {**MIN["recall"], "filter": {"all": [{"field": "x", "op": "lt", "value": {"type": "float", "value": number}}]}})
        for lone in ("\ud800", "a\udfffb"):
            self.assertRefused("remember", remember_with(content=lone))
            self.assertRefused("remember", remember_with(metadata={lone: {"type": "string", "value": "v"}}))
            self.assertRefused("remember", remember_with(metadata={"m": {"type": "string", "value": lone}}))
            self.assertRefused("recall", {"scope": {"agent": lone}, "text": "t"})
            self.assertRefused("get_receipt", {**MIN["get_receipt"], "idempotency_key": lone})

    def test_nothing_is_embedded_split_paginated_defaulted_or_generated(self):
        self.call("recall", {"scope": {"agent": "a"}, "text": "Ada"})
        self.assertEqual(Scripted.log, [("POST", "/v1/memory/notes/recall", {"scope": {"agent": "a"}, "text": "Ada"})])
        partial = copy.deepcopy(exchange_responses("recall")[0][2])
        partial["meta"].update(complete=False, truncation=["output"])
        self.reply(200, partial)
        Scripted.log.clear()
        result = self.call("recall", {**MIN["recall"], "allow_partial": True})
        self.assertWire(result, partial)
        self.assertEqual(len(Scripted.log), 1)
        many = {**MIN["remember"], "items": [{**MIN_ITEM, "id": f"e{n}"} for n in range(4097)]}
        Scripted.log.clear()
        self.call("remember", copy.deepcopy(many))
        self.assertEqual(len(Scripted.log), 1)
        self.assertEqual(len(Scripted.log[0][2]["items"]), 4097)
        for method in METHODS:
            Scripted.log.clear()
            self.call(method, copy.deepcopy(MIN[method]))
            sent = Scripted.log[0][2]
            self.assertWire(sent, MIN[method], method)
            for generated in ("embedding_provider", "vector", "deadline_ms", "max_references", "limit", "limits",
                              "projection", "require_complete_feed", "as_of_ms", "allow_partial", "importance",
                              "merge", "dry_run", "allow_job", "history", "evidence", "tenant"):
                if generated not in MIN[method]:
                    self.assertNotIn(generated, sent, method)

    def test_the_callers_request_is_never_modified(self):
        for method in METHODS:
            for variant in (FULL[method], integer_twin(method), MIN[method]):
                request = copy.deepcopy(variant)
                self.call(method, request)
                self.assertWire(request, variant, method)
        for target in FORGET_TARGETS:
            request = {**MIN["forget"], "target": copy.deepcopy(target)}
            self.call("forget", request)
            self.assertWire(request["target"], target)

    def test_refusals_surface_code_status_request_id_and_details_for_every_method(self):
        cases = [
            (409, "memory_capability_unavailable", {"feature": "facts"}),
            (409, "memory_capability_unavailable", {"feature": "erasure_jobs"}),
            (409, "memory_deletion_pending", None),
            (412, "precondition_failed", {"field": "revision"}),
            (422, "invalid_input", {"field": "items", "limit": "4096", "requested": "4097"}),
            (422, "invalid_input", {"field": "x", "category": "budget", "feature": "y", "future": [1, {"a": None}]}),
            (504, "deadline_exceeded", None),
            (404, "memory_not_found", None),
            (404, "receipt_not_found", {"field": "idempotency_key"}),
            (404, "erasure_not_found", {}),
            (503, "commit_indeterminate", None),
        ]
        for status, code, details in cases:
            Scripted.respond = lambda *_, s=status, c=code, d=details: memory_error(s, c, d)
            for method in METHODS:
                with self.subTest(status=status, code=code, method=method):
                    Scripted.log.clear()
                    with self.assertRaises(VLiteError) as raised:
                        self.call(method, copy.deepcopy(MIN[method]))
                    error = raised.exception
                    self.assertNotIsInstance(error, ImportRejectedError)
                    self.assertEqual((error.status, error.code, error.request_id), (status, code, f"rid-{status}"))
                    self.assertEqual(str(error), f"HTTP {status}: {code}: m")
                    if details is None:
                        self.assertIsNone(error.details)
                    else:
                        self.assertWire(error.details, details)
                    self.assertEqual(len(Scripted.log), 1)

    def test_every_fixture_error_envelope_surfaces_unchanged(self):
        # The 34 section 6 envelopes rotate across the nine methods; each refusal exchange
        # uses its own operation's method.
        statuses = [(c["status"], c["value"]) for c in memory_fixture("remember")["structural"]
                    if c["schema"] == "exchange" and c["valid"] and c["name"].startswith("error-status-")]
        self.assertEqual(len(statuses), 34)
        pairs = [(METHODS[index % len(METHODS)], status, body) for index, (status, body) in enumerate(statuses)]
        for operation, method, _ in MEMORY_OPERATIONS:
            for case in memory_fixture(operation)["cases"]:
                if case["expected"]["status"] >= 400:
                    pairs.append((method, case["expected"]["status"], case["expected"]["body"]))
        self.assertEqual(len(pairs), 34 + 111)
        for method, status, body in pairs:
            self.reply(status, body, {"X-Request-ID": f"rid-{status}"})
            with self.subTest(status=status, code=body["error"]["code"], method=method):
                Scripted.log.clear()
                with self.assertRaises(VLiteError) as raised:
                    self.call(method, copy.deepcopy(MIN[method]))
                error = raised.exception
                self.assertEqual((error.status, error.code, error.request_id),
                                 (status, body["error"]["code"], f"rid-{status}"))
                self.assertEqual(str(error), f"HTTP {status}: {body['error']['code']}: {body['error']['message']}")
                self.assertIsNone(error.details)
                self.assertEqual(len(Scripted.log), 1)

    def test_malformed_error_details_and_bodies_never_hide_the_refusal(self):
        for details in ("text", ["x"], 5, True, None):
            Scripted.respond = lambda *_, d=details: (
                409, {"error": {"code": "memory_capability_unavailable", "message": "m", "details": d}}, {})
            with self.subTest(details=details), self.assertRaises(VLiteError) as raised:
                self.call("find_facts", copy.deepcopy(MIN["find_facts"]))
            error = raised.exception
            self.assertEqual((error.status, error.code, error.request_id, error.details),
                             (409, "memory_capability_unavailable", None, None))
        for status, raw in ((409, "<html>"), (404, "<html>"), (502, ""), (500, "null"), (422, '{"error": "x"}'),
                            (422, '{"error": {"code": "invalid_input"}}'), (404, '{"error": {"message": "m"}}'),
                            (404, '{"error": {"code": "not_found"}}')):
            Scripted.respond = lambda *_, s=status, r=raw: (s, r, {})
            for method in METHODS:
                with self.subTest(status=status, body=raw, method=method), self.assertRaises(VLiteError) as raised:
                    self.call(method, copy.deepcopy(MIN[method]))
                error = raised.exception
                self.assertEqual((error.status, error.code, error.request_id, error.details),
                                 (status, "http_error", None, None))

    def test_only_404_not_found_means_unsupported_for_all_nine_methods(self):
        Scripted.respond = lambda *_: memory_error(404, "not_found")
        for method in METHODS:
            before = len(Scripted.log)
            with self.subTest(method=method), self.assertRaises(UnsupportedError) as raised:
                self.call(method, copy.deepcopy(MIN[method]))
            self.assertNotIsInstance(raised.exception, VLiteError)
            cause = raised.exception.__cause__
            self.assertIsInstance(cause, VLiteError)
            self.assertEqual((cause.status, cause.code, cause.request_id), (404, "not_found", "rid-404"))
            self.assertEqual(len(Scripted.log), before + 1)
        for status, code in ((404, "memory_not_found"), (404, "receipt_not_found"), (404, "erasure_not_found"),
                             (404, "kv_not_found"), (404, "collection_not_found"), (404, "Not_Found"),
                             (404, "not-found"), (409, "not_found"), (400, "not_found"), (410, "not_found"),
                             (500, "not_found"), (403, "not_found"), (401, "not_found")):
            Scripted.respond = lambda *_, s=status, c=code: memory_error(s, c)
            for method in METHODS:
                with self.subTest(status=status, code=code, method=method), self.assertRaises(VLiteError) as raised:
                    self.call(method, copy.deepcopy(MIN[method]))
                self.assertEqual((raised.exception.status, raised.exception.code), (status, code))

    def test_errors_are_never_retried(self):
        for status, code, headers in ((409, "idempotency_conflict", None), (412, "precondition_failed", None),
                                      (422, "invalid_input", None), (429, "rate_limited", {"Retry-After": "1"}),
                                      (429, "quota_exceeded", {"Retry-After": "1"}), (500, "internal_error", None),
                                      (503, "busy", {"Retry-After": "1"}), (503, "unavailable", None),
                                      (503, "poisoned", None), (503, "commit_indeterminate", None),
                                      (504, "deadline_exceeded", None), (401, "unauthorized", None),
                                      (403, "forbidden_scope", None)):
            Scripted.respond = lambda *_, s=status, c=code, h=headers: memory_error(s, c, None, h)
            for method in METHODS:
                before = len(Scripted.log)
                with self.subTest(status=status, code=code, method=method), self.assertRaises(VLiteError):
                    self.call(method, copy.deepcopy(MIN[method]))
                self.assertEqual(len(Scripted.log), before + 1)

    def test_timeouts_are_transport_errors_and_nothing_is_resent(self):
        admitted, release = Event(), Event()

        def respond(*_):
            admitted.set()
            release.wait(5)
            return 200, None, {}

        Scripted.respond = respond
        client = Client(self.base_url, TOKEN, timeout=0.3)
        try:
            for method in METHODS:
                admitted.clear()
                before = len(Scripted.log)
                with self.subTest(method=method), self.assertRaises(TransportError):
                    self.call(method, copy.deepcopy(MIN[method]), client=client)
                self.assertTrue(admitted.wait(5), method)
                self.assertEqual(len(Scripted.log), before + 1, method)
        finally:
            release.set()

    def test_a_dropped_connection_is_a_transport_error_and_is_not_resent(self):
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen(16)
        listener.settimeout(0.05)
        accepted, stop = [], Event()

        def serve():
            while not stop.is_set():
                try:
                    connection, _ = listener.accept()
                except socket.timeout:
                    continue
                except OSError:
                    return
                with connection:
                    try:
                        accepted.append(connection.recv(65536))
                    except OSError:
                        accepted.append(b"")

        thread = Thread(target=serve, daemon=True)
        thread.start()
        try:
            client = Client(f"http://127.0.0.1:{listener.getsockname()[1]}", TOKEN, timeout=5)
            for method in METHODS:
                with self.subTest(method=method), self.assertRaises(TransportError):
                    self.call(method, copy.deepcopy(MIN[method]), client=client)
            time.sleep(0.1)
            self.assertEqual(len(accepted), len(METHODS))
        finally:
            stop.set()
            thread.join(5)
            listener.close()

    def test_caller_interruption_of_a_mutation_is_not_retried(self):
        from unittest.mock import patch
        original_open = self.client._opener.open

        def interrupted(*args, **kwargs):
            with original_open(*args, **kwargs):
                raise KeyboardInterrupt("caller cancelled")

        for method in ("remember", "correct_facts", "consolidate", "forget", "resume_erasure"):
            Scripted.log.clear()
            with patch.object(self.client._opener, "open", side_effect=interrupted):
                with self.subTest(method=method), self.assertRaises(KeyboardInterrupt):
                    self.call(method, copy.deepcopy(MIN[method]))
            self.assertEqual(len(Scripted.log), 1, method)

    def test_deadline_ms_is_the_engine_budget_never_the_http_timeout(self):
        admitted = Event()

        def slow(*_):
            admitted.set()
            time.sleep(0.4)
            return 200, exchange_responses("remember")[0][2], {}

        Scripted.respond = slow
        patient = Client(self.base_url, TOKEN, timeout=10)
        self.assertIn("receipt", self.call("remember", {**MIN["remember"], "deadline_ms": 1}, client=patient))
        self.assertEqual(Scripted.log[-1][2]["deadline_ms"], 1)
        self.call("recall", {**MIN["recall"], "limits": {"deadline_ms": 1}}, client=patient)
        self.assertEqual(Scripted.log[-1][2]["limits"], {"deadline_ms": 1})
        # A long engine deadline does not stretch a short HTTP timeout either.
        admitted.clear()
        before = len(Scripted.log)
        with self.assertRaises(TransportError):
            self.call("remember", {**MIN["remember"], "deadline_ms": 5000}, client=Client(self.base_url, TOKEN, timeout=0.1))
        self.assertTrue(admitted.wait(5))
        self.assertEqual(len(Scripted.log), before + 1)
        self.assertEqual(Scripted.log[-1][2]["deadline_ms"], 5000)
        # The HTTP timeout is never copied into the body.
        Scripted.respond = None
        self.call("remember", copy.deepcopy(MIN["remember"]), client=Client(self.base_url, TOKEN, timeout=7))
        self.assertWire(Scripted.log[-1][2], MIN["remember"])

    def test_redirects_are_not_followed_for_memory_posts(self):
        Scripted.respond = lambda *_: (307, None, {"Location": "/v1/memory/other/remember"})
        for method in METHODS:
            before = len(Scripted.log)
            with self.subTest(method=method), self.assertRaises(VLiteError) as raised:
                self.call(method, copy.deepcopy(MIN[method]))
            self.assertEqual((raised.exception.status, raised.exception.code), (307, "http_error"))
            self.assertEqual(len(Scripted.log), before + 1)

    def test_error_details_are_available_on_existing_routes_too(self):
        Scripted.respond = lambda *_: memory_error(422, "invalid_input", {"field": "namespace", "limit": "255"})
        with self.assertRaises(VLiteError) as raised:
            self.client.kv_get(collection="cache", namespace="n", key="k")
        self.assertEqual(raised.exception.details, {"field": "namespace", "limit": "255"})
        Scripted.respond = lambda *_: error(404, "kv_not_found")
        with self.assertRaises(VLiteError) as raised:
            self.client.kv_get(collection="cache", namespace="n", key="k")
        self.assertEqual((raised.exception.status, raised.exception.code), (404, "kv_not_found"))
        self.assertIsNone(raised.exception.details)
        self.assertIsNone(VLiteError(500, "internal_error", "m", None).details)
        self.assertEqual(VLiteError(409, "c", "m", "rid", {"feature": "f"}).details, {"feature": "f"})

    def test_local_errors_name_the_offending_field(self):
        cases = [
            ("remember", remember_with(observed_at_ms="01"), r"request\.items\[0\]\.observed_at_ms must be a canonical"),
            ("remember", remember_with(observed_at_ms=2 ** 63), r"request\.items\[0\]\.observed_at_ms must be in"),
            ("recall", {**MIN["recall"], "limits": {"candidates": True}}, r"request\.limits\.candidates must be an integer"),
            ("recall", {**MIN["recall"], "zz": 1}, r"request: unknown field 'zz' is never sent"),
            ("recall", {"text": "t"}, r"request: missing required field 'scope'"),
            ("remember", remember_with(metadata={"m": {"type": "x", "value": 1}}),
             r"request\.items\[0\]\.metadata\.m\.type must be one of"),
            ("remember", remember_with(metadata={1: {"type": "string", "value": "v"}}),
             r"request\.items\[0\]\.metadata key must be a string"),
            ("forget", {**MIN["forget"], "target": {"ids": [REF, {"id": "r"}]}},
             r"request\.target\.ids\[1\]: missing required field 'revision'"),
            ("forget", {**MIN["forget"], "target": {"scope": False}}, r"request\.target\.scope must be true"),
            ("correct_facts", {**MIN["correct_facts"], "interval": {"valid_from_ms": None}},
             r"request\.interval: missing required field 'valid_to_ms'"),
            ("get_erasure", {"scope": {"agent": "\ud800"}, "job_id": "j"},
             r"request\.scope\.agent must contain Unicode scalar values"),
            ("find_facts", {**MIN["find_facts"], "slot": {"subject_id": "s", "predicate": "p", "slot_key": "k", "zz": 1}},
             r"request\.slot: unknown field 'zz'"),
        ]
        for method, request, message in cases:
            with self.subTest(method=method, message=message), self.assertRaisesRegex(ValueError, message):
                self.call(method, request)
        self.assertEqual(Scripted.log, [])

    TYPED = {
        "MemoryScope": ({"agent"}, {"user", "session"}),
        "MemoryEffectiveScope": ({"tenant", "agent"}, {"user", "session"}),
        "MemoryExpectedAbsent": ({"absent"}, set()),
        "MemoryExpectedRevision": ({"revision"}, set()),
        "MemoryInterval": ({"valid_from_ms", "valid_to_ms"}, set()),
        "MemoryRef": ({"id", "revision"}, set()),
        "MemorySource": ({"source_id", "source_kind"}, {"uri", "transformation_id"}),
        "MemoryProvenance": ({"source_id", "source_kind", "content_digest"}, {"uri", "transformation_id"}),
        "MemoryStringScalar": ({"type", "value"}, set()),
        "MemoryIntegerScalar": ({"type", "value"}, set()),
        "MemoryFloatScalar": ({"type", "value"}, set()),
        "MemoryBooleanScalar": ({"type", "value"}, set()),
        "MemoryCondition": ({"field", "op"}, {"value"}),
        "MemoryFilter": (set(), {"all", "any"}),
        "MemoryEvidenceRef": ({"collection", "record"}, set()),
        "MemoryEvidence": ({"collection", "record"}, {"vector", "text", "payload_base64", "metadata"}),
        "MemoryAssertion": ({"subject_id", "predicate", "slot_key", "destination_id", "interval", "slot_expected"},
                            {"disagreement"}),
        "MemoryItem": ({"id", "kind", "content", "observed_at_ms", "source"},
                       {"importance", "confidence", "expires_at_ms", "provenance", "references", "evidence",
                        "metadata", "assertion", "expected", "merge"}),
        "MemoryMutation": ({"scope", "idempotency_key"}, {"max_references", "deadline_ms", "require_complete_feed"}),
        "MemoryRememberRequest": ({"scope", "idempotency_key", "items"},
                                  {"max_references", "deadline_ms", "require_complete_feed", "evidence"}),
        "MemoryRanking": (set(), {"selection", "weights", "half_life_ms", "future_observations", "lambda"}),
        "MemoryLimits": (set(), set(LIMITS)),
        "MemoryProjection": (set(), {"snippets", "provenance", "paths", "metadata"}),
        "MemoryRecallRequest": ({"scope"}, {"scopes", "vector", "text", "embedding_provider", "entity_seeds", "kinds",
                                            "filter", "as_of_ms", "ranking", "limits", "projection", "mode", "limit",
                                            "allow_partial"}),
        "MemorySlot": ({"subject_id", "predicate", "slot_key"}, set()),
        "MemoryFactsRequest": ({"scope", "slot"}, {"as_of_ms", "history", "limit", "limits", "projection",
                                                   "allow_partial"}),
        "MemoryCorrectionRequest": ({"scope", "idempotency_key", "slot", "series", "slot_revision", "interval", "action"},
                                    {"max_references", "deadline_ms", "require_complete_feed", "at_ms",
                                     "correction_interval", "replacement", "evidence"}),
        "MemoryInput": ({"id", "revision"}, {"disposition"}),
        "MemoryConsolidateRequest": ({"scope", "idempotency_key", "inputs", "output"},
                                     {"max_references", "deadline_ms", "require_complete_feed", "evidence", "dry_run"}),
        "MemoryTargetIds": ({"ids"}, set()),
        "MemoryTargetSeries": ({"series"}, set()),
        "MemoryTargetEntity": ({"entity"}, set()),
        "MemoryTargetSession": ({"session"}, set()),
        "MemoryTargetScope": ({"scope"}, set()),
        "MemoryForgetRequest": ({"scope", "idempotency_key", "target", "mode"},
                                {"max_references", "deadline_ms", "require_complete_feed", "expires_at_ms",
                                 "allow_job"}),
        "MemoryReceiptRequest": ({"scope", "operation", "idempotency_key"}, set()),
        "MemoryErasureRequest": ({"scope", "job_id"}, set()),
        "MemoryResumeRequest": ({"scope", "idempotency_key", "job_id", "revision", "generation", "resume_token"},
                                {"max_references", "deadline_ms", "require_complete_feed"}),
        "MemoryReceipt": ({"id", "operation", "scope", "sequence", "digest_version"}, {"retry_until_ms"}),
        "MemoryOutcome": ({"id", "action", "revision"}, {"expires_at_ms"}),
        "MemoryFeed": ({"mode", "temporal_complete", "ttl_complete", "consumer_purge_acknowledged"}, set()),
        "MemoryErasureNotice": ({"live_state_only", "secure_erasure", "external_copies", "restore_requires_manifest",
                                 "backup_history_purge"}, {"manifest_id"}),
        "MemoryErasure": ({"id", "scope", "target", "revision", "generation", "state", "logical_visibility",
                           "deleted_count", "remaining_count"}, {"resume_token"}),
        "MemoryMutationResponse": ({"receipt", "replayed", "outcomes", "feed", "remaining_work"},
                                   {"state", "erasure", "erasure_notice"}),
        "MemoryReceiptResponse": ({"result"}, set()),
        "MemoryErasureResponse": ({"erasure", "erasure_notice", "feed"}, set()),
        "MemoryErrorDetail": (set(), {"field", "limit", "requested", "competing", "receipt_lookup_required", "code"}),
        "MemoryPlanResponse": ({"dry_run", "user_operations", "encoded_bytes", "conflicts", "remaining_work"}, set()),
        "MemoryEvidenceView": ({"collection", "record"}, {"snippet"}),
        "MemoryPath": ({"nodes", "edges"}, set()),
        "MemoryScores": ({"R", "D", "I", "S"}, {"dense_rank", "sparse_rank", "text_rank", "distance", "bm25",
                                                 "sparse_score", "fused_rank", "fused_score"}),
        "MemoryView": ({"id", "revision", "scope", "kind", "content", "observed_at_ms", "created_at_ms",
                        "updated_at_ms", "importance", "status", "stale", "segment", "evidence", "scores"},
                       {"confidence", "expires_at_ms", "provenance", "references", "paths", "metadata"}),
        "MemoryReadMeta": ({"scopes", "as_of_ms", "ttl_clock_ms", "sequence", "complete", "truncation", "limitations",
                            "mode", "candidates_examined", "edges_charged", "vector_components", "mmr_components",
                            "evidence_bytes"}, set()),
        "MemoryRecallResponse": ({"memories", "meta", "ranking_version", "selection"}, set()),
        "MemoryFactView": ({"series", "slot", "slot_revision", "destination_id", "interval", "status", "stale",
                            "evidence"}, {"provenance"}),
        "MemoryFactsResponse": ({"facts", "meta"}, set()),
        "MemoryCompeting": ({"id", "interval", "revision"}, set()),
        "MemoryErrorContext": (set(), {"field", "limit", "requested", "feature", "category"}),
    }

    def test_typed_dicts_match_section_8_names_and_required_presence(self):
        for name, (required, optional) in self.TYPED.items():
            with self.subTest(type=name):
                cls = getattr(vlite_client, name)
                self.assertEqual((set(cls.__required_keys__), set(cls.__optional_keys__)), (required, optional))
        self.assertEqual(set(vlite_client.MemoryLimits.__annotations__), set(LIMITS))
        # Python's builtin MemoryError must never be shadowed by an envelope type.
        self.assertIs(getattr(vlite_client, "MemoryError", MemoryError), MemoryError)

    def test_integer_typing_separates_input_forms_from_canonical_string_outputs(self):
        def hint(name, field):
            return get_type_hints(getattr(vlite_client, name))[field]

        for name, field in (("MemoryReceipt", "sequence"), ("MemoryReceipt", "retry_until_ms"),
                            ("MemoryOutcome", "revision"), ("MemoryOutcome", "expires_at_ms"),
                            ("MemoryErasure", "revision"), ("MemoryErasure", "generation"),
                            ("MemoryErasure", "deleted_count"), ("MemoryPlanResponse", "user_operations"),
                            ("MemoryPlanResponse", "encoded_bytes"), ("MemoryView", "revision"),
                            ("MemoryView", "observed_at_ms"), ("MemoryView", "created_at_ms"),
                            ("MemoryView", "updated_at_ms"), ("MemoryView", "expires_at_ms"),
                            ("MemoryReadMeta", "as_of_ms"), ("MemoryReadMeta", "ttl_clock_ms"),
                            ("MemoryReadMeta", "sequence"), ("MemoryReadMeta", "candidates_examined"),
                            ("MemoryReadMeta", "edges_charged"), ("MemoryReadMeta", "vector_components"),
                            ("MemoryReadMeta", "mmr_components"), ("MemoryReadMeta", "evidence_bytes"),
                            ("MemoryFactView", "slot_revision"), ("MemoryEvidenceView", "record"),
                            ("MemoryCompeting", "revision"), ("MemoryErrorContext", "limit"),
                            ("MemoryErrorContext", "requested"), ("MemoryErrorDetail", "limit"),
                            ("MemoryErrorDetail", "requested")):
            self.assertIs(hint(name, field), str, (name, field))
        for name, field in (("MemoryMutationResponse", "remaining_work"), ("MemoryErasure", "remaining_count"),
                            ("MemoryPlanResponse", "remaining_work")):
            self.assertEqual(hint(name, field), str | None, (name, field))
        for name, field in (("MemoryRef", "revision"), ("MemoryItem", "observed_at_ms"), ("MemoryItem", "expires_at_ms"),
                            ("MemoryRecallRequest", "as_of_ms"), ("MemoryFactsRequest", "as_of_ms"),
                            ("MemoryCorrectionRequest", "slot_revision"), ("MemoryCorrectionRequest", "at_ms"),
                            ("MemoryInput", "revision"), ("MemoryForgetRequest", "expires_at_ms"),
                            ("MemoryResumeRequest", "revision"), ("MemoryResumeRequest", "generation"),
                            ("MemoryEvidence", "record"), ("MemoryEvidenceRef", "record"),
                            ("MemoryExpectedRevision", "revision"), ("MemoryIntegerScalar", "value")):
            self.assertEqual(hint(name, field), str | int, (name, field))
        for name, field in (("MemoryInterval", "valid_from_ms"), ("MemoryInterval", "valid_to_ms")):
            self.assertEqual(hint(name, field), str | int | None)
        for name, field in (("MemoryMutation", "deadline_ms"), ("MemoryMutation", "max_references"),
                            ("MemoryRecallRequest", "limit"), ("MemoryFactsRequest", "limit"),
                            *(("MemoryLimits", key) for key in LIMITS)):
            self.assertIs(hint(name, field), int, (name, field))
        for name, field in (("MemoryItem", "importance"), ("MemoryItem", "confidence"), ("MemoryRanking", "lambda"),
                            ("MemoryRanking", "half_life_ms"), ("MemoryFloatScalar", "value"),
                            ("MemoryScores", "I"), ("MemoryScores", "S"), ("MemoryScores", "R"),
                            ("MemoryView", "importance")):
            self.assertIs(hint(name, field), float, (name, field))
        self.assertEqual(hint("MemoryScores", "D"), float | None)
        self.assertEqual(hint("MemoryView", "segment"), vlite_client.MemoryInterval | None)
        self.assertEqual(get_args(hint("MemoryRanking", "weights")), (float,))
        self.assertEqual(get_args(hint("MemoryEvidence", "vector")), (float,))

    def test_unions_and_literals_match_section_8(self):
        def literal(name, field):
            return get_args(get_type_hints(getattr(vlite_client, name))[field])

        for name, required in (("MemoryExpected", [{"absent"}, {"revision"}]),
                               ("MemoryTarget", [{"ids"}, {"series"}, {"entity"}, {"session"}, {"scope"}]),
                               ("MemoryScalar", [{"type", "value"}] * 4)):
            self.assertEqual([set(member.__required_keys__) for member in get_args(getattr(vlite_client, name))],
                             required, name)
        self.assertEqual(get_args(vlite_client.MemoryConsolidateResponse),
                         (vlite_client.MemoryMutationResponse, vlite_client.MemoryPlanResponse))
        for name, field in (("MemoryExpectedAbsent", "absent"), ("MemoryTargetSession", "session"),
                            ("MemoryTargetScope", "scope"), ("MemoryPlanResponse", "dry_run")):
            self.assertEqual(literal(name, field), (True,), (name, field))
        for field in ("temporal_complete", "ttl_complete", "consumer_purge_acknowledged"):
            self.assertEqual(literal("MemoryFeed", field), (False,), field)
        self.assertEqual(literal("MemoryErasureNotice", "secure_erasure"), (False,))
        for name, field in (("MemoryErasureNotice", "live_state_only"), ("MemoryErasureNotice", "restore_requires_manifest")):
            self.assertEqual(literal(name, field), (True,), (name, field))
        tags = {get_args(get_type_hints(member)["type"])[0]: get_type_hints(member)["value"]
                for member in get_args(vlite_client.MemoryScalar)}
        self.assertEqual(tags, {"string": str, "integer": str | int, "float": float, "boolean": bool})
        operations = ("remember", "correct_facts", "consolidate", "forget", "resume_erasure")
        actions = ("created", "merged", "noop", "corrected", "deleted", "expired", "retained_shared")
        for name, field, expected in (
                ("MemoryItem", "kind", ("entity", "episodic", "fact", "relation")),
                ("MemoryItem", "merge", ("none", "merge_provenance")),
                ("MemoryAssertion", "disagreement", ("reject", "disputed")),
                ("MemoryCondition", "op", ("eq", "lt", "lte", "gt", "gte", "exists")),
                ("MemoryRanking", "selection", ("weighted", "exact", "mmr")),
                ("MemoryRanking", "future_observations", ("reject", "clamp")),
                ("MemoryRecallRequest", "mode", ("exact", "approximate")),
                ("MemoryCorrectionRequest", "action", ("close", "replace", "remove")),
                ("MemoryInput", "disposition", ("retain", "logical_delete")),
                ("MemoryForgetRequest", "mode", ("logical_delete", "expire")),
                ("MemoryReceiptRequest", "operation", operations), ("MemoryReceipt", "operation", operations),
                ("MemoryReceipt", "digest_version", ("memory-request-v1",)),
                ("MemoryOutcome", "action", actions),
                ("MemoryMutationResponse", "state", ("logical", "pending", "retention_set")),
                ("MemoryErasure", "state", ("pending", "logical")),
                ("MemoryErasure", "logical_visibility", ("hidden",)),
                ("MemoryErasureNotice", "external_copies", ("operator_managed",)),
                ("MemoryErasureNotice", "backup_history_purge", ("not_verified",)),
                ("MemoryFeed", "mode", ("ordinary_changes",)),
                ("MemoryView", "status", ("accepted", "disputed")), ("MemoryFactView", "status", ("accepted", "disputed")),
                ("MemoryView", "kind", ("entity", "episodic", "fact", "relation")),
                ("MemoryReadMeta", "mode", ("exact_enumeration", "approximate", "bounded_expansion")),
                ("MemoryRecallResponse", "ranking_version", ("memory-ranking-v1",)),
                ("MemoryRecallResponse", "selection", ("weighted", "exact", "mmr"))):
            self.assertEqual(literal(name, field), expected, (name, field))
        self.assertEqual(get_args(vlite_client.MemoryKind), ("entity", "episodic", "fact", "relation"))
        self.assertEqual(get_args(vlite_client.MemoryOperation), operations)
        meta = get_type_hints(vlite_client.MemoryReadMeta)
        self.assertEqual(get_args(get_args(meta["truncation"])[0]),
                         ("candidates", "source_scan", "vector_components", "evidence_bytes", "mmr_components",
                          "visited_nodes", "edges", "degree", "deadline", "output", "deletion_barrier"))
        self.assertEqual(get_args(get_args(meta["limitations"])[0]),
                         ("historical_candidate_discovery_not_exhaustive", "retained_state_only"))

    def test_the_facade_exposes_exactly_the_nine_synchronous_methods_with_full_typed_results(self):
        import inspect
        self.assertEqual({name for name in dir(vlite_client.Memory) if not name.startswith("_")}, set(METHODS))
        signatures = {"remember": ("MemoryRememberRequest", "MemoryMutationResponse"),
                      "recall": ("MemoryRecallRequest", "MemoryRecallResponse"),
                      "find_facts": ("MemoryFactsRequest", "MemoryFactsResponse"),
                      "correct_facts": ("MemoryCorrectionRequest", "MemoryMutationResponse"),
                      "consolidate": ("MemoryConsolidateRequest", "MemoryConsolidateResponse"),
                      "forget": ("MemoryForgetRequest", "MemoryMutationResponse"),
                      "get_receipt": ("MemoryReceiptRequest", "MemoryReceiptResponse"),
                      "get_erasure": ("MemoryErasureRequest", "MemoryErasureResponse"),
                      "resume_erasure": ("MemoryResumeRequest", "MemoryMutationResponse")}
        self.assertEqual(set(signatures), set(METHODS))
        for method, (request, response) in signatures.items():
            function = getattr(vlite_client.Memory, method)
            self.assertFalse(inspect.iscoroutinefunction(function), method)
            self.assertEqual(get_type_hints(function), {"request": getattr(vlite_client, request),
                                                        "return": getattr(vlite_client, response)}, method)
            self.assertEqual(list(inspect.signature(function).parameters), ["self", "request"], method)
        self.assertIs(get_type_hints(Client.memory)["return"], vlite_client.Memory)
        self.assertEqual(list(inspect.signature(Client.memory).parameters), ["self", "binding"])


if __name__ == "__main__":
    unittest.main()
