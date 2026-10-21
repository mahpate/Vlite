import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from threading import Thread
import time
import unittest

from vlite_client import Client, TransportError, UnsupportedError, VLiteError

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
            time.sleep(0.2)
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
        with self.assertRaises(TransportError):
            Client(self.base, TOKEN, timeout=0.05).put("1", {"vector": [1]})
        self.assertEqual(len(Handler.calls), 1)

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


if __name__ == "__main__":
    unittest.main()
