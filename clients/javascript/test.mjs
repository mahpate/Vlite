import assert from "node:assert/strict";
import { createServer } from "node:http";
import { test } from "node:test";
import { Client, ImportRejectedError, TransportError, UnsupportedError, VLiteError } from "./index.js";
import { mkdtempSync, writeFileSync } from "node:fs";
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
      accept: req.headers.accept, ifMatch: req.headers["if-match"], auth: req.headers.authorization });
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
