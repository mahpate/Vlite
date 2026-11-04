import assert from "node:assert/strict";
import { createServer } from "node:http";
import { test } from "node:test";
import { Client, ImportRejectedError, TransportError, UnsupportedError, VLiteError } from "./index.js";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const token = "test_" + "a".repeat(32);
test("HTTP contract, redirects, validation and no retries", async (t) => {
  const calls = [];
  const timeoutController = new AbortController();
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
      // Abort only after admission, not before a slow runner can send the request.
      timeoutController.abort(new DOMException("Timed out", "TimeoutError"));
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
    t.mock.method(AbortSignal, "timeout", (milliseconds) => {
      assert.equal(milliseconds, 100);
      return timeoutController.signal;
    });
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
// Scripted fake server: each request consumes the next queued reply; unscripted requests fail the test.
async function scripted(run) {
  const calls = [];
  const replies = [];
  const server = createServer(async (req, res) => {
    let body = "";
    for await (const chunk of req) body += chunk;
    calls.push({ method: req.method, path: req.url, body: body ? JSON.parse(body) : undefined,
      accept: req.headers.accept, ifMatch: req.headers["if-match"], auth: req.headers.authorization,
      contentType: req.headers["content-type"] });
    const reply = replies.shift() ?? { status: 599, body: { error: { code: "unscripted", message: req.url } } };
    const headers = { "Content-Type": reply.type ?? "application/json", ...(reply.headers ?? {}) };
    res.writeHead(reply.status ?? 200, headers);
    res.end(reply.text ?? (reply.body === undefined ? "" : JSON.stringify(reply.body)));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const client = new Client(`http://127.0.0.1:${server.address().port}`, token);
  const reply = (...items) => replies.push(...items);
  try { await run({ client, calls, reply }); } finally { server.close(); }
}
const err = (status, code) => ({ status, body: { error: { code, message: code } } });

test("KV exact wire, Expected variants and lossless results", async () => {
  await scripted(async ({ client, calls, reply }) => {
    const entry = { key: "雪 /+?%", value_base64: "AP8=", version: "18446744073709551615", expires_at_ms: "0", future: true };
    reply({ body: entry });
    assert.deepEqual(await client.kvGet({ collection: "cache", namespace: "名 +", key: "雪 /+?%" }),
      { key: "雪 /+?%", value: new Uint8Array([0, 255]), version: 18446744073709551615n, expires_at_ms: 0n });
    assert.equal(calls.at(-1).path, "/v1/collections/cache/kv/entry?namespace=%E5%90%8D%20%2B&key=%E9%9B%AA%20%2F%2B%3F%25");
    const cases = [["any", "any"], ["absent", "absent"], [{ revision: 0n }, { revision: "0" }],
      [{ revision: 9007199254740993n }, { revision: "9007199254740993" }],
      [{ revision: 18446744073709551615n }, { revision: "18446744073709551615" }]];
    for (const [expected, wire] of cases) {
      reply({ body: { version: "9007199254740993", expires_at_ms: "18446744073709551615" } });
      assert.deepEqual(await client.kvPut({ collection: "cache", namespace: "n", key: "k", value: new Uint8Array([255]),
        expected, expiresAtMs: 18446744073709551615n }), { version: 9007199254740993n, expires_at_ms: 18446744073709551615n });
      assert.equal(calls.at(-1).method, "PUT");
      assert.equal(calls.at(-1).path, "/v1/collections/cache/kv/entry");
      assert.deepEqual(calls.at(-1).body, { namespace: "n", key: "k", value_base64: "/w==", expected: wire, expires_at_ms: "18446744073709551615" });
      for (const deleted of [true, false]) {
        reply({ body: { deleted } });
        assert.equal(await client.kvDelete({ collection: "cache", namespace: "名 +", key: "雪 /+?%", expected }), deleted);
        assert.equal(calls.at(-1).method, "DELETE");
        assert.equal(calls.at(-1).path, "/v1/collections/cache/kv/entry?namespace=%E5%90%8D%20%2B&key=%E9%9B%AA%20%2F%2B%3F%25&expected="
          + (typeof wire === "string" ? wire : wire.revision));
        assert.equal(calls.at(-1).body, undefined);
      }
    }
    reply({ body: { items: [entry], next_after: "雪 /+?%" } }, { body: { items: [], next_after: null } });
    assert.deepEqual(await client.kvList({ collection: "cache", namespace: "名 +", prefix: "雪 ", after: "a/+", limit: 10000 }),
      { items: [{ key: "雪 /+?%", value: new Uint8Array([0, 255]), version: 18446744073709551615n, expires_at_ms: 0n }], next_after: "雪 /+?%" });
    assert.equal(calls.at(-1).path, "/v1/collections/cache/kv/entries?namespace=%E5%90%8D%20%2B&prefix=%E9%9B%AA%20&after=a%2F%2B&limit=10000");
    assert.deepEqual(await client.kvList({ collection: "cache", namespace: "n" }), { items: [], next_after: null });
    assert.equal(calls.at(-1).path, "/v1/collections/cache/kv/entries?namespace=n&prefix=&limit=100");
    for (const call of calls) {
      assert.equal(call.auth, `Bearer ${token}`);
      assert.equal(call.accept, "application/json");
      assert.equal(call.contentType, call.body === undefined ? undefined : "application/json");
    }
  });
});

test("KV value bounds and null expiry", async () => {
  await scripted(async ({ client, calls, reply }) => {
    for (const [value, encoded] of [[new Uint8Array(), ""], [new Uint8Array(1047290), "A".repeat(1396387) + "="]]) {
      reply({ body: { version: "1", expires_at_ms: null } });
      assert.deepEqual(await client.kvPut({ collection: "cache", namespace: "n", key: "k", value }),
        { version: 1n, expires_at_ms: null });
      assert.deepEqual(calls.at(-1).body, { namespace: "n", key: "k", value_base64: encoded, expected: "any", expires_at_ms: null });
    }
    await assert.rejects(async () => client.kvPut({ collection: "cache", namespace: "n", key: "k", value: new Uint8Array(1047291) }), TypeError);
    assert.equal(calls.length, 2);
  });
});

test("KV maximum entry and required response shapes", async () => {
  await scripted(async ({ client, reply }) => {
    const address = { collection: "cache", namespace: "n", key: "k" };
    reply({ body: { key: "k", value_base64: "A".repeat(1396387) + "=", version: "1", expires_at_ms: null } });
    assert.deepEqual((await client.kvGet(address)).value, new Uint8Array(1047290));
    for (const [body, call] of [
      [{ version: "1" }, () => client.kvPut({ ...address, value: new Uint8Array() })],
      [{ deleted: 1 }, () => client.kvDelete(address)],
      [{ items: [], next_after: 0 }, () => client.kvList(address)],
      [{ items: {} }, () => client.kvList(address)],
      [null, () => client.kvGet(address)]]) {
      reply({ body });
      await assert.rejects(call, TransportError);
    }
  });
});

test("KV stable errors and exact unsupported route mapping without retries", async () => {
  await scripted(async ({ client, calls, reply }) => {
    for (const [status, code] of [[404, "kv_not_found"], [412, "precondition_failed"], [409, "not_kv_collection"],
      [409, "kv_slots_exhausted"], [422, "invalid_input"], [404, "collection_not_found"], [503, "commit_indeterminate"], [404, "not_found"],
      [409, "not_found"]]) {
      for (const call of [() => client.kvGet({ collection: "cache", namespace: "n", key: "k" }),
        () => client.kvPut({ collection: "cache", namespace: "n", key: "k", value: new Uint8Array() }),
        () => client.kvDelete({ collection: "cache", namespace: "n", key: "k" }),
        () => client.kvList({ collection: "cache", namespace: "n" })]) {
        const before = calls.length;
        reply({ ...err(status, code), headers: { "X-Request-ID": "kv-error" } });
        await assert.rejects(async () => call(), (e) => status === 404 && code === "not_found" ? e instanceof UnsupportedError
          : e instanceof VLiteError && e.status === status && e.code === code && e.requestId === "kv-error");
        assert.equal(calls.length, before + 1);
      }
    }
  });
});

test("KV invalid inputs and noncanonical responses", async () => {
  await scripted(async ({ client, calls, reply }) => {
    const address = { collection: "cache", namespace: "n", key: "k" };
    for (const patch of [{ namespace: "" }, { namespace: "é".repeat(128) }, { key: "" }, { key: "é".repeat(513) },
      { key: "\ud800" }, { collection: "" }]) {
      await assert.rejects(async () => client.kvGet({ ...address, ...patch }), TypeError);
    }
    for (const expected of [null, { revision: 0 }, { revision: -1n }, { revision: 18446744073709551616n },
      { revision: "1" }, { revision: 1n, extra: true }, "wrong"]) {
      await assert.rejects(async () => client.kvDelete({ ...address, expected }), TypeError);
    }
    for (const patch of [{ limit: 0 }, { limit: 10001 }, { limit: true }, { prefix: "é".repeat(513) }, { after: "" }]) {
      await assert.rejects(async () => client.kvList({ ...address, ...patch }), TypeError);
    }
    for (const expiresAtMs of [0, -1n, 18446744073709551616n, "1"]) {
      await assert.rejects(async () => client.kvPut({ ...address, value: new Uint8Array(), expiresAtMs }), TypeError);
    }
    await assert.rejects(async () => client.kvPut({ ...address, value: "not bytes" }), TypeError);
    assert.equal(calls.length, 0);
    const valid = { key: "k", value_base64: "", version: "1", expires_at_ms: null };
    for (const patch of [{ version: "0" }, { version: 1 }, { version: "01" }, { version: "18446744073709551616" },
      { expires_at_ms: 0 }, { expires_at_ms: "01" }, { value_base64: "/x==" }, { value_base64: "/w" },
      { value_base64: "_w==" }, { value_base64: " /w==" }]) {
      reply({ body: { ...valid, ...patch } });
      await assert.rejects(async () => client.kvGet(address), TransportError);
    }
  });
});

test("KV text boundaries, empty entry and zero deadline", async () => {
  await scripted(async ({ client, calls, reply }) => {
    reply({ body: { key: "k", value_base64: "", version: "1", expires_at_ms: null } },
      { body: { items: [], next_after: null } }, { body: { version: "1", expires_at_ms: "0" } });
    assert.deepEqual((await client.kvGet({ collection: "cache", namespace: "n".repeat(255), key: "é".repeat(512) })).value, new Uint8Array());
    await client.kvList({ collection: "cache", namespace: "n", prefix: "é".repeat(512), after: "é".repeat(512), limit: 1 });
    assert.deepEqual(await client.kvPut({ collection: "cache", namespace: "n", key: "k", value: new Uint8Array(), expiresAtMs: 0n }),
      { version: 1n, expires_at_ms: 0n });
    assert.equal(calls.at(-1).body.expires_at_ms, "0");
    assert.equal(calls.length, 3);
  });
});

test("KV timeout still applies with a caller signal and never retries", async (t) => {
  const timeout = new AbortController();
  const caller = new AbortController();
  let calls = 0;
  const server = createServer(async (req, res) => {
    for await (const _ of req);
    calls++;
    timeout.abort(new DOMException("Timed out", "TimeoutError"));
    res.end('{"version":"1","expires_at_ms":null}');
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.mock.method(AbortSignal, "timeout", (milliseconds) => {
    assert.equal(milliseconds, 100);
    return timeout.signal;
  });
  try {
    const client = new Client(`http://127.0.0.1:${server.address().port}`, token, { timeoutMs: 100 });
    await assert.rejects(client.kvPut({ collection: "cache", namespace: "n", key: "k",
      value: new Uint8Array(), signal: caller.signal }), TransportError);
    assert.equal(calls, 1);
    assert.equal(caller.signal.aborted, false);
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  }
});

test("KV cancellation after admission never retries a mutation", async () => {
  const controller = new AbortController();
  let calls = 0;
  const server = createServer(async (req, res) => {
    for await (const _ of req);
    calls++;
    controller.abort();
    res.end('{"version":"1","expires_at_ms":null}');
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  try {
    const client = new Client(`http://127.0.0.1:${server.address().port}`, token);
    await assert.rejects(async () => client.kvPut({ collection: "cache", namespace: "n", key: "k",
      value: new Uint8Array(), signal: controller.signal }), TransportError);
    assert.equal(calls, 1);
    await assert.rejects(async () => client.kvDelete({ collection: "cache", namespace: "n", key: "k",
      signal: controller.signal }), TransportError);
    assert.equal(calls, 1);
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  }
});

test("collection-scoped routes and legacy defaults", async () => {
  await scripted(async ({ client, calls, reply }) => {
    const docs = client.collection("docs");
    assert.equal(docs.name, "docs");
    reply({ body: { id: "1", revision: "7" }, headers: { ETag: '"r2-a-1-1-7-b"' } }, { body: { api_version: 1, features: { conditional_writes: true } } },
      { status: 204 }, { body: { deleted: true } }, { body: { items: [], next_after: null } }, { status: 204 },
      { body: { hits: [{ id: "1", distance: 0 }] } }, { status: 204 }, { status: 204 }, { status: 204 }, { body: { ready: false } },
      { body: { recall_at_k: 1, samples_used: 4, distance_evaluations: 9, skipped_reason: null } },
      { body: {} }, { body: {} }, { body: {} }, { body: {} }, { status: 204 });
    const { record, etag } = await docs.getVersioned("1");
    assert.equal(record.revision, "7");
    await docs.put("1", { vector: [1] }, { ifMatch: etag });
    assert.equal(await docs.delete("1"), true);
    await docs.listVectors({ limit: 5, after: "3" });
    await docs.batch([{ op: "delete", id: "2" }]);
    assert.deepEqual(await docs.search({ vector: [1], k: 1 }), [{ id: "1", distance: 0 }]);
    await docs.buildIndex();
    await docs.buildIndex({ quantization: "scalar_u8", rerankMultiplier: 4 });
    await client.buildIndex();
    await docs.indexStatus();
    await docs.annQuality({ samples: 4 });
    await docs.rebuildStatus();
    await docs.startRebuild({ quantization: "scalar_u8" });
    await docs.pollRebuild();
    await docs.cancelRebuild();
    await client.buildIndex({ metric: "cosine", quantization: "none" });
    const p = "/v1/collections/docs";
    assert.deepEqual(calls.map((c) => `${c.method} ${c.path}`), [
      `GET ${p}/vectors/1`, "GET /v1/capabilities", `PUT ${p}/vectors/1`, `DELETE ${p}/vectors/1`,
      `GET ${p}/vectors?limit=5&after=3`, `POST ${p}/batches`, `POST ${p}/search`, `POST ${p}/index/hnsw`,
      `POST ${p}/index/hnsw`, "POST /v1/index/hnsw", `GET ${p}/index/hnsw`, `POST ${p}/index/hnsw/quality`,
      `GET ${p}/index/hnsw/rebuild`, `POST ${p}/index/hnsw/rebuild`, `POST ${p}/index/hnsw/rebuild/poll`,
      `POST ${p}/index/hnsw/rebuild/cancel`, "POST /v1/index/hnsw",
    ]);
    assert.equal(calls[2].ifMatch, '"r2-a-1-1-7-b"');
    assert.deepEqual(calls[7].body, {});
    assert.deepEqual(calls[8].body, { quantization: "scalar_u8", rerank_multiplier: 4 });
    assert.deepEqual(calls[9].body, {});
    assert.deepEqual(calls[11].body, { samples: 4 });
    assert.equal(calls[14].body, undefined);
    assert.deepEqual(calls[16].body, { metric: "cosine", quantization: "none" });
  });
});

test("collection management and invalid arguments send nothing", async () => {
  await scripted(async ({ client, calls, reply }) => {
    reply({ body: { items: [], format_version: 4 } }, { status: 201, body: { id: "2", name: "docs" } },
      { body: { name: "docs", text_index: {} } }, { body: { collection: {}, persisted: true } }, { body: { deleted: true } });
    await client.listCollections();
    await client.createCollection("docs", 768, { metric: "cosine" });
    await client.getCollection("docs");
    assert.equal((await client.setCollectionSettings("docs", { text_fields: ["t"] })).persisted, true);
    assert.equal(await client.dropCollection("docs"), true);
    assert.deepEqual(calls.map((c) => c.body), [undefined, { name: "docs", dimensions: 768, settings: { metric: "cosine" } },
      undefined, { text_fields: ["t"] }, undefined]);
    assert.equal(calls[3].path, "/v1/collections/docs/settings");
    assert.equal(calls[4].method, "DELETE");
    const bad = [
      () => client.collection("Docs"), () => client.collection("../x"), () => client.collection("a/b"),
      () => client.createCollection("docs", 0), () => client.createCollection("docs", 3, []),
      () => client.importChunk("1", "01", []), () => client.importChunk("x", "1", []),
      () => client.importChunk("1", "1", [], { policy: "ignore" }), () => client.rotateCredential("../admin"),
      () => client.rotateCredential("abcdefghijk2", { graceSeconds: 86401 }), () => client.revokeCredential("ABCDEFGHIJK2"),
      () => client.persistentAudit({ limit: 0 }), () => client.persistentAudit({ limit: 1001 }),
      () => client.verifyBackup("../etc"), () => client.drillBackup("a/b"), () => client.verifyBackup("a..b"),
      () => client.collection("docs").buildIndex({ rerankMultiplier: 17 }),
      () => client.collection("docs").search({ vector: [Infinity], k: 1 }),
    ];
    for (const call of bad) await assert.rejects(async () => call(), TypeError);
    assert.equal(calls.length, 5);
  });
});

test("search modes, explain and one-shot error propagation without fallback", async () => {
  await scripted(async ({ client, calls, reply }) => {
    const plan = { strategy: "exact_indexed", estimated_candidates: 3, live_records: 9, index_field: "c", reason: "selective" };
    reply({ body: { hits: [{ id: "1", distance: 0.5 }], plan, stats: { records_scanned: 3 } } },
      { body: { hits: [{ id: "4", score: 0.03, distance: null, bm25: 1.2, vector_rank: null, text_rank: 1 }] } });
    const docs = client.collection("docs");
    const detailed = await docs.searchDetailed({ vector: [1], k: 1, mode: "auto", explain: true });
    assert.equal(detailed.plan.strategy, "exact_indexed");
    const hybrid = { mode: "hybrid", text: "apple", k: 2, fusion: { rrf_k: 60, vector_weight: 1, text_weight: 2, candidates: 50 },
      vector_leg: "approximate_ready", quantization: "scalar_u8", rerank_multiplier: 2 };
    assert.equal((await docs.search(hybrid))[0].bm25, 1.2);
    assert.deepEqual(calls[1].body, hybrid);
    for (const [status, code] of [[409, "index_not_ready"], [422, "invalid_input"], [403, "forbidden_scope"],
      [401, "credential_expired"], [404, "collection_not_found"], [409, "requires_format_v4"]]) {
      const before = calls.length;
      reply(err(status, code));
      await assert.rejects(docs.search({ vector: [1], k: 1, mode: "approximate_ready" }),
        (e) => e instanceof VLiteError && e.status === status && e.code === code);
      assert.equal(calls.length, before + 1, "no retry or exact fallback");
      assert.equal(calls.at(-1).body.mode, "approximate_ready");
    }
  });
});

test("imports: explicit replay, rejected chunk and resumable NDJSON", async () => {
  await scripted(async ({ client, calls, reply }) => {
    const open = { job: "9", collection: "docs", state: "open", last_chunk: 0, rows_committed: 0, rows_skipped: 0, durable: false };
    const report = (outcome, chunk) => ({ body: { outcome, job: "9", chunk, rows_committed: 1, rows_skipped: 0, invalid_rows: 0, invalid: [], revision: "3" } });
    reply({ status: 201, body: open }, report("committed", 1), report("already_committed", 1),
      { status: 422, body: { error: { code: "import_rows_invalid", message: "bad" }, job: "9", chunk: 2, rows_committed: 0,
        rows_skipped: 0, invalid_rows: 1, invalid: [{ row: 0, code: "dimension_mismatch", message: "m" }], revision: null } },
      err(409, "import_conflict"), { body: { ...open, state: "finished" } }, { body: { ...open, state: "aborted" } },
      { body: { ...open, durable: true, last_digest: "0".repeat(64), collection_dropped: false } });
    assert.equal((await client.beginImport("docs")).job, "9");
    assert.equal((await client.importChunk("9", "1", [{ op: "delete", id: "1" }], { policy: "skip_invalid" })).outcome, "committed");
    assert.equal((await client.importChunk("9", "1", [{ op: "delete", id: "1" }])).outcome, "already_committed");
    await assert.rejects(client.importChunk("9", "2", []), (e) => e instanceof ImportRejectedError && e.status === 422
      && e.report.outcome === "rejected" && e.report.invalid[0].code === "dimension_mismatch" && !("error" in e.report));
    await assert.rejects(client.importChunk("9", "2", []), (e) => e instanceof VLiteError && !(e instanceof ImportRejectedError) && e.code === "import_conflict");
    await client.finishImport("9");
    await client.abortImport("9");
    await client.importStatus("9");
    assert.deepEqual(calls.map((c) => `${c.method} ${c.path}`), ["POST /v1/collections/docs/imports", "PUT /v1/imports/9/chunks/1",
      "PUT /v1/imports/9/chunks/1", "PUT /v1/imports/9/chunks/2", "PUT /v1/imports/9/chunks/2", "POST /v1/imports/9/finish",
      "POST /v1/imports/9/abort", "GET /v1/imports/9"]);
    assert.equal(calls[0].body, undefined);
    assert.deepEqual(calls[1].body, { operations: [{ op: "delete", id: "1" }], policy: "skip_invalid" });
    assert.deepEqual(calls[2].body, { operations: [{ op: "delete", id: "1" }] });
    assert.equal(calls[5].body, undefined);

    const dir = mkdtempSync(join(tmpdir(), "vlite-import-"));
    const file = join(dir, "rows.ndjson");
    writeFileSync(file, '{"op":"delete","id":"1"}\n\n{"op":"delete","id":"2"}\r\n{"op":"delete","id":"3"}\n');
    calls.length = 0;
    reply({ status: 201, body: open }, report("committed", 1), report("committed", 2), { body: { ...open, state: "finished" } });
    assert.equal((await client.importNdjson(file, "docs", { chunkSize: 2 })).state, "finished");
    assert.deepEqual(calls.map((c) => c.path), ["/v1/collections/docs/imports", "/v1/imports/9/chunks/1", "/v1/imports/9/chunks/2", "/v1/imports/9/finish"]);
    assert.deepEqual(calls[1].body, { operations: [{ op: "delete", id: "1" }, { op: "delete", id: "2" }], policy: "reject_chunk" });

    // Resume: chunk 1 is skipped, chunk 2 (= last_chunk) is replayed explicitly, then finish.
    calls.length = 0;
    reply({ body: { ...open, last_chunk: 2 } }, report("already_committed", 2), { body: { ...open, state: "finished" } });
    await client.importNdjson(file, "docs", { chunkSize: 2, job: "9" });
    assert.deepEqual(calls.map((c) => c.path), ["/v1/imports/9", "/v1/imports/9/chunks/2", "/v1/imports/9/finish"]);
    // An unexpected outcome stops immediately without finishing, retrying or aborting; the job ID is exposed.
    calls.length = 0;
    reply({ body: { ...open, last_chunk: 1 } }, report("committed", 1));
    await assert.rejects(client.importNdjson(file, "docs", { chunkSize: 2, job: "9" }),
      (e) => e instanceof TransportError && e.importJob === "9");
    assert.equal(calls.length, 2);
    // A failure after a fresh reservation exposes the new job for an explicit resume or abort.
    calls.length = 0;
    reply({ status: 201, body: open }, err(409, "import_limit"));
    await assert.rejects(client.importNdjson(file, "docs", { chunkSize: 2 }),
      (e) => e instanceof VLiteError && e.code === "import_limit" && e.importJob === "9");
    assert.deepEqual(calls.map((c) => c.path), ["/v1/collections/docs/imports", "/v1/imports/9/chunks/1"]);
    // A finished job returns its status and sends nothing else; other states and collections refuse.
    calls.length = 0;
    reply({ body: { ...open, state: "finished" } }, { body: { ...open, state: "aborted" } }, { body: { ...open, collection: "other" } },
      { body: { ...open, collection_dropped: true } }, { body: { ...open, last_chunk: 5 } });
    assert.equal((await client.importNdjson(file, "docs", { job: "9" })).state, "finished");
    await assert.rejects(client.importNdjson(file, "docs", { job: "9" }), TypeError);
    await assert.rejects(client.importNdjson(file, "docs", { job: "9" }), TypeError);
    await assert.rejects(client.importNdjson(file, "docs", { job: "9" }), TypeError);
    await assert.rejects(client.importNdjson(file, "docs", { chunkSize: 2, job: "9" }), (e) => e instanceof TypeError && e.importJob === "9");
    assert.equal(calls.length, 5, "short file never sends a chunk or finishes");
    // u64 counters above Number.MAX_SAFE_INTEGER stay exact, and resume compares them exactly.
    calls.length = 0;
    const huge = "18446744073709551615";
    reply({ text: `{"job":"9","collection":"docs","collection_dropped":false,"state":"open","last_chunk":${huge},`
      + `"rows_committed":${huge},"rows_skipped":9007199254740993,"last_digest":null,"durable":true}` });
    const exact = await client.importStatus("9");
    assert.equal(exact.last_chunk, BigInt(huge));
    assert.equal(exact.rows_committed, BigInt(huge));
    assert.equal(exact.rows_skipped, 9007199254740993n);
    reply({ text: `{"job":"9","collection":"docs","collection_dropped":false,"state":"open","last_chunk":9007199254740993,`
      + `"rows_committed":0,"rows_skipped":0,"last_digest":null,"durable":true}` });
    await assert.rejects(client.importNdjson(file, "docs", { chunkSize: 2, job: "9" }),
      (e) => e instanceof TypeError && /fewer chunks/.test(e.message) && e.importJob === "9");
    reply({ text: '{"job":"9","state":"open","last_chunk":1.5,"collection":"docs"}' });
    await assert.rejects(client.importNdjson(file, "docs", { chunkSize: 2, job: "9" }),
      (e) => e instanceof TransportError && e.importJob === "9");
    reply({ text: '{"distance":1e300,"small":3}' });
    assert.deepEqual(await client.importStatus("9"), { distance: 1e300, small: 3 });
    assert.equal(calls.length, 4, "nothing beyond the status reads was sent");
    // Missing, empty and malformed files are rejected before any job is reserved.
    calls.length = 0;
    await assert.rejects(client.importNdjson(join(dir, "missing.ndjson"), "docs"), (e) => e.code === "ENOENT" && e.importJob === undefined);
    writeFileSync(file, "\n\n");
    await assert.rejects(client.importNdjson(file, "docs"), /no operations/);
    writeFileSync(file, '{"op":"delete","id":"1"}\n[1]\n');
    await assert.rejects(client.importNdjson(file, "docs"), /line 2/);
    await assert.rejects(client.importNdjson(file, "docs", { job: "9" }), /line 2/);
    assert.equal(calls.length, 0);
  });
});

test("admin operations, credentials, scoped capabilities and Prometheus text", async () => {
  await scripted(async ({ client, calls, reply }) => {
    const ok = { body: {} };
    reply(ok, ok, ok, ok, { body: { items: [] } }, { status: 201, body: { name: "snap-1", manifest: {} } }, ok, ok, ok,
      { body: { items: [], capacity: 256 } }, { status: 201, body: { credential: { id: "abcdefghijk2" }, token: "vlk_x" } },
      { status: 201, body: { credential: {}, token: "vlk_y", previous: {} } }, ok, ok, { body: { items: [] } },
      { body: { role: "scoped", scope: { operations: ["read"] } } },
      { type: "text/plain; version=0.0.4", text: "vlite_up 1\n" }, err(422, "drill_failed"), err(403, "forbidden_scope"),
      { body: { not: "text" } }, ok, ok, ok, ok);
    await client.autoMaintenance();
    await client.setAutoMaintenance({ enabled: true, interval_secs: 60 });
    await client.backupSchedule();
    await client.setBackupSchedule({ enabled: false });
    await client.listBackups();
    await client.createBackup();
    await client.verifyBackup("snap-1");
    await client.drillBackup("snap-1");
    await client.slowQueries();
    await client.listCredentials();
    const expires = Date.now() + 1000;
    assert.equal((await client.createCredential({ name: "r", collections: ["docs"], operations: ["read"], expires_unix_ms: expires })).token, "vlk_x");
    await client.rotateCredential("abcdefghijk2", { graceSeconds: 0 });
    await client.rotateCredential("abcdefghijk2");
    await client.revokeCredential("abcdefghijk2");
    await client.persistentAudit({ limit: 1000 });
    assert.equal((await client.capabilities()).role, "scoped");
    assert.equal(await client.prometheusMetrics(), "vlite_up 1\n");
    await assert.rejects(client.drillBackup("snap-1"), (e) => e.code === "drill_failed" && e.status === 422);
    await assert.rejects(client.revokeCredential("abcdefghijk2"), (e) => e.code === "forbidden_scope");
    await assert.rejects(client.prometheusMetrics(), TransportError);
    await client.tickAutoMaintenance();
    await client.runBackupSchedule();
    await client.collection("docs").maintenanceStatus();
    await client.collection("docs").maintenanceTick();
    assert.deepEqual(calls.slice(20).map((c) => `${c.method} ${c.path} ${c.body}`), [
      "POST /v1/admin/maintenance/auto/tick undefined", "POST /v1/admin/backups/schedule/run undefined",
      "GET /v1/collections/docs/index/hnsw/maintenance undefined", "POST /v1/collections/docs/index/hnsw/maintenance/tick undefined",
    ]);
    calls.length = 20;
    assert.deepEqual(calls.map((c) => `${c.method} ${c.path}`), [
      "GET /v1/admin/maintenance/auto", "PUT /v1/admin/maintenance/auto", "GET /v1/admin/backups/schedule",
      "PUT /v1/admin/backups/schedule", "GET /v1/admin/backups", "POST /v1/admin/backups", "POST /v1/admin/backups/snap-1/verify",
      "POST /v1/admin/backups/snap-1/drill", "GET /v1/admin/diagnostics/slow-queries", "GET /v1/admin/credentials",
      "POST /v1/admin/credentials", "POST /v1/admin/credentials/abcdefghijk2/rotate", "POST /v1/admin/credentials/abcdefghijk2/rotate",
      "POST /v1/admin/credentials/abcdefghijk2/revoke", "GET /v1/admin/audit/persistent?limit=1000", "GET /v1/capabilities",
      "GET /metrics?format=prometheus", "POST /v1/admin/backups/snap-1/drill", "POST /v1/admin/credentials/abcdefghijk2/revoke",
      "GET /metrics?format=prometheus",
    ]);
    assert.deepEqual(calls[1].body, { enabled: true, interval_secs: 60 });
    assert.deepEqual(calls[10].body, { name: "r", collections: ["docs"], operations: ["read"], expires_unix_ms: expires });
    assert.deepEqual(calls[11].body, { grace_seconds: 0 });
    assert.deepEqual(calls[12].body, {});
    for (const i of [5, 6, 7, 13]) assert.equal(calls[i].body, undefined);
    assert.equal(calls[16].accept, "text/plain");
    // Scoped vlk_ tokens are accepted, sent only as the bearer and never exposed on the object.
    const scopedToken = "vlk_abcdefghijk2_" + "A".repeat(43);
    const scoped = new Client("http://127.0.0.1:1", scopedToken);
    assert.ok(!JSON.stringify(scoped).includes(scopedToken));
    assert.ok(!JSON.stringify(scoped.collection("docs")).includes(scopedToken));
  });
});

test("feature REST methods use the documented routes", async () => {
  const calls = [];
  const server = createServer(async (req, res) => {
    let body = ""; for await (const chunk of req) body += chunk;
    calls.push({ method: req.method, path: req.url, body: body ? JSON.parse(body) : null });
    res.end(JSON.stringify({ hits: [], changes: [], next_cursor: "cursor" }));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  try {
    const client = new Client(`http://127.0.0.1:${server.address().port}`, token);
    await client.changes({ limit: 20 });
    await client.collection("docs").groupedSearch({ vector: [1], field: "group", groups: 2, hits_per_group: 3 });
    await client.put("1", { vector: [1], ttl_ms: 30000 });
    assert.equal(calls[0].path, "/v1/changes?limit=20");
    assert.equal(calls[1].path, "/v1/collections/docs/search/grouped");
    assert.equal(calls[2].body.ttl_ms, 30000);
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  }
});

// ---------------------------------------------------------------------------------------------
// Agent memory facade: docs/projects/roadmap/serving-memory-contract.md sections 3, 4, 6 and 8.
// Requests, responses and structural vectors come from the independent logical goldens in
// tests/fixtures/agent_memory/wire; no expected value is derived from the client under test.
const wireDirectory = new URL("../../tests/fixtures/agent_memory/wire/", import.meta.url);
// Independent of the client: integer literals beyond 2^53 stay exact as bigint.
const exact = (text) => JSON.parse(text, (_, value, context) =>
  typeof value === "number" && Number.isInteger(value) && !Number.isSafeInteger(value) && /^-?\d+$/.test(context?.source)
    ? BigInt(context.source) : value);
// Hand-written from contract section 4: [fixture operation, facade method, REST suffix].
const MEMORY_OPERATIONS = [
  ["remember", "remember", "/remember"], ["recall", "recall", "/recall"], ["facts", "findFacts", "/facts/query"],
  ["correct_facts", "correctFacts", "/facts/correct"], ["consolidate", "consolidate", "/consolidate"],
  ["forget", "forget", "/forget"], ["receipt", "getReceipt", "/receipts/lookup"],
  ["erasure", "getErasure", "/erasures/get"], ["resume_erasure", "resumeErasure", "/erasures/resume"],
];
const MEMORY_METHODS = MEMORY_OPERATIONS.map(([, method]) => method);
const wire = Object.fromEntries(MEMORY_OPERATIONS.map(([name]) =>
  [name, exact(readFileSync(new URL(`${name}.json`, wireDirectory), "utf8"))]));
const inventoryOf = (name) => wire[name].structural.find((s) => s.schema === "http" && s.valid && s.operation === name).value;
const successesOf = (name) => wire[name].structural.filter((s) => s.schema === "exchange" && s.valid && s.status < 400);
// One accepted response per facade method, for tests that only care about the request.
const ACCEPTED = Object.fromEntries(MEMORY_OPERATIONS.map(([name, method]) => [method, successesOf(name)[0].value]));
const U64_KEYS = new Set(["revision", "record", "expires_at_ms", "slot_revision", "generation"]);
const I64_KEYS = new Set(["observed_at_ms", "as_of_ms", "at_ms", "valid_from_ms", "valid_to_ms"]);

// Input-only fixture compression (wire README): $text, $repeat and $properties expand before use.
function expand(value) {
  if (Array.isArray(value)) return value.map(expand);
  if (value === null || typeof value !== "object") return value;
  if (Object.hasOwn(value, "$text")) return value.$text.repeat(value.count);
  if (Object.hasOwn(value, "$repeat")) {
    const item = expand(value.$repeat);
    return Array.from({ length: value.count }, (_, index) => ({
      ...structuredClone(item), ...Object.fromEntries((value.enumerate ?? []).map((key) => [key, `${item[key]}${index}`])) }));
  }
  if (Object.hasOwn(value, "$properties")) {
    return Object.fromEntries(Array.from({ length: value.count }, (_, index) => [`p${index}`, structuredClone(expand(value.$properties))]));
  }
  return Object.fromEntries(Object.entries(value).map(([key, child]) => [key, expand(child)]));
}

// What the contract requires on the wire for an accepted fixture request: u64/i64 integers are decimal strings.
function wireOf(value, key, parent) {
  if (typeof value === "number" || typeof value === "bigint") {
    return U64_KEYS.has(key) || I64_KEYS.has(key) || (key === "value" && parent?.type === "integer") ? String(value) : value;
  }
  if (Array.isArray(value)) return value.map((child) => wireOf(child, key, parent));
  if (value !== null && typeof value === "object") {
    return Object.fromEntries(Object.entries(value).map(([name, child]) => [name, wireOf(child, name, value)]));
  }
  return value;
}

// Scripted in-process server that records the exact request text, headers and parsed body.
async function memoryServer(run) {
  const calls = [];
  const replies = [];
  const server = createServer(async (req, res) => {
    let raw = "";
    for await (const chunk of req) raw += chunk;
    calls.push({ method: req.method, path: req.url, headers: req.headers, raw, body: raw ? exact(raw) : undefined });
    const reply = replies.shift() ?? err(599, "unscripted");
    if (reply.destroy) { req.socket.destroy(); return; }
    if (reply.delayMs) await new Promise((resolve) => setTimeout(resolve, reply.delayMs));
    res.writeHead(reply.status ?? 200, { "Content-Type": reply.type ?? "application/json", ...(reply.headers ?? {}) });
    res.end(reply.text ?? (reply.body === undefined ? "" : JSON.stringify(reply.body)));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const base = `http://127.0.0.1:${server.address().port}`;
  try {
    await run({ client: new Client(base, token), base, calls, reply: (...items) => replies.push(...items) });
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  }
}

// A TypeError for the intended reason, raised as a rejection (never synchronously) before anything is sent.
async function refuses(call, pattern) {
  let promise;
  try { promise = call(); } catch (error) { assert.fail(`must reject, not throw synchronously: ${error.message}`); }
  await assert.rejects(promise, (error) => error instanceof TypeError && pattern.test(error.message), String(pattern));
}
// The rejection of a promise, for tests that assert several properties of it.
async function failure(promise, label = "") {
  try { await promise; } catch (error) { return error; }
  return assert.fail(`expected a rejection ${label}`.trim());
}

const scope = { agent: "a" };
const source = { source_id: "s1", source_kind: "caller" };
const mutation = { scope, idempotency_key: "k1" };
const memoryItem = (extra = {}) => ({ id: "e1", kind: "entity", content: "Ada", observed_at_ms: "90", source, ...extra });
const rememberOf = (itemExtra = {}, top = {}) => ({ ...mutation, items: [memoryItem(itemExtra)], ...top });
const recallOf = (extra = {}) => ({ scope, text: "Ada", ...extra });
const slot = { subject_id: "e1", predicate: "role", slot_key: "primary" };
const correctionOf = (extra = {}) => ({ ...mutation, slot, series: { id: "f1", revision: "7" }, slot_revision: "7",
  interval: { valid_from_ms: "0", valid_to_ms: null }, action: "close", at_ms: "100", ...extra });
const forgetOf = (target, extra = {}) => ({ ...mutation, target, mode: "logical_delete", ...extra });
const consolidateOf = (extra = {}) => ({ ...mutation, inputs: [{ id: "m", revision: "1" }],
  output: memoryItem({ id: "summary1", kind: "episodic", source: { ...source, transformation_id: "summary-v1" } }), ...extra });
const resumeOf = (extra = {}) => ({ ...mutation, job_id: "j1", revision: "8", generation: "1", resume_token: "opaque1", ...extra });

test("memory: exact method, path, headers and body for all nine methods", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    for (const [name, method, suffix] of MEMORY_OPERATIONS) {
      const request = inventoryOf(name);
      assert.equal(request.method, "POST");
      assert.equal(request.path, `/v1/memory/notes${suffix}`, `${name}: the contract table and the fixture agree`);
      const body = name === "consolidate" ? { ...request.body, dry_run: true } : request.body;
      const success = successesOf(name)[0];
      reply({ status: success.status, body: success.value });
      const result = await memory[method](structuredClone(body));
      const call = calls.at(-1);
      assert.equal(call.method, "POST", method);
      assert.equal(call.path, request.path, method);
      assert.equal(call.headers.authorization, `Bearer ${token}`);
      assert.equal(call.headers["content-type"], "application/json");
      assert.equal(call.headers.accept, "application/json");
      for (const header of ["idempotency-key", "x-idempotency-key", "if-match", "x-tenant"]) {
        assert.equal(call.headers[header], undefined, `${method} never sends ${header}`);
      }
      assert.deepEqual(call.body, body, method);
      assert.equal(call.raw, JSON.stringify(body), `${method}: compact JSON, no extra members`);
      assert.deepEqual(result, success.value, method);
    }
    assert.equal(calls.length, 9);
    assert.equal(MEMORY_METHODS.length, new Set(MEMORY_METHODS).size);
  });
});

test("memory: the facade is binding-local and refuses unsafe binding names", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const { Memory } = await import("./index.js");
    const notes = client.memory("notes");
    const other = client.memory("a-b_9");
    assert.ok(notes instanceof Memory);
    assert.equal(notes.binding, "notes");
    assert.deepEqual(Object.keys(notes), ["binding"]);
    assert.ok(!JSON.stringify(notes).includes(token), "the token is never reachable from the facade");
    for (const [facade, binding] of [[notes, "notes"], [other, "a-b_9"], [notes, "notes"]]) {
      reply({ body: ACCEPTED.recall });
      await facade.recall(recallOf());
      assert.equal(calls.at(-1).path, `/v1/memory/${binding}/recall`);
    }
    for (const good of ["a", "a".repeat(63), "a0_-"]) assert.equal(client.memory(good).binding, good);
    for (const bad of ["", "Notes", "1a", "_a", "-a", "a b", "a/b", "../x", "a?b", "a#b", "a%2Fb", "a.b", "a".repeat(64), "é", "a\n",
      null, undefined, 1, {}, ["notes"]]) {
      assert.throws(() => client.memory(bad), { name: "TypeError", message: /binding/ });
    }
    assert.equal(calls.length, 3, "an invalid binding never reaches the network");
  });
});

test("memory: every fixture request the oracle accepts is sent canonically and its response returned unchanged", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    let swept = 0;
    for (const [name, method, suffix] of MEMORY_OPERATIONS) {
      for (const fixture of wire[name].cases) {
        const body = fixture.request.body;
        if (fixture.expected.status !== 200 || typeof body !== "object" || Object.hasOwn(body, "$json_padding")) continue;
        assert.equal(fixture.request.path, `/v1/memory/notes${suffix}`, fixture.name);
        const request = expand(body);
        reply({ body: fixture.expected.body });
        const result = await memory[method](structuredClone(request));
        assert.deepEqual(calls.at(-1).body, wireOf(request), fixture.name);
        assert.deepEqual(result, fixture.expected.body, fixture.name);
        swept += 1;
      }
    }
    assert.ok(swept >= 50, `swept ${swept} fixture exchanges`);
    assert.equal(calls.length, swept);
  });
});

// Request-direction structural vectors: schema -> [facade method, embedding of the vector in a request].
const EMBEDDINGS = {
  RememberRequest: ["remember", (v) => v], RecallRequest: ["recall", (v) => v], FactsRequest: ["findFacts", (v) => v],
  CorrectionRequest: ["correctFacts", (v) => v], ConsolidateRequest: ["consolidate", (v) => v], ForgetRequest: ["forget", (v) => v],
  Item: ["remember", (v) => ({ ...mutation, items: [v] })], Target: ["forget", (v) => forgetOf(v)],
  Scalar: ["remember", (v) => rememberOf({ metadata: { m: v } })], Properties: ["remember", (v) => rememberOf({ metadata: v })],
  Limits: ["recall", (v) => recallOf({ limits: v })], Ranking: ["recall", (v) => recallOf({ ranking: v })],
  Source: ["remember", (v) => rememberOf({ source: v })], Scope: ["recall", (v) => ({ scope: v, text: "Ada" })],
  Interval: ["correctFacts", (v) => correctionOf({ interval: v })], Expected: ["remember", (v) => rememberOf({ expected: v })],
  Condition: ["recall", (v) => recallOf({ filter: { all: [v] } })], Filter: ["recall", (v) => recallOf({ filter: v })],
  U64: ["remember", (v) => rememberOf({ expected: { revision: v } })], I64: ["remember", (v) => rememberOf({ observed_at_ms: v })],
};
// The client refuses only representation defects (key, shape, union tag, integer form). Every policy
// refusal (bounds, counts, conditional rules) belongs to the server and must reach it unchanged.
const LOCAL_REFUSALS = new Set(["source-required-kind", "source-unknown-field", "old-validity-name-rejected",
  "expected-invalid-90", "expected-invalid-91", "expected-invalid-92", "typed-boolean-not-integer",
  "target-multiple-branches-rejected", "correction-close-requires-time"]);

test("memory: the client refuses only representation defects and forwards policy refusals untouched", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    const counts = { accepted: 0, local: 0, forwarded: 0 };
    for (const [name] of MEMORY_OPERATIONS) {
      for (const vector of wire[name].structural) {
        const embedding = vector.request === true ? EMBEDDINGS[vector.schema] : undefined;
        // JavaScript cannot tell the float literal 1.0 from the integer 1.
        if (embedding === undefined || vector.name === "integer-invalid-U64-1.0") continue;
        const [method, embed] = embedding;
        const value = expand(structuredClone(vector.value));
        const request = embed(value);
        const snapshot = structuredClone(request);
        const before = calls.length;
        const label = `${vector.schema} ${vector.name}`;
        const local = !vector.valid && (LOCAL_REFUSALS.has(vector.name) || vector.schema === "U64" || vector.schema === "I64"
          || (vector.schema === "Limits" && Object.values(value).some((limit) => limit < 0)));
        if (local) {
          await refuses(() => memory[method](request), /^request/);
          assert.equal(calls.length, before, label);
          counts.local += 1;
        } else if (vector.valid) {
          reply({ body: ACCEPTED[method] });
          assert.deepEqual(await memory[method](request), ACCEPTED[method], label);
          assert.deepEqual(calls.at(-1).body, wireOf(request), label);
          counts.accepted += 1;
        } else {
          reply(err(422, "invalid_input"));
          await assert.rejects(memory[method](request), (e) => e instanceof VLiteError && e.status === 422 && e.code === "invalid_input", label);
          assert.deepEqual(calls.at(-1).body, wireOf(request), label);
          counts.forwarded += 1;
        }
        assert.deepEqual(request, snapshot, `${label}: the caller's request is never mutated`);
      }
    }
    assert.ok(counts.accepted >= 50 && counts.local >= 20 && counts.forwarded >= 40, JSON.stringify(counts));
  });
});

