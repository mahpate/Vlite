import assert from "node:assert/strict";
import { createServer } from "node:http";
import { test } from "node:test";
import { Client, TransportError, UnsupportedError, VLiteError } from "./index.js";

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

test("versioned reads and conditional writes", async () => {
  const maxId = "18446744073709551615";
  const tag = '"vlite-1:Zm9v_bar-:18446744073709551615:42"';
  const record = { id: maxId, vector: [1], payload_base64: "", metadata: {} };
  const caps = { version: "0.1.2", api_version: 1, role: "admin", features: { conditional_writes: true }, routes: [] };
  const cap = { method: "GET", path: "/v1/capabilities", ifMatch: undefined };
  let capability = caps;
  const calls = [];
  let mode = "ok";
  let etags = [tag];
  const server = createServer(async (req, res) => {
    for await (const _ of req);
    calls.push({ method: req.method, path: req.url, ifMatch: req.headers["if-match"] });
    if (req.url === "/v1/capabilities") {
      const [status, body] = capability === 404 ? [404, '{"error":{"code":"not_found","message":"No route"}}']
        : capability === 401 ? [401, '{"error":{"code":"unauthorized","message":"Bad token"}}']
          : capability === "bad-json" ? [200, "not json"] : [200, JSON.stringify(capability)];
      res.writeHead(status, { "X-Request-ID": "test-caps" }).end(body);
    } else if (mode === "versioned") {
      res.setHeader("ETag", etags);
      res.end(JSON.stringify(record));
    } else if (mode === "precondition") {
      res.writeHead(412, { "X-Request-ID": "test-412" });
      res.end(JSON.stringify({ error: { code: "precondition_failed", message: "Record changed" } }));
    } else if (req.method === "DELETE") {
      res.end(JSON.stringify({ deleted: true }));
    } else {
      res.writeHead(204).end();
    }
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const client = new Client(`http://127.0.0.1:${server.address().port}`, token);
  try {
    mode = "versioned";
    assert.deepEqual(await client.getVersioned(maxId), { record, etag: tag });
    assert.deepEqual(await client.get(maxId), record);
    assert.deepEqual(calls.splice(0), [
      { method: "GET", path: `/v1/vectors/${maxId}`, ifMatch: undefined },
      { method: "GET", path: `/v1/vectors/${maxId}`, ifMatch: undefined },
    ]);
    for (const bad of [[], ['W/"abc"'], ["*"], ['"a", "b"'], ['"a"', '"b"'], ["abc"], ['"a b"']]) {
      etags = bad;
      await assert.rejects(client.getVersioned("1"), TransportError);
    }
    assert.equal(calls.splice(0).length, 7);

    mode = "ok";
    assert.equal(await client.put(maxId, { vector: [1] }, { ifMatch: tag }), undefined);
    assert.equal(await client.delete(maxId, { ifMatch: tag }), true);
    assert.equal(await client.put("1", { vector: [1] }), undefined);
    assert.equal(await client.delete("1"), true);
    assert.equal(await client.put("1", { vector: [1] }, {}), undefined);
    assert.deepEqual(calls.splice(0), [
      cap, { method: "PUT", path: `/v1/vectors/${maxId}`, ifMatch: tag },
      cap, { method: "DELETE", path: `/v1/vectors/${maxId}`, ifMatch: tag },
      { method: "PUT", path: "/v1/vectors/1", ifMatch: undefined },
      { method: "DELETE", path: "/v1/vectors/1", ifMatch: undefined },
      { method: "PUT", path: "/v1/vectors/1", ifMatch: undefined },
    ]);

    mode = "precondition";
    for (const call of [() => client.put("1", { vector: [1] }, { ifMatch: tag }), () => client.delete("1", { ifMatch: tag })]) {
      await assert.rejects(call(), (e) => e instanceof VLiteError && e.status === 412
        && e.code === "precondition_failed" && e.requestId === "test-412");
    }
    assert.deepEqual(calls.splice(0).map((c) => c.method), ["GET", "PUT", "GET", "DELETE"]);

    const bad = ["", "abc", "*", 'W/"abc"', '"a", "b"', '"a","b"', '"a b"', '"a\tb"', '"a\x7fb"', '"a\nb"',
      ' "a"', '"a" ', '"\u00e9"', '"a"b"', undefined, null, 1, true, ["\"a\""]];
    for (const value of bad) {
      await assert.rejects(client.put("1", { vector: [1] }, { ifMatch: value }), TypeError);
      await assert.rejects(client.delete("1", { ifMatch: value }), TypeError);
    }
    for (const options of [null, "x", tag, { ifmatch: tag }, { ifMatch: tag, extra: 1 }]) {
      await assert.rejects(client.put("1", { vector: [1] }, options), TypeError);
      await assert.rejects(client.delete("1", options), TypeError);
    }
    assert.throws(() => client.getVersioned("18446744073709551616"), TypeError);
    assert.equal(calls.length, 0);

    // Older servers ignore If-Match: refuse rather than silently write unconditionally.
    const unsupported = [404, { api_version: 1, features: {} }, { api_version: 1 },
      { api_version: 1, features: { conditional_writes: "true" } }, { api_version: 1, features: { conditional_writes: 1 } },
      { api_version: 2, features: { conditional_writes: true } }, { api_version: "1", features: { conditional_writes: true } },
      { api_version: 1, features: null }, [], null];
    for (const value of unsupported) {
      capability = value;
      await assert.rejects(client.put("1", { vector: [1] }, { ifMatch: tag }), UnsupportedError);
      await assert.rejects(client.delete("1", { ifMatch: tag }), UnsupportedError);
    }
    assert.deepEqual(calls.splice(0), Array(2 * unsupported.length).fill(cap));
    capability = 401;
    await assert.rejects(client.delete("1", { ifMatch: tag }), (e) => e instanceof VLiteError
      && e.status === 401 && e.code === "unauthorized" && e.requestId === "test-caps");
    capability = "bad-json";
    await assert.rejects(client.put("1", { vector: [1] }, { ifMatch: tag }), TransportError);
    assert.deepEqual(calls.splice(0), [cap, cap]);
    // Checked before every conditional write; a positive answer is never reused.
    capability = caps;
    mode = "ok";
    await client.put("1", { vector: [1] }, { ifMatch: tag });
    capability = 404;
    await assert.rejects(client.put("1", { vector: [1] }, { ifMatch: tag }), UnsupportedError);
    assert.deepEqual(calls.splice(0), [cap, { method: "PUT", path: "/v1/vectors/1", ifMatch: tag }, cap]);
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  }
});

test("filtered search sends the request unchanged and propagates rejections without fallback", async () => {
  const calls = [];
  let reject = false;
  const server = createServer(async (req, res) => {
    let body = "";
    for await (const chunk of req) body += chunk;
    calls.push({ method: req.method, path: req.url, body: JSON.parse(body) });
    if (reject) {
      res.writeHead(422, { "X-Request-ID": "test-422" });
      res.end(JSON.stringify({ error: { code: "invalid_input", message: "filters require mode exact; approximate filtering is not supported" } }));
    } else {
      res.end(JSON.stringify({ hits: [{ id: "7", distance: 0.5 }] }));
    }
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const client = new Client(`http://127.0.0.1:${server.address().port}`, token);
  const filter = [
    { op: "eq", field: "tag", value: { type: "string", value: "a" } },
    { op: "range", field: "n", min: { type: "integer", value: "-9223372036854775808" }, max: null },
  ];
  try {
    const requests = ["exact", "approximate", "approximate_ready"].flatMap((mode) => ["squared_l2", "cosine"].map(
      (metric) => ({ vector: [1, 0], k: 3, mode, metric, filter, ...(mode === "exact" ? {} : { ef_search: 50 }) })));
    for (const request of requests) assert.deepEqual(await client.search(request), [{ id: "7", distance: 0.5 }]);
    assert.deepEqual(calls.splice(0), requests.map((body) => ({ method: "POST", path: "/v1/search", body })));
    // Requests without a filter are unchanged by the filtered-ANN contract.
    await client.search({ vector: [1, 0], k: 1, mode: "approximate" });
    assert.deepEqual(calls.splice(0)[0].body, { vector: [1, 0], k: 1, mode: "approximate" });
    // An older server's explicit rejection is surfaced once: no retry, no exact fallback.
    reject = true;
    for (const mode of ["approximate", "approximate_ready"]) {
      await assert.rejects(client.search({ vector: [1, 0], k: 1, mode, filter }), (e) => e instanceof VLiteError
        && e.status === 422 && e.code === "invalid_input" && e.requestId === "test-422");
    }
    assert.deepEqual(calls.splice(0).map((c) => [c.body.mode, c.body.filter]), [["approximate", filter], ["approximate_ready", filter]]);
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  }
});