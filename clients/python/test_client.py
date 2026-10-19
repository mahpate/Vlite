import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from threading import Thread
import time
import unittest

from vlite_client import Client, TransportError, VLiteError

TOKEN = "test_" + "a" * 32


class Handler(BaseHTTPRequestHandler):
    calls = []
    mode = "ok"

    def log_message(self, *_):
        pass

    def do_GET(self):
        self.calls.append((self.path, self.headers.get("Authorization")))
        if self.mode == "redirect":
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
        Handler.mode = "ok"
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


if __name__ == "__main__":
    unittest.main()