const assertionOf = (extra = {}) => ({ subject_id: "e1", predicate: "role", slot_key: "primary", destination_id: "e2",
  interval: { valid_from_ms: "0", valid_to_ms: null }, slot_expected: { absent: true }, ...extra });
// [path of the value inside the request, method, request carrying the value at that path]
const U64_POSITIONS = [
  ["items[0].expected.revision", "remember", (v) => rememberOf({ expected: { revision: v } })],
  ["items[0].expires_at_ms", "remember", (v) => rememberOf({ expires_at_ms: v })],
  ["items[0].references[0].revision", "remember", (v) => rememberOf({ references: [{ id: "r", revision: v }] })],
  ["items[0].evidence[0].record", "remember", (v) => rememberOf({ evidence: [{ collection: "ev", record: v }] })],
  ["evidence[0].record", "remember", (v) => rememberOf({}, { evidence: [{ collection: "ev", record: v }] })],
  ["items[0].assertion.slot_expected.revision", "remember",
    (v) => rememberOf({ kind: "fact", assertion: assertionOf({ slot_expected: { revision: v } }) })],
  ["series.revision", "correctFacts", (v) => correctionOf({ series: { id: "f1", revision: v } })],
  ["slot_revision", "correctFacts", (v) => correctionOf({ slot_revision: v })],
  ["replacement.expected.revision", "correctFacts",
    (v) => correctionOf({ action: "replace", replacement: memoryItem({ expected: { revision: v } }) })],
  ["inputs[0].revision", "consolidate", (v) => consolidateOf({ inputs: [{ id: "m", revision: v }] })],
  ["target.ids[0].revision", "forget", (v) => forgetOf({ ids: [{ id: "m", revision: v }] })],
  ["target.series.revision", "forget", (v) => forgetOf({ series: { id: "m", revision: v } })],
  ["target.entity.revision", "forget", (v) => forgetOf({ entity: { id: "m", revision: v } })],
  ["expires_at_ms", "forget", (v) => forgetOf({ scope: true }, { mode: "expire", expires_at_ms: v })],
  ["revision", "resumeErasure", (v) => resumeOf({ revision: v })],
  ["generation", "resumeErasure", (v) => resumeOf({ generation: v })],
];
const interval = (field, v) => ({ valid_from_ms: null, valid_to_ms: null, [field]: v });
const integerScalar = (v) => ({ type: "integer", value: v });
const I64_POSITIONS = [
  ["items[0].observed_at_ms", "remember", (v) => rememberOf({ observed_at_ms: v })],
  ["items[0].assertion.interval.valid_from_ms", "remember",
    (v) => rememberOf({ kind: "fact", assertion: assertionOf({ interval: interval("valid_from_ms", v) }) })],
  ["items[0].assertion.interval.valid_to_ms", "remember",
    (v) => rememberOf({ kind: "fact", assertion: assertionOf({ interval: interval("valid_to_ms", v) }) })],
  ["interval.valid_from_ms", "correctFacts", (v) => correctionOf({ interval: interval("valid_from_ms", v) })],
  ["interval.valid_to_ms", "correctFacts", (v) => correctionOf({ interval: interval("valid_to_ms", v) })],
  ["correction_interval.valid_from_ms", "correctFacts", (v) => correctionOf({ correction_interval: interval("valid_from_ms", v) })],
  ["correction_interval.valid_to_ms", "correctFacts", (v) => correctionOf({ correction_interval: interval("valid_to_ms", v) })],
  ["at_ms", "correctFacts", (v) => correctionOf({ at_ms: v })],
  ["as_of_ms", "recall", (v) => recallOf({ as_of_ms: v })],
  ["as_of_ms", "findFacts", (v) => ({ scope, slot, as_of_ms: v })],
  ["items[0].metadata.n.value", "remember", (v) => rememberOf({ metadata: { n: integerScalar(v) } })],
  ["evidence[0].metadata.n.value", "remember",
    (v) => rememberOf({}, { evidence: [{ collection: "ev", record: "1", metadata: { n: integerScalar(v) } }] })],
  ["filter.all[0].value.value", "recall", (v) => recallOf({ filter: { all: [{ field: "n", op: "gt", value: integerScalar(v) }] } })],
  ["filter.any[0].value.value", "recall", (v) => recallOf({ filter: { any: [{ field: "n", op: "lte", value: integerScalar(v) }] } })],
];
const at = (value, path) => path.split(/\.|\[|\]/).filter(Boolean).reduce((node, key) => node[key], value);
const escaped = (text) => text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
const refusal = (path, pattern) => new RegExp(`^request\\.${escaped(path)} ${pattern.source}`);
const TYPE = /must be a decimal string, a safe integer or a bigint/;
const UNSAFE = /is not a safe integer/;
const CANON = /must be a canonical decimal string/;
const RANGE_U = /is outside the unsigned 64-bit range/;
const RANGE_I = /is outside the signed 64-bit range/;
const U64_VALID = [["0", "0"], [0, "0"], [0n, "0"], [1, "1"], ["1", "1"], [Number.MAX_SAFE_INTEGER, "9007199254740991"],
  ["9007199254740993", "9007199254740993"], [9007199254740993n, "9007199254740993"],
  ["18446744073709551615", "18446744073709551615"], [18446744073709551615n, "18446744073709551615"]];
