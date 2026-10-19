import assert from "node:assert/strict";
import { createServer } from "node:http";
import { test } from "node:test";
import { Client, TransportError, VLiteError } from "./index.js";

const token = "test_" + "a".repeat(32);
test("HTTP contract, redirects, validation and no retries", async () => {
  const calls = [];
  let mode = "ok";
  const server = createServer(async (req, res) => {
    let body = "";
    for await (const chunk of req) body += chunk;
    calls.push({ path: req.url, auth: req.headers.authorization, body });
    if (mode === "redirect") {
      res.writeHead(302, { Location: "/stolen" }).end();
    } else if (mode === "error") {
      res.writeHead(409, { "X-Request-ID": "test-42" });
      res.end(JSON.stringify({ error: { code: "index_not_ready", message: "Build first" } }));
    } else if (mode === "bad-json") {
      res.end("not json");
    } else if (mode === "timeout") {
      // No response; client must abort without retrying.
    } else {
      res.end(JSON.stringify({ items: [], next_after: null }));
    }
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const base = `http://127.0.0.1:${server.address().port}`;
  try {
    const client = new Client(base, token);
    assert.deepEqual(await client.listVectors({ after: "18446744073709551615" }), { items: [], next_after: null });
    assert.equal(calls[0].path, "/v1/vectors?limit=25&after=18446744073709551615");
    assert.equal(calls[0].auth, `Bearer ${token}`);
    assert.equal(JSON.stringify(client).includes(token), false);
    for (const value of [1, "", "01", "-1", "18446744073709551616", "1/health"]) {
      assert.throws(() => client.get(value), TypeError);
    }
    assert.throws(() => client.listVectors({ limit: 101 }), TypeError);
    await assert.rejects(client.put("1", { vector: [NaN] }), TypeError);
    assert.equal(calls.length, 1);
    mode = "error";
    await assert.rejects(client.indexStatus(), (e) =>
      e instanceof VLiteError && e.status === 409 && e.code === "index_not_ready" && e.requestId === "test-42");
    mode = "redirect";
    await assert.rejects(client.database(), TransportError);
    assert.equal(calls.length, 3);
    mode = "bad-json";
    await assert.rejects(client.database(), TransportError);
    mode = "timeout";
    await assert.rejects(new Client(base, token, { timeoutMs: 100 }).put("1", { vector: [1] }), TransportError);
    assert.equal(calls.length, 5);
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  }
});