const I64_VALID = [["0", "0"], [0, "0"], [-0, "0"], [0n, "0"], [-1, "-1"], ["-1", "-1"], [-1n, "-1"], [1, "1"],
  [Number.MIN_SAFE_INTEGER, "-9007199254740991"], [Number.MAX_SAFE_INTEGER, "9007199254740991"],
  ["-9007199254740993", "-9007199254740993"], [-9007199254740993n, "-9007199254740993"], [9007199254740993n, "9007199254740993"],
  ["-9223372036854775808", "-9223372036854775808"], [-9223372036854775808n, "-9223372036854775808"],
  ["9223372036854775807", "9223372036854775807"], [9223372036854775807n, "9223372036854775807"]];
const NOT_INTEGER = [[true, TYPE], [null, TYPE], [[], TYPE], [{}, TYPE], [Object(1n), TYPE], [Object("1"), TYPE], [() => 1, TYPE]];
const U64_INVALID = [
  [-1, RANGE_U], [-1n, RANGE_U], ["18446744073709551616", RANGE_U], [18446744073709551616n, RANGE_U], ["9".repeat(30), RANGE_U],
  [2 ** 53, UNSAFE], [18446744073709551615, UNSAFE], [9007199254740993, UNSAFE], [1e21, UNSAFE], [1.5, UNSAFE],
  [NaN, UNSAFE], [Infinity, UNSAFE], [-Infinity, UNSAFE],
  ["-1", CANON], ["00", CANON], ["01", CANON], ["+1", CANON], [" 1", CANON], ["1 ", CANON], ["1.0", CANON], ["1e3", CANON],
  ["0x1", CANON], ["", CANON], ["-0", CANON], ["\u0663", CANON], ["1_000", CANON], ["1\n", CANON], ...NOT_INTEGER,
];
const I64_INVALID = [
  [9223372036854775808n, RANGE_I], [-9223372036854775809n, RANGE_I], ["9223372036854775808", RANGE_I],
  ["-9223372036854775809", RANGE_I], [`-${"9".repeat(30)}`, RANGE_I],
  [2 ** 53, UNSAFE], [-(2 ** 53), UNSAFE], [9223372036854775807, UNSAFE], [-9223372036854775808, UNSAFE], [1.5, UNSAFE],
  [NaN, UNSAFE], [Infinity, UNSAFE],
  ["-0", CANON], ["-01", CANON], ["00", CANON], ["+1", CANON], [" 1", CANON], ["1.0", CANON], ["--1", CANON], ["", CANON],
  ["-", CANON], ["1e3", CANON], ["\u0663", CANON], ...NOT_INTEGER,
];

test("memory: u64 and i64 inputs accept the exact forms and serialize canonical decimal strings at every position", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    for (const [positions, valid, invalid] of [[U64_POSITIONS, U64_VALID, U64_INVALID], [I64_POSITIONS, I64_VALID, I64_INVALID]]) {
      for (const [path, method, build] of positions) {
        for (const [input, expected] of valid) {
          reply({ body: ACCEPTED[method] });
          await memory[method](build(input));
          const call = calls.at(-1);
          assert.equal(typeof at(call.body, path), "string", `${method} ${path} ${String(input)}`);
          assert.equal(at(call.body, path), expected, `${method} ${path} ${String(input)}`);
          assert.ok(call.raw.includes(`"${expected}"`), `${path}: quoted in the raw JSON text`);
        }
        const before = calls.length;
        for (const [input, pattern] of invalid) {
          if (input === null && /valid_(from|to)_ms$/.test(path)) continue; // an explicit null is an unbounded interval end
          await refuses(() => memory[method](build(input)), refusal(path, pattern));
        }
        assert.equal(calls.length, before, `${method} ${path}: refused integers are never sent`);
      }
    }
  });
});

test("memory: nullable interval bounds keep an explicit null and never become defaults", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    reply({ body: ACCEPTED.correctFacts });
    await memory.correctFacts(correctionOf({ interval: { valid_from_ms: null, valid_to_ms: null },
      correction_interval: { valid_from_ms: "-1", valid_to_ms: null } }));
    assert.deepEqual(calls.at(-1).body.interval, { valid_from_ms: null, valid_to_ms: null });
    assert.deepEqual(calls.at(-1).body.correction_interval, { valid_from_ms: "-1", valid_to_ms: null });
    assert.match(calls.at(-1).raw, /"interval":\{"valid_from_ms":null,"valid_to_ms":null\}/);
    const before = calls.length;
    for (const [bad, field] of [[{ valid_to_ms: null }, "valid_from_ms"], [{ valid_from_ms: "0" }, "valid_to_ms"],
      [{ valid_from_ms: "0", valid_to_ms: undefined }, "valid_to_ms"], [{}, "valid_from_ms"]]) {
      await refuses(() => memory.correctFacts(correctionOf({ interval: bad })), new RegExp(`^request\\.interval\\.${field} is required`));
    }
    await refuses(() => memory.correctFacts(correctionOf({ interval: null })), /^request\.interval must be an object/);
    assert.equal(calls.length, before);
  });
});

const BUDGETS = [
  ["deadline_ms", "remember", (v) => rememberOf({}, { deadline_ms: v })],
  ["max_references", "remember", (v) => rememberOf({}, { max_references: v })],
  ["deadline_ms", "consolidate", (v) => consolidateOf({ deadline_ms: v })],
  ["deadline_ms", "resumeErasure", (v) => resumeOf({ deadline_ms: v })],
  ["limit", "recall", (v) => recallOf({ limit: v })],
  ["limit", "findFacts", (v) => ({ scope, slot, limit: v })],
  ...["candidates", "vector_components", "evidence_bytes", "mmr_components", "deadline_ms", "max_hops", "max_visited_nodes",
    "max_edges_examined", "max_edges_per_node", "max_output_nodes", "max_output_edges", "max_output_paths"].flatMap((field) => [
    [`limits.${field}`, "recall", (v) => recallOf({ limits: { [field]: v } })],
    [`limits.${field}`, "findFacts", (v) => ({ scope, slot, limits: { [field]: v } })]]),
];

test("memory: budgets stay JSON integers, are forwarded unclamped and refuse only unrepresentable values", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    for (const [path, method, build] of BUDGETS) {
      for (const value of [0, 1, 250, 5000, 5001, 100000000, Number.MAX_SAFE_INTEGER]) {
        reply({ body: ACCEPTED[method] });
        await memory[method](build(value));
        assert.equal(at(calls.at(-1).body, path), value, `${method}.${path}`);
        assert.equal(typeof at(calls.at(-1).body, path), "number", "a budget is a JSON integer, not a string");
      }
      const before = calls.length;
      for (const value of [-1, 1.5, NaN, Infinity, 2 ** 53, "250", 250n, true, null, [], {}]) {
        await refuses(() => memory[method](build(value)), new RegExp(`^request\\.${escaped(path)} must be a non-negative safe integer`));
      }
      assert.equal(calls.length, before);
    }
  });
});

test("memory: MemoryExpected is exactly {absent: true} or {revision}", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    const sites = [
      ["items[0].expected", (expected) => rememberOf({ expected })],
      ["items[0].assertion.slot_expected", (expected) => rememberOf({ kind: "fact", assertion: assertionOf({ slot_expected: expected }) })],
    ];
    for (const [path, build] of sites) {
      for (const [expected, wireExpected] of [[{ absent: true }, { absent: true }], [{ revision: "7" }, { revision: "7" }],
        [{ revision: 7 }, { revision: "7" }], [{ revision: 18446744073709551615n }, { revision: "18446744073709551615" }]]) {
        reply({ body: ACCEPTED.remember });
        await memory.remember(build(expected));
        assert.deepEqual(at(calls.at(-1).body, path), wireExpected);
      }
      const before = calls.length;
      for (const [expected, suffix] of [[{ absent: false }, "\\.absent must be true"], [{ absent: "true" }, "\\.absent must be true"],
        [{ absent: 1 }, "\\.absent must be true"], [{ absent: true, revision: "1" }, " must have exactly one of absent, revision"],
        [{}, " must have exactly one of absent, revision"], [{ revision: "1", extra: 1 }, " must have exactly one of absent, revision"],
        [{ Absent: true }, " must have exactly one of absent, revision"], [{ any: true }, " must have exactly one of absent, revision"],
        [{ revision: null }, `\\.revision ${TYPE.source}`], [{ revision: -1 }, `\\.revision ${RANGE_U.source}`],
        ["any", " must be an object"], ["absent", " must be an object"], [null, " must be an object"], [[], " must be an object"],
        [true, " must be an object"]]) {
        await refuses(() => memory.remember(build(expected)), new RegExp(`^request\\.${escaped(path)}${suffix}`));
      }
      assert.equal(calls.length, before);
    }
  });
});

test("memory: MemoryTarget is exactly one of ids, series, entity, session or scope", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    for (const [target, wireTarget] of [
      [{ ids: [{ id: "a", revision: "1" }, { id: "b", revision: 2 }, { id: "c", revision: 18446744073709551615n }] },
        { ids: [{ id: "a", revision: "1" }, { id: "b", revision: "2" }, { id: "c", revision: "18446744073709551615" }] }],
      [{ ids: [] }, { ids: [] }], // an empty list is the server's policy refusal, not the client's
      [{ series: { id: "s", revision: 3 } }, { series: { id: "s", revision: "3" } }],
      [{ entity: { id: "e", revision: "4" } }, { entity: { id: "e", revision: "4" } }],
      [{ session: true }, { session: true }], [{ scope: true }, { scope: true }]]) {
      reply({ body: ACCEPTED.forget });
      await memory.forget(forgetOf(target));
      assert.deepEqual(calls.at(-1).body.target, wireTarget);
    }
    const before = calls.length;
    const one = " must have exactly one of ids, series, entity, session, scope";
    for (const [target, suffix] of [
      [{}, one], [{ ids: [{ id: "a", revision: "1" }], scope: true }, one], [{ scope: true, session: true }, one],
      [{ id: "a", revision: "1" }, one], [{ Scope: true }, one], [{ scope: true, extra: 1 }, one],
      [{ session: false }, "\\.session must be true"], [{ scope: "true" }, "\\.scope must be true"], [{ scope: 1 }, "\\.scope must be true"],
      [{ ids: "a" }, "\\.ids must be an array"], [{ ids: [{ id: "a" }] }, "\\.ids\\[0\\]\\.revision is required"],
      [{ ids: [null] }, "\\.ids\\[0\\] must be an object"], [{ ids: [{ id: "a", revision: "1", extra: 1 }] }, "\\.ids\\[0\\]\\.extra is not a field"],
      [{ series: { id: "s", revision: "1", extra: 1 } }, "\\.series\\.extra is not a field"], [{ series: null }, "\\.series must be an object"],
      [{ entity: "e" }, "\\.entity must be an object"], [{ entity: { id: "e", revision: 1.5 } }, `\\.entity\\.revision ${UNSAFE.source}`],
      [null, " must be an object"], [[], " must be an object"], ["scope", " must be an object"], [true, " must be an object"]]) {
      await refuses(() => memory.forget(forgetOf(target)), new RegExp(`^request\\.target${suffix}`));
    }
    assert.equal(calls.length, before);
  });
});

test("memory: MemoryScalar is a tagged union of string, integer, float and boolean", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    const sites = [
      ["items[0].metadata.m", (value) => ["remember", rememberOf({ metadata: { m: value } })]],
      ["evidence[0].metadata.m", (value) => ["remember", rememberOf({}, { evidence: [{ collection: "ev", record: "1", metadata: { m: value } }] })]],
      ["filter.all[0].value", (value) => ["recall", recallOf({ filter: { all: [{ field: "m", op: "eq", value }] } })]],
      ["filter.any[0].value", (value) => ["recall", recallOf({ filter: { any: [{ field: "m", op: "lt", value }] } })]],
    ];
    for (const [path, build] of sites) {
      for (const [scalar, wireScalar] of [[{ type: "string", value: "x" }, { type: "string", value: "x" }],
        [{ type: "string", value: "" }, { type: "string", value: "" }],
        [{ type: "integer", value: 5 }, { type: "integer", value: "5" }],
        [{ type: "integer", value: -9223372036854775808n }, { type: "integer", value: "-9223372036854775808" }],
        [{ type: "float", value: 1.25 }, { type: "float", value: 1.25 }], [{ type: "float", value: 5 }, { type: "float", value: 5 }],
        [{ type: "float", value: -1e300 }, { type: "float", value: -1e300 }],
        [{ type: "boolean", value: true }, { type: "boolean", value: true }], [{ type: "boolean", value: false }, { type: "boolean", value: false }]]) {
        const [method, request] = build(scalar);
        reply({ body: ACCEPTED[method] });
        await memory[method](request);
        assert.deepEqual(at(calls.at(-1).body, path), wireScalar, `${path} ${JSON.stringify(wireScalar)}`);
      }
      assert.match(calls.at(-1).raw, /"type":"boolean","value":false/);
      const before = calls.length;
      const tags = "\\.type must be string, integer, float or boolean";
      for (const [scalar, suffix] of [
        [{ type: "integer", value: true }, `\\.value ${TYPE.source}`], [{ type: "integer", value: 1.5 }, `\\.value ${UNSAFE.source}`],
        [{ type: "float", value: "1" }, "\\.value must be a finite number"], [{ type: "float", value: NaN }, "\\.value must be a finite number"],
        [{ type: "float", value: Infinity }, "\\.value must be a finite number"], [{ type: "float", value: -Infinity }, "\\.value must be a finite number"],
        [{ type: "float", value: 1n }, "\\.value must be a finite number"], [{ type: "float", value: null }, "\\.value must be a finite number"],
        [{ type: "string", value: 1 }, "\\.value must be a string"], [{ type: "string", value: null }, "\\.value must be a string"],
        [{ type: "boolean", value: "true" }, "\\.value must be a boolean"], [{ type: "boolean", value: 1 }, "\\.value must be a boolean"],
        [{ type: "decimal", value: "1" }, tags], [{ type: "toString", value: "x" }, tags], [{ type: "constructor", value: "x" }, tags],
        [{ type: "__proto__", value: "x" }, tags], [{ type: 1, value: "x" }, tags], [{ value: "x" }, tags], [{ type: "string" }, "\\.value is required"],
        [{ type: "string", value: "x", extra: 1 }, "\\.extra is not a field"], ["x", " must be an object"], [1, " must be an object"],
        [null, " must be an object"], [[], " must be an object"]]) {
        const [method, request] = build(scalar);
        await refuses(() => memory[method](request), new RegExp(`^request\\.${escaped(path)}${suffix}`));
      }
      assert.equal(calls.length, before);
    }
  });
});

test("memory: metadata maps keep every key, including __proto__, and accept null-prototype requests", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    const metadata = Object.fromEntries([["__proto__", { type: "string", value: "p" }], ["constructor", { type: "integer", value: 1n }],
      ["toString", { type: "boolean", value: true }], ["a.b", { type: "float", value: 0.5 }], ["", { type: "string", value: "" }]]);
    reply({ body: ACCEPTED.remember });
    await memory.remember(rememberOf({ metadata }));
    assert.equal(calls.at(-1).raw, `{"scope":{"agent":"a"},"idempotency_key":"k1","items":[{"id":"e1","kind":"entity","content":"Ada","observed_at_ms":"90",`
      + `"source":{"source_id":"s1","source_kind":"caller"},"metadata":{"__proto__":{"type":"string","value":"p"},"constructor":{"type":"integer","value":"1"},`
      + `"toString":{"type":"boolean","value":true},"a.b":{"type":"float","value":0.5},"":{"type":"string","value":""}}}]}`);
    assert.deepEqual(Object.keys(calls.at(-1).body.items[0].metadata), ["__proto__", "constructor", "toString", "a.b", ""]);
    reply({ body: ACCEPTED.recall });
    await memory.recall(Object.assign(Object.create(null), recallOf()));
    assert.deepEqual(calls.at(-1).body, recallOf());
  });
});

const LIMITS_ALL = { candidates: 200, vector_components: 1000000, evidence_bytes: 1048576, mmr_components: 10000000, deadline_ms: 250,
  max_hops: 2, max_visited_nodes: 1000, max_edges_examined: 10000, max_edges_per_node: 1000, max_output_nodes: 100,
  max_output_edges: 200, max_output_paths: 20 };
const PROJECTION_ALL = { snippets: true, provenance: false, paths: true, metadata: ["m"] };
const FULL_ITEM = { id: "e1", kind: "fact", content: "Ada", observed_at_ms: "90", importance: 0.5, confidence: 0.25, expires_at_ms: "200",
  source: { source_id: "s1", source_kind: "caller", uri: "https://example.test/a", transformation_id: "t1" },
  provenance: [{ source_id: "s2", source_kind: "caller", uri: "u", transformation_id: "t" }],
  references: [{ id: "e0", revision: "1" }], evidence: [{ collection: "ev", record: "1" }],
  metadata: { m: { type: "string", value: "x" } }, assertion: assertionOf({ disagreement: "disputed" }), expected: { absent: true },
  merge: "merge_provenance" };
const FULL_EVIDENCE = { collection: "ev", record: "1", vector: [1, 0.5], text: "t", payload_base64: "AA==",
  metadata: { n: { type: "integer", value: "1" } } };
const FULL_MUTATION = { ...mutation, max_references: 64, deadline_ms: 250, require_complete_feed: false };
// Every field of every request schema in contract section 8, all in canonical wire form.
const FULL = {
  remember: { ...FULL_MUTATION, items: [FULL_ITEM], evidence: [FULL_EVIDENCE] },
  recall: { scope: { agent: "a", user: "u", session: "s" }, scopes: [{ agent: "a", user: "u", session: "s" }], vector: [1, 0.5], text: "Ada",
    embedding_provider: "p", entity_seeds: ["e1"], kinds: ["entity", "episodic"],
    filter: { all: [{ field: "x", op: "eq", value: { type: "boolean", value: true } }], any: [{ field: "y", op: "exists" }] },
    as_of_ms: "100", ranking: { selection: "mmr", weights: [1, 0, 0], half_life_ms: 10, future_observations: "clamp", lambda: 0.5 },
    limits: LIMITS_ALL, projection: PROJECTION_ALL, mode: "exact", limit: 20, allow_partial: true },
  findFacts: { scope: { agent: "a", user: "u" }, slot, as_of_ms: "100", history: false, limit: 20, limits: LIMITS_ALL,
    projection: PROJECTION_ALL, allow_partial: false },
  correctFacts: { ...FULL_MUTATION, slot, series: { id: "f1", revision: "7" }, slot_revision: "7",
    interval: { valid_from_ms: "0", valid_to_ms: null }, action: "replace", at_ms: "100",
    correction_interval: { valid_from_ms: null, valid_to_ms: "9223372036854775807" }, replacement: FULL_ITEM, evidence: [FULL_EVIDENCE] },
  consolidate: { ...FULL_MUTATION, inputs: [{ id: "m", revision: "1", disposition: "logical_delete" }],
    output: { ...FULL_ITEM, kind: "episodic" }, evidence: [FULL_EVIDENCE], dry_run: true },
  forget: { ...FULL_MUTATION, target: { ids: [{ id: "m", revision: "1" }] }, mode: "expire", expires_at_ms: "300", allow_job: true },
  getReceipt: { scope: { agent: "a", user: "u", session: "s" }, operation: "forget", idempotency_key: "k1" },
  getErasure: { scope: { agent: "a" }, job_id: "j1" },
  resumeErasure: { ...FULL_MUTATION, job_id: "j1", revision: "8", generation: "1", resume_token: "opaque1" },
};

test("memory: every request field of contract section 8 is accepted and sent unchanged", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    for (const method of MEMORY_METHODS) {
      reply({ body: ACCEPTED[method] });
      await memory[method](structuredClone(FULL[method]));
      assert.deepEqual(calls.at(-1).body, FULL[method], method);
      assert.equal(calls.at(-1).raw, JSON.stringify(FULL[method]), `${method}: no member added, dropped or reordered`);
    }
    for (const target of [{ series: { id: "s", revision: "1" } }, { entity: { id: "e", revision: "1" } }, { session: true }, { scope: true }]) {
      reply({ body: ACCEPTED.forget });
      await memory.forget({ ...FULL.forget, target });
      assert.deepEqual(calls.at(-1).body.target, target);
    }
  });
});

const OBJECT_PATHS = {
  remember: ["", "scope", "items[0]", "items[0].source", "items[0].provenance[0]", "items[0].references[0]", "items[0].evidence[0]",
    "items[0].assertion", "items[0].assertion.interval", "evidence[0]"],
  recall: ["", "scope", "scopes[0]", "filter", "filter.all[0]", "filter.any[0]", "ranking", "limits", "projection"],
  findFacts: ["", "scope", "slot", "limits", "projection"],
  correctFacts: ["", "scope", "slot", "series", "interval", "correction_interval", "replacement", "replacement.source", "evidence[0]"],
  consolidate: ["", "scope", "inputs[0]", "output", "output.source", "evidence[0]"],
  forget: ["", "scope", "target.ids[0]"],
  getReceipt: ["", "scope"], getErasure: ["", "scope"], resumeErasure: ["", "scope"],
};
function withValue(request, path, value) {
  const copy = structuredClone(request);
  const dot = path.lastIndexOf(".");
  (dot === -1 ? copy : at(copy, path.slice(0, dot)))[path.slice(dot + 1)] = value;
  return copy;
}
function without(request, path) {
  const copy = structuredClone(request);
  const dot = path.lastIndexOf(".");
  delete (dot === -1 ? copy : at(copy, path.slice(0, dot)))[path.slice(dot + 1)];
  return copy;
}
// Every object property (never an array index) of a request, as a path from the request root.
function* propertyPaths(value, prefix = "") {
  if (Array.isArray(value)) {
    for (let index = 0; index < value.length; index++) yield* propertyPaths(value[index], `${prefix}[${index}]`);
  } else if (value !== null && typeof value === "object") {
    for (const [key, child] of Object.entries(value)) {
      const path = prefix === "" ? key : `${prefix}.${key}`;
      yield path;
      yield* propertyPaths(child, path);
    }
  }
}

test("memory: unknown request fields are never sent, at any depth", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    for (const method of MEMORY_METHODS) {
      for (const path of OBJECT_PATHS[method]) {
        const prefix = path === "" ? "request" : `request.${path}`;
        await refuses(() => memory[method](withValue(FULL[method], path === "" ? "extra" : `${path}.extra`, 1)),
          new RegExp(`^${escaped(prefix)}\\.extra is not a field of this request`));
      }
    }
    // Names the contract rules out: client tenant, header-style aliases, camelCase spellings, old validity names, transport options.
    for (const name of ["tenant", "binding", "receipt_window_ms", "created_at_ms", "Idempotency-Key", "idempotencyKey", "deadlineMs",
      "signal", "timeoutMs", "dryRun", "headers", "__proto__", "constructor"]) {
      await refuses(() => memory.remember(Object.defineProperty({ ...FULL.remember }, name, { value: 1, enumerable: true })),
        new RegExp(`^request\\.${escaped(name)} is not a field`));
    }
    for (const method of MEMORY_METHODS) {
      await refuses(() => memory[method](withValue(FULL[method], "scope.tenant", "@root")), /^request\.scope\.tenant is not a field/);
    }
    for (const old of ["from_ms", "to_ms", "valid_from", "valid_to"]) {
      await refuses(() => memory.correctFacts(withValue(FULL.correctFacts, `interval.${old}`, "0")), new RegExp(`^request\\.interval\\.${old} is not a field`));
    }
    assert.equal(calls.length, 0);
    // An undefined member is simply absent, whatever its name.
    reply({ body: ACCEPTED.recall });
    await memory.recall({ ...FULL.recall, extra: undefined, limits: { ...LIMITS_ALL, surplus: undefined } });
    assert.deepEqual(calls.at(-1).body, FULL.recall);
  });
});

const itemRequired = (prefix) => ["id", "kind", "content", "observed_at_ms", "source", "source.source_id", "source.source_kind",
  "provenance[0].source_id", "provenance[0].source_kind", "references[0].id", "references[0].revision", "evidence[0].collection",
  "evidence[0].record", "assertion.subject_id", "assertion.predicate", "assertion.slot_key", "assertion.destination_id",
  "assertion.interval", "assertion.slot_expected", "assertion.interval.valid_from_ms", "assertion.interval.valid_to_ms",
  "metadata.m.value"].map((path) => `${prefix}.${path}`);
const evidenceRequired = (prefix) => [`${prefix}.collection`, `${prefix}.record`, `${prefix}.metadata.n.value`];
const mutationRequired = ["scope", "scope.agent", "idempotency_key"];
const slotRequired = (prefix) => ["subject_id", "predicate", "slot_key"].map((key) => `${prefix}.${key}`);
const REQUIRED = {
  remember: [...mutationRequired, "items", ...itemRequired("items[0]"), ...evidenceRequired("evidence[0]")],
  recall: ["scope", "scope.agent", "scopes[0].agent", "filter.all[0].field", "filter.all[0].op", "filter.all[0].value.value",
    "filter.any[0].field", "filter.any[0].op"],
  findFacts: ["scope", "scope.agent", "slot", ...slotRequired("slot")],
  correctFacts: [...mutationRequired, "slot", ...slotRequired("slot"), "series", "series.id", "series.revision", "slot_revision", "interval",
    "interval.valid_from_ms", "interval.valid_to_ms", "action", "correction_interval.valid_from_ms", "correction_interval.valid_to_ms",
    ...itemRequired("replacement"), ...evidenceRequired("evidence[0]")],
  consolidate: [...mutationRequired, "inputs", "inputs[0].id", "inputs[0].revision", "output", ...itemRequired("output"), ...evidenceRequired("evidence[0]")],
  forget: [...mutationRequired, "target", "target.ids[0].id", "target.ids[0].revision", "mode"],
  getReceipt: ["scope", "scope.agent", "operation", "idempotency_key"],
  getErasure: ["scope", "scope.agent", "job_id"],
  resumeErasure: [...mutationRequired, "job_id", "revision", "generation", "resume_token"],
};
// Members of a union are checked by the union tests: they are neither required nor optional.
const UNION_MEMBER = /(\.expected|\.slot_expected)\.(absent|revision)$|^target\.(ids|series|entity|session|scope)$|\.type$/;

test("memory: undefined is absent: optional members are omitted and required members are refused", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    let optional = 0;
    let required = 0;
    for (const method of MEMORY_METHODS) {
      const mandatory = new Set(REQUIRED[method]);
      for (const path of propertyPaths(FULL[method])) {
        if (UNION_MEMBER.test(path)) continue;
        const request = withValue(FULL[method], path, undefined);
        if (mandatory.has(path)) {
          const before = calls.length;
          await refuses(() => memory[method](request), new RegExp(`^request\\.${escaped(path)} is required`));
          assert.equal(calls.length, before, `${method} ${path}`);
          required += 1;
        } else {
          reply({ body: ACCEPTED[method] });
          await memory[method](request);
          assert.deepEqual(calls.at(-1).body, without(FULL[method], path), `${method} ${path} is omitted, not nulled`);
          optional += 1;
        }
      }
      for (const path of mandatory) assert.ok([...propertyPaths(FULL[method])].includes(path), `${method}: stale required path ${path}`);
    }
    assert.ok(optional >= 100 && required >= 100, `${optional} optional and ${required} required members checked`);
  });
});

test("memory: null is refused everywhere except the nullable interval bounds", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    let nullable = 0;
    let refused = 0;
    for (const method of MEMORY_METHODS) {
      for (const path of propertyPaths(FULL[method])) {
        const request = withValue(FULL[method], path, null);
        if (/valid_(from|to)_ms$/.test(path)) {
          reply({ body: ACCEPTED[method] });
          await memory[method](request);
          assert.equal(at(calls.at(-1).body, path), null, `${method} ${path}`);
          assert.deepEqual(calls.at(-1).body, request);
          nullable += 1;
        } else {
          const before = calls.length;
          await refuses(() => memory[method](request), new RegExp(`^request\\.${escaped(path)} must be`));
          assert.equal(calls.length, before, `${method} ${path}`);
          refused += 1;
        }
      }
    }
    assert.ok(nullable >= 5 && refused >= 200, `${nullable} nullable and ${refused} refused members checked`);
  });
});

function setAt(request, path, value) {
  const copy = structuredClone(request);
  const index = /\[(\d+)\]$/.exec(path);
  const split = index ? index.index : path.lastIndexOf(".");
  (split === -1 ? copy : at(copy, path.slice(0, split)))[index ? index[1] : path.slice(split + 1)] = value;
  return copy;
}
const isInteger = (path, request) => {
  const dot = path.lastIndexOf(".");
  const key = path.slice(dot + 1);
  return U64_KEYS.has(key) || I64_KEYS.has(key) || (key === "value" && at(request, path.slice(0, dot)).type === "integer");
};
const wrongTypes = (value, integer) => Array.isArray(value) ? [{}, "x", 7, true]
  : value !== null && typeof value === "object" ? [[], "x", 7, true]
    : integer ? [true, {}, []]
      : typeof value === "boolean" ? ["true", 1, {}, []]
        : typeof value === "number" ? ["1", true, {}, []] : [7, true, {}, []];

test("memory: a wrong JSON type is refused at every member of every request", async () => {
  await memoryServer(async ({ client, calls }) => {
    const memory = client.memory("notes");
    let refused = 0;
    for (const method of MEMORY_METHODS) {
      for (const path of propertyPaths(FULL[method])) {
        const value = at(FULL[method], path);
        const targets = [[path, value, isInteger(path, FULL[method])]];
        // The first element of an array of strings or numbers stands for all of them.
        if (Array.isArray(value) && value.length > 0 && typeof value[0] !== "object") targets.push([`${path}[0]`, value[0], false]);
        for (const [target, original, integer] of targets) {
          for (const wrong of wrongTypes(original, integer)) {
            await refuses(() => memory[method](setAt(FULL[method], target, wrong)), new RegExp(`^request\\.${escaped(target)} must be`));
            refused += 1;
          }
        }
      }
    }
    assert.equal(calls.length, 0);
    assert.ok(refused >= 700, `${refused} wrong-typed members refused`);
  });
});

test("memory: no automatic idempotency key, and the caller's key and request are never altered", async () => {
  const frozen = (value) => {
    if (value !== null && typeof value === "object") Object.values(value).forEach(frozen);
    return Object.freeze(value);
  };
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    for (const method of ["remember", "correctFacts", "consolidate", "forget", "resumeErasure"]) {
      const request = withValue(FULL[method], "idempotency_key", undefined);
      await refuses(() => memory[method](request), /^request\.idempotency_key is required/);
    }
    assert.equal(calls.length, 0);
    for (let attempt = 0; attempt < 2; attempt++) {
      reply({ body: ACCEPTED.remember });
      await memory.remember(frozen(structuredClone(FULL.remember)));
    }
    assert.deepEqual(calls.map((call) => call.body.idempotency_key), ["k1", "k1"], "an identical key is sent each time, never regenerated");
    assert.equal(calls[0].raw, calls[1].raw);
    for (const method of MEMORY_METHODS) {
      reply({ body: ACCEPTED[method] });
      await memory[method](frozen(structuredClone(FULL[method])));
    }
    assert.equal(calls.length, 2 + MEMORY_METHODS.length);
  });
});

// Every plain object of a response gets a member the client cannot know: it must neither break parsing nor vanish.
function withUnknownMembers(value) {
  if (Array.isArray(value)) return value.map(withUnknownMembers);
  if (value === null || typeof value !== "object") return value;
  return { ...Object.fromEntries(Object.entries(value).map(([key, child]) => [key, withUnknownMembers(child)])),
    future_member: { note: ["x", 1, null] } };
}

test("memory: every fixture response is returned whole, including HTTP 202 and unknown members", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    let checked = 0;
    for (const [name, method] of MEMORY_OPERATIONS) {
      for (const success of successesOf(name)) {
        for (const body of [success.value, withUnknownMembers(success.value)]) {
          reply({ status: success.status, body });
          assert.deepEqual(await memory[method](structuredClone(FULL[method])), body, `${name} ${success.name}`);
          checked += 1;
        }
      }
    }
    assert.equal(calls.length, checked);
    assert.deepEqual(successesOf("resume_erasure").map((s) => s.status), [202], "the pending fixture is HTTP 202");
    assert.ok(checked >= 20);
  });
});

// Response-direction structural vectors: schema -> [facade method, how the vector sits inside an accepted response].
const RESPONSE_EMBEDDINGS = {
  MutationResponse: ["remember", (_, v) => v], ReceiptResponse: ["getReceipt", (_, v) => v], ErasureResponse: ["getErasure", (_, v) => v],
  PlanResponse: ["consolidate", (_, v) => v], RecallResponse: ["recall", (_, v) => v], FactsResponse: ["findFacts", (_, v) => v],
  Receipt: ["remember", (base, v) => ({ ...base, receipt: v })], Outcome: ["remember", (base, v) => ({ ...base, outcomes: [v] })],
  Feed: ["remember", (base, v) => ({ ...base, feed: v })], ErasureNotice: ["forget", (base, v) => ({ ...base, erasure_notice: v })],
  Erasure: ["resumeErasure", (base, v) => ({ ...base, erasure: v })], View: ["recall", (base, v) => ({ ...base, memories: [v] })],
  ReadMeta: ["recall", (base, v) => ({ ...base, meta: v })], FactView: ["findFacts", (base, v) => ({ ...base, facts: [v] })],
  ErrorDetail: ["consolidate", (base, v) => ({ ...base, conflicts: [v] })],
};

test("memory: component fixtures parse inside full responses; the oracle's closed-object defect is an ignorable unknown member", async () => {
  await memoryServer(async ({ client, reply }) => {
    const memory = client.memory("notes");
    const seen = { valid: 0, unknownMember: 0, missingEnvelope: 0 };
    for (const [name] of MEMORY_OPERATIONS) {
      for (const vector of wire[name].structural) {
        const embedding = vector.request === false ? RESPONSE_EMBEDDINGS[vector.schema] : undefined;
        if (embedding === undefined) continue;
        const [method, embed] = embedding;
        const body = embed(ACCEPTED[method], expand(structuredClone(vector.value)));
        const kind = vector.valid ? "valid" : /^closed-object-/.test(vector.name) ? "unknownMember"
          : vector.schema === "MutationResponse" && /^required-MutationResponse-/.test(vector.name) ? "missingEnvelope" : undefined;
        if (kind === undefined) continue; // other nested defects: the contract does not say how a client reacts
        reply({ body });
        if (kind === "missingEnvelope") await assert.rejects(memory[method](structuredClone(FULL[method])), TransportError, vector.name);
        else assert.deepEqual(await memory[method](structuredClone(FULL[method])), body, vector.name);
        seen[kind] += 1;
      }
    }
    assert.ok(seen.valid >= 20 && seen.unknownMember >= 10 && seen.missingEnvelope === 5, JSON.stringify(seen));
  });
});

test("memory: maximum u64 and extreme i64 outputs stay canonical decimal strings", async () => {
  const maxU = "18446744073709551615";
  const minI = "-9223372036854775808";
  const maxI = "9223372036854775807";
  const unsafe = "9007199254740993";
  const remember = structuredClone(ACCEPTED.remember);
  Object.assign(remember.receipt, { sequence: maxU, retry_until_ms: unsafe });
  remember.outcomes = [{ id: "e1", action: "created", revision: maxU, expires_at_ms: unsafe }];
  remember.remaining_work = maxU;
  const recall = structuredClone(ACCEPTED.recall);
  Object.assign(recall.memories[0], { revision: maxU, observed_at_ms: minI, created_at_ms: unsafe, updated_at_ms: maxU, expires_at_ms: "0",
    segment: { valid_from_ms: minI, valid_to_ms: maxI } });
  Object.assign(recall.meta, { as_of_ms: "-1", ttl_clock_ms: maxU, sequence: maxU, candidates_examined: unsafe });
  const facts = structuredClone(ACCEPTED.findFacts);
  Object.assign(facts.facts[0], { slot_revision: maxU, series: { id: "f1", revision: unsafe }, interval: { valid_from_ms: minI, valid_to_ms: maxI } });
  const erasure = structuredClone(ACCEPTED.getErasure);
  Object.assign(erasure.erasure, { revision: maxU, generation: unsafe, deleted_count: maxU, remaining_count: unsafe });
  const plan = { ...structuredClone(ACCEPTED.consolidate), user_operations: maxU, encoded_bytes: unsafe, remaining_work: maxU };
  await memoryServer(async ({ client, reply }) => {
    const memory = client.memory("notes");
    for (const [method, body] of [["remember", remember], ["recall", recall], ["findFacts", facts], ["getErasure", erasure], ["consolidate", plan]]) {
      reply({ text: JSON.stringify(body) });
      const result = await memory[method](structuredClone(FULL[method]));
      assert.deepEqual(result, body, method);
    }
    reply({ text: JSON.stringify(remember) });
    const { receipt, outcomes, remaining_work: remaining } = await memory.remember(structuredClone(FULL.remember));
    for (const value of [receipt.sequence, receipt.retry_until_ms, outcomes[0].revision, outcomes[0].expires_at_ms, remaining]) {
      assert.equal(typeof value, "string", "never a Number or a bigint");
    }
    assert.equal(receipt.sequence, maxU);
  });
});

test("memory: nulls and absent optional members of a response are preserved as they arrived", async () => {
  await memoryServer(async ({ client, reply }) => {
    const memory = client.memory("notes");
    reply({ body: ACCEPTED.recall });
    const recall = await memory.recall(structuredClone(FULL.recall));
    assert.equal(recall.memories[0].segment, null);
    assert.equal(recall.memories[0].scores.D, null);
    assert.equal(Object.hasOwn(recall.memories[0], "confidence"), false);
    assert.equal(Object.hasOwn(recall.memories[0].scores, "distance"), false);
    reply({ body: ACCEPTED.getErasure });
    const erasure = await memory.getErasure(structuredClone(FULL.getErasure));
    assert.equal(erasure.erasure.remaining_count, null);
    reply({ body: ACCEPTED.resumeErasure });
    const resumed = await memory.resumeErasure(structuredClone(FULL.resumeErasure));
    assert.equal(resumed.remaining_work, null);
    assert.equal(Object.hasOwn(resumed.erasure, "resume_token"), true);
    const noToken = structuredClone(ACCEPTED.resumeErasure);
    delete noToken.erasure.resume_token;
    delete noToken.erasure_notice.manifest_id;
    delete noToken.receipt.retry_until_ms;
    reply({ body: noToken });
    const finished = await memory.resumeErasure(structuredClone(FULL.resumeErasure));
    assert.deepEqual(finished, noToken);
    for (const key of ["resume_token"]) assert.equal(Object.hasOwn(finished.erasure, key), false);
    assert.equal(Object.hasOwn(finished.erasure_notice, "manifest_id"), false);
    assert.equal(Object.hasOwn(finished.receipt, "retry_until_ms"), false);
  });
});

test("memory: consolidate returns either variant of MemoryConsolidateResponse, told apart by dry_run", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    const plan = ACCEPTED.consolidate;
    const committed = structuredClone(ACCEPTED.remember);
    committed.receipt.operation = "consolidate";
    reply({ body: plan }, { body: committed });
    const dry = await memory.consolidate({ ...FULL.consolidate, dry_run: true });
    assert.deepEqual(dry, plan);
    assert.equal(dry.dry_run, true);
    const done = await memory.consolidate({ ...FULL.consolidate, dry_run: undefined });
    assert.deepEqual(done, committed);
    assert.equal(Object.hasOwn(done, "dry_run"), false);
    assert.equal(done.receipt.operation, "consolidate");
    assert.equal(calls[0].body.dry_run, true);
    assert.equal(Object.hasOwn(calls[1].body, "dry_run"), false, "dry_run defaults on the server; the client never sets it");
  });
});

test("memory: erasure targets and typed metadata scalars in a response keep every union variant", async () => {
  await memoryServer(async ({ client, reply }) => {
    const memory = client.memory("notes");
    for (const target of [{ ids: [{ id: "a", revision: "1" }, { id: "b", revision: "18446744073709551615" }] },
      { series: { id: "s", revision: "3" } }, { entity: { id: "e", revision: "4" } }, { session: true }, { scope: true }]) {
      const body = structuredClone(ACCEPTED.getErasure);
      body.erasure.target = target;
      reply({ body });
      assert.deepEqual((await memory.getErasure(structuredClone(FULL.getErasure))).erasure.target, target);
    }
    const body = structuredClone(ACCEPTED.recall);
    body.memories[0].metadata = { s: { type: "string", value: "x" }, i: { type: "integer", value: "-9223372036854775808" },
      f: { type: "float", value: 1.5 }, b: { type: "boolean", value: false } };
    reply({ body });
    assert.deepEqual((await memory.recall(structuredClone(FULL.recall))).memories[0].metadata, body.memories[0].metadata);
  });
});

test("memory: forget and resumeErasure treat HTTP 202 as an ordinary pending response", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    const pending = structuredClone(ACCEPTED.resumeErasure);
    pending.receipt.operation = "forget";
    reply({ status: 202, body: pending }, { status: 202, body: ACCEPTED.resumeErasure }, { status: 200, body: ACCEPTED.forget });
    const accepted = await memory.forget(structuredClone(FULL.forget));
    assert.deepEqual(accepted, pending);
    assert.equal(accepted.state, "pending");
    assert.equal(accepted.erasure.state, "pending");
    const resumed = await memory.resumeErasure(structuredClone(FULL.resumeErasure));
    assert.equal(resumed.state, "pending");
    assert.equal(resumed.receipt.sequence, "9");
    assert.equal((await memory.forget(structuredClone(FULL.forget))).state, "logical");
    assert.equal(calls.length, 3, "an accepted request is never re-sent, resumed or polled");
  });
});

test("memory: every error code of the registry carries status, code, request id and message on each method", async () => {
  const registry = wire.remember.structural.filter((s) => s.schema === "exchange" && s.valid && s.status >= 400);
  assert.ok(registry.length >= 30);
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    let serial = 0;
    for (const method of MEMORY_METHODS) {
      for (const failed of registry) {
        const before = calls.length;
        serial += 1;
        reply({ status: failed.status, body: failed.value, headers: { "X-Request-ID": `req-${serial}` } });
        const error = await failure(memory[method](structuredClone(FULL[method])), `${method} ${failed.name}`);
        const label = `${method} ${failed.name}`;
        assert.ok(error instanceof VLiteError && !(error instanceof UnsupportedError), label);
        assert.equal(error.name, "VLiteError", label);
        assert.equal(error.status, failed.status, label);
        assert.equal(error.code, failed.value.error.code, label);
        assert.equal(error.requestId, `req-${serial}`, label);
        assert.equal(error.message, `HTTP ${failed.status}: ${error.code}: ${failed.value.error.message}`, label);
        assert.equal(Object.hasOwn(error, "details"), Object.hasOwn(failed.value.error, "details"), label);
        assert.equal(calls.length, before + 1, `${label}: one attempt, no retry`);
      }
    }
  });
});

test("memory: structured error details reach the caller unchanged", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    const cases = [
      ["findFacts", 409, "memory_capability_unavailable", { feature: "facts" }],
      ["correctFacts", 409, "memory_capability_unavailable", { feature: "facts", category: "temporal" }],
      ["getErasure", 409, "memory_capability_unavailable", { feature: "erasure_jobs" }],
      ["remember", 409, "memory_deletion_pending", { field: "scope" }],
      ["forget", 409, "memory_deletion_pending", {}],
      ["remember", 412, "precondition_failed", { field: "revision" }],
      ["consolidate", 412, "precondition_failed", { field: "inputs" }],
      ["recall", 422, "invalid_input", { field: "limit", limit: "100", requested: "101" }],
      ["remember", 422, "invalid_input", { field: "items", limit: "4096", requested: "18446744073709551615" }],
      ["recall", 504, "deadline_exceeded", { field: "deadline_ms", limit: "250" }],
      ["resumeErasure", 409, "memory_job_reconciliation_required", { field: "generation", extra_future_key: [1, "2"] }],
    ];
    for (const [method, status, code, details] of cases) {
      reply({ status, headers: { "X-Request-ID": "req-d" }, body: { error: { code, message: "sanitized", details } } });
      const error = await failure(memory[method](structuredClone(FULL[method])), `${method} ${code}`);
      assert.ok(error instanceof VLiteError, `${method} ${code}`);
      assert.equal(error.status, status);
      assert.equal(error.code, code);
      assert.equal(error.requestId, "req-d");
      assert.deepEqual(error.details, details, `${method} ${code}`);
    }
    // A malformed details member is ignored, never promoted to something else.
    for (const details of [null, "feature=facts", ["facts"], 7, true]) {
      reply({ status: 409, body: { error: { code: "memory_capability_unavailable", message: "m", details } } });
      const error = await failure(memory.recall(structuredClone(FULL.recall)), `details ${JSON.stringify(details)}`);
      assert.ok(error instanceof VLiteError && error.code === "memory_capability_unavailable");
      assert.equal(Object.hasOwn(error, "details"), false, JSON.stringify(details));
    }
    assert.equal(calls.length, cases.length + 5);
  });
});

test("memory: nonstandard failures keep an explicit code and a null request id", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    for (const [scripted, code, message] of [
      [{ status: 502, type: "text/html", text: "<html>bad gateway</html>" }, "http_error", /nonstandard error/],
      [{ status: 503, text: "" }, "http_error", /nonstandard error/],
      [{ status: 500, body: {} }, "http_error", /nonstandard error/],
      [{ status: 500, body: { error: { message: "only a message" } } }, "http_error", /only a message/],
      [{ status: 429, headers: { "Retry-After": "1" }, body: { error: { code: "rate_limited", message: "slow down" } } }, "rate_limited", /slow down/]]) {
      reply(scripted);
      const error = await failure(memory.remember(structuredClone(FULL.remember)), code);
      assert.ok(error instanceof VLiteError, code);
      assert.equal(error.code, code);
      assert.equal(error.requestId, null);
      assert.match(error.message, message);
      assert.equal(Object.hasOwn(error, "details"), false);
    }
    assert.equal(calls.length, 5, "a 429 is surfaced once; no automatic retry or queue");
  });
});

test("memory: only 404 not_found means the server lacks the feature; every other refusal stays an HTTP error", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    for (const method of MEMORY_METHODS) {
      const before = calls.length;
      reply({ ...err(404, "not_found"), headers: { "X-Request-ID": "old-server" } });
      await assert.rejects(memory[method](structuredClone(FULL[method])), (e) => e instanceof UnsupportedError && !(e instanceof VLiteError)
        && /memory routes/.test(e.message) && /no fallback/.test(e.message), method);
      assert.equal(calls.length, before + 1, "no fallback request");
      for (const [status, code] of [[404, "memory_not_found"], [404, "receipt_not_found"], [404, "erasure_not_found"], [404, "kv_not_found"],
        [404, "collection_not_found"], [404, "NOT_FOUND"], [404, "not_found "], [409, "not_found"], [400, "not_found"], [405, "not_found"],
        [500, "not_found"], [409, "memory_not_configured"]]) {
        reply(err(status, code));
        await assert.rejects(memory[method](structuredClone(FULL[method])), (e) => e instanceof VLiteError && !(e instanceof UnsupportedError)
          && e.status === status && e.code === code, `${method} ${status} ${code}`);
      }
      for (const raw of [{ status: 404 }, { status: 404, text: "Not Found", type: "text/plain" }, { status: 404, body: { error: {} } },
        { status: 404, body: { code: "not_found" } }]) {
        reply(raw);
        await assert.rejects(memory[method](structuredClone(FULL[method])), (e) => e instanceof VLiteError && e.code === "http_error", method);
      }
    }
  });
});

test("memory: transport failures stay TransportError, never become success, and are never retried", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    for (const method of MEMORY_METHODS) {
      const before = calls.length;
      reply({ destroy: true });
      await assert.rejects(memory[method](structuredClone(FULL[method])), TransportError, `${method}: connection lost after admission`);
      assert.equal(calls.length, before + 1, `${method}: a lost mutation outcome is never retried`);
      for (const reply2 of [{ text: "not json" }, { text: "" }, { text: "null" }, { text: "[]" }, { text: "\"x\"" }, { text: "7" },
        { text: "{}" }, { status: 204 }, { status: 302, headers: { Location: "/v1/memory/notes/stolen" } }]) {
        const length = calls.length;
        reply(reply2);
        await assert.rejects(memory[method](structuredClone(FULL[method])), (e) => e instanceof TransportError && !(e instanceof VLiteError),
          `${method} ${JSON.stringify(reply2)}`);
        assert.equal(calls.length, length + 1, "a redirect target is never contacted");
      }
    }
    assert.ok(calls.every((call) => !call.path.endsWith("/stolen")));
  });
});

// Top-level members each success response must carry (contract section 8), by JSON type.
const IS = {
  object: (v) => typeof v === "object" && v !== null && !Array.isArray(v), array: Array.isArray, boolean: (v) => typeof v === "boolean",
  string: (v) => typeof v === "string", nullableString: (v) => v === null || typeof v === "string",
};
const MUTATION_RESPONSE = { receipt: "object", replayed: "boolean", outcomes: "array", feed: "object", remaining_work: "nullableString" };
const RESPONSE_MEMBERS = {
  remember: MUTATION_RESPONSE, correctFacts: MUTATION_RESPONSE, forget: MUTATION_RESPONSE, resumeErasure: MUTATION_RESPONSE,
  recall: { memories: "array", meta: "object", ranking_version: "string", selection: "string" },
  findFacts: { facts: "array", meta: "object" },
  getErasure: { erasure: "object", erasure_notice: "object", feed: "object" },
  getReceipt: { result: "object" },
  consolidate: { dry_run: "dryRun", user_operations: "string", encoded_bytes: "string", conflicts: "array", remaining_work: "nullableString" },
};
const WRONG_SAMPLES = [undefined, null, 7, "x", [], {}, true, "false"];

const typeOk = (kind, value) => kind === "dryRun" ? value === true : IS[kind](value);

test("memory: a success response missing a required envelope member or mistyping it is a transport error", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    const committed = structuredClone(ACCEPTED.remember);
    committed.receipt.operation = "consolidate";
    // [method, accepted body, members to break, wrapper placing the body in a response]
    const subjects = [
      ...MEMORY_METHODS.map((method) => [method, ACCEPTED[method], RESPONSE_MEMBERS[method], (body) => body]),
      ["getReceipt", ACCEPTED.getReceipt.result, MUTATION_RESPONSE, (body) => ({ result: body })],
      ["consolidate", committed, MUTATION_RESPONSE, (body) => body],
    ];
    let rejected = 0;
    for (const [method, accepted, members, wrap] of subjects) {
      for (const [member, kind] of Object.entries(members)) {
        for (const wrong of WRONG_SAMPLES) {
          if (typeOk(kind, wrong)) continue;
          const body = structuredClone(accepted);
          if (wrong === undefined) delete body[member]; else body[member] = wrong;
          reply({ text: JSON.stringify(wrap(body)) });
          const error = await failure(memory[method](structuredClone(FULL[method])), `${method}.${member} = ${JSON.stringify(wrong)}`);
          assert.ok(error instanceof TransportError && !(error instanceof VLiteError), `${method}.${member} = ${JSON.stringify(wrong)}`);
          rejected += 1;
        }
      }
    }
    assert.equal(calls.length, rejected, "each malformed response was received once and never retried");
    assert.ok(rejected >= 100, `${rejected} malformed responses rejected`);
    reply({ body: committed });
    assert.deepEqual(await memory.consolidate(structuredClone(FULL.consolidate)), committed);
  });
});

test("memory: cancellation and timeout are transport ambiguity: the signal is honored and nothing is retried", async (t) => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    for (const method of MEMORY_METHODS) {
      const before = calls.length;
      const error = await failure(memory[method](structuredClone(FULL[method]), { signal: AbortSignal.abort() }));
      assert.ok(error instanceof TransportError && !(error instanceof VLiteError), `${method}: already aborted`);
      assert.equal(calls.length, before, `${method}: an aborted call sends nothing`);
    }
    for (const options of [undefined, {}, { signal: undefined }]) {
      reply({ body: ACCEPTED.recall });
      await memory.recall(structuredClone(FULL.recall), options);
    }
    assert.equal(calls.length, 3);
    for (const bad of [null, "signal", [], 1, { signal: null }, { signal: "x" }, { signal: {} }, { signal: () => {} }, { timeoutMs: 1 },
      { signal: AbortSignal.abort(), extra: 1 }, { Signal: AbortSignal.abort() }]) {
      await refuses(() => memory.recall(structuredClone(FULL.recall), bad), /options/);
    }
    assert.equal(calls.length, 3, "invalid options are refused before sending");
  });
  // The caller aborts after the server admitted the request: one attempt per method, never repeated.
  let controller;
  let admitted = 0;
  const abortingServer = createServer(async (req, res) => {
    for await (const _ of req);
    admitted += 1;
    controller.abort();
    res.end(JSON.stringify(ACCEPTED.remember));
  });
  await new Promise((resolve) => abortingServer.listen(0, "127.0.0.1", resolve));
  try {
    const memory = new Client(`http://127.0.0.1:${abortingServer.address().port}`, token).memory("notes");
    for (const method of MEMORY_METHODS) {
      controller = new AbortController();
      const before = admitted;
      const error = await failure(memory[method](structuredClone(FULL[method]), { signal: controller.signal }));
      assert.ok(error instanceof TransportError && !(error instanceof VLiteError), method);
      assert.equal(admitted, before + 1, `${method}: sent once and not repeated`);
    }
  } finally {
    abortingServer.closeAllConnections();
    await new Promise((resolve) => abortingServer.close(resolve));
  }
  // The client's own timeout still applies when the caller passes a signal.
  const timeout = new AbortController();
  const caller = new AbortController();
  let served = 0;
  const timeoutServer = createServer(async (req, res) => {
    for await (const _ of req);
    served += 1;
    timeout.abort(new DOMException("Timed out", "TimeoutError"));
    res.end(JSON.stringify(ACCEPTED.forget));
  });
  await new Promise((resolve) => timeoutServer.listen(0, "127.0.0.1", resolve));
  t.mock.method(AbortSignal, "timeout", (milliseconds) => {
    assert.equal(milliseconds, 100);
    return timeout.signal;
  });
  try {
    const memory = new Client(`http://127.0.0.1:${timeoutServer.address().port}`, token, { timeoutMs: 100 }).memory("notes");
    const error = await failure(memory.forget(structuredClone(FULL.forget), { signal: caller.signal }));
    assert.ok(error instanceof TransportError);
    assert.equal(served, 1);
    assert.equal(caller.signal.aborted, false, "the caller's signal is untouched by the client timeout");
  } finally {
    timeoutServer.closeAllConnections();
    await new Promise((resolve) => timeoutServer.close(resolve));
  }
});

test("memory: deadline_ms is the engine deadline and never the HTTP timeout", async (t) => {
  t.mock.method(AbortSignal, "timeout");
  await memoryServer(async ({ client, base, calls, reply }) => {
    const memory = client.memory("notes");
    reply({ delayMs: 80, body: ACCEPTED.remember });
    const result = await memory.remember({ ...FULL.remember, deadline_ms: 1 });
    assert.deepEqual(result, ACCEPTED.remember, "a slow answer to deadline_ms 1 is still a success");
    assert.equal(calls.at(-1).body.deadline_ms, 1);
    assert.match(calls.at(-1).raw, /"deadline_ms":1[,}]/);
    reply({ body: ACCEPTED.recall });
    await memory.recall({ ...FULL.recall, limits: { ...LIMITS_ALL, deadline_ms: 5000 } });
    reply({ body: ACCEPTED.forget });
    await new Client(base, token, { timeoutMs: 1234 }).memory("notes").forget({ ...FULL.forget, deadline_ms: 7 });
    assert.deepEqual(AbortSignal.timeout.mock.calls.map((call) => call.arguments[0]), [30000, 30000, 1234]);
    assert.equal(calls.at(-1).body.deadline_ms, 7);
    // The client neither supplies nor edits the engine deadline when the caller omits it.
    reply({ body: ACCEPTED.remember });
    await memory.remember(withValue(FULL.remember, "deadline_ms", undefined));
    assert.equal(Object.hasOwn(calls.at(-1).body, "deadline_ms"), false);
  });
});

test("memory: no automatic embedding, splitting, paging, provider policy or retry", async () => {
  await memoryServer(async ({ client, calls, reply }) => {
    const memory = client.memory("notes");
    reply({ body: ACCEPTED.recall });
    await memory.recall({ scope, text: "Ada" });
    assert.equal(calls.length, 1, "lexical recall performs no embedding call");
    assert.deepEqual(calls[0].body, { scope, text: "Ada" });
    reply({ body: ACCEPTED.recall });
    await memory.recall({ scope, text: "Ada", embedding_provider: "alias" });
    assert.deepEqual(calls[1].body, { scope, text: "Ada", embedding_provider: "alias" });
    reply({ body: ACCEPTED.recall });
    await memory.recall({ scope, vector: [0.1, 1e-7, -3.5e38] });
    assert.deepEqual(calls[2].body.vector, [0.1, 1e-7, -3.5e38], "vectors are sent as given, never rounded or embedded");
    // More items than one frame allows is the server's refusal; the client never splits the bundle.
    const items = Array.from({ length: 4097 }, (_, index) => memoryItem({ id: `e${index}` }));
    reply(err(422, "invalid_input"));
    await assert.rejects(memory.remember({ ...mutation, items }), (e) => e instanceof VLiteError && e.code === "invalid_input");
    assert.equal(calls.length, 4, "one request for the whole bundle");
    assert.equal(calls[3].body.items.length, 4097);
    // A partial answer is returned as is: there is no pagination, cursor or follow-up request.
    const partial = structuredClone(ACCEPTED.findFacts);
    Object.assign(partial.meta, { complete: false, truncation: ["output"] });
    reply({ body: partial });
    assert.deepEqual(await memory.findFacts({ scope, slot, allow_partial: true }), partial);
    assert.equal(calls.length, 5);
    assert.equal(calls.at(-1).path, "/v1/memory/notes/facts/query");
  });
});

test("memory: the facade exposes exactly the nine contract methods and nothing else", async () => {
  await memoryServer(async ({ client }) => {
    const memory = client.memory("notes");
    const names = Object.getOwnPropertyNames(Object.getPrototypeOf(memory)).filter((name) => name !== "constructor").sort();
    assert.deepEqual(names, [...MEMORY_METHODS].sort());
    assert.deepEqual(MEMORY_METHODS, ["remember", "recall", "findFacts", "correctFacts", "consolidate", "forget", "getReceipt", "getErasure", "resumeErasure"]);
  });
});

test("memory: error details are carried on every route and old error shapes are unchanged", async () => {
  await scripted(async ({ client, calls, reply }) => {
    reply({ status: 422, headers: { "X-Request-ID": "other" }, body: { error: { code: "invalid_input", message: "m", details: { field: "k", limit: "5" } } } },
      { status: 409, body: { error: { code: "index_not_ready", message: "Build first" } } },
      { status: 422, body: { error: { code: "import_rows_invalid", message: "bad", details: { field: "ignored" } }, job: "9", chunk: 2, rows_committed: 0,
        rows_skipped: 0, invalid_rows: 0, invalid: [], revision: null } });
    const detailed = await failure(client.database());
    assert.ok(detailed instanceof VLiteError);
    assert.equal(detailed.status, 422);
    assert.equal(detailed.requestId, "other");
    assert.deepEqual(detailed.details, { field: "k", limit: "5" });
    const plain = await failure(client.indexStatus());
    assert.ok(plain instanceof VLiteError && plain.code === "index_not_ready");
    assert.equal(Object.hasOwn(plain, "details"), false);
    const rejected = await failure(client.importChunk("9", "2", []));
    assert.ok(rejected instanceof ImportRejectedError && rejected.code === "import_rows_invalid");
    assert.equal(rejected.report.outcome, "rejected");
    assert.equal(Object.hasOwn(rejected, "details"), false);
    assert.equal(calls.length, 3);
  });
});
