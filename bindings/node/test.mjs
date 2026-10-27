import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NativeLibrary, EmbeddedError } from "./index.js";

const code = (expected) => (error) => error instanceof EmbeddedError && error.code === expected;

test("worker initialization failures reject requests and shutdown explicitly", { timeout: 30000 }, async () => {
  const native = new NativeLibrary({ addonPath: new URL("./package.json", import.meta.url) });
  await assert.rejects(native.open("unused.vlite"), (error) =>
    code("worker_failed")(error) && error.cause instanceof Error);
  await assert.rejects(native.close(), code("worker_failed"));
});

test("embedded persistence, errors, ordering, binary data and shutdown", { timeout: 60000 }, async () => {
  const directory = await mkdtemp(join(tmpdir(), "vlite-node-"));
  const native = new NativeLibrary();
  try {
    const path = join(directory, "unicode-\u03bb.vlite");
    const db = await native.create(path, 3);
    await assert.rejects(native.open(path), code("locked"));
    const metadata = { n: { type: "integer", value: "-9223372036854775808" } };
    await db.put("18446744073709551615", [1, 0, 0], { payload: Buffer.from([0, 255]), metadata });
    const revision = await db.revision("18446744073709551615");
    assert.match(revision, /^[1-9][0-9]*$/);
    assert.deepEqual(await db.get("18446744073709551615"), {
      id: "18446744073709551615", vector: [1, 0, 0], payload_base64: "AP8=", metadata, revision,
    });
    assert.equal(await db.revision("7"), null);
    assert.equal((await db.info()).format_version, 5);
    assert.equal(await db.isGraphEnabled(), true);
    await assert.rejects(db.searchApproximateReady([1, 0, 0], 1), code("index_not_ready"));
    assert.equal((await db.search([1, 0, 0], 1, { metric: "cosine" }))[0].distance, 0);
    assert.equal((await db.search([1, 0, 0], 1, { metric: "inner_product" }))[0].distance, -1);
    await assert.rejects(db.search([0, 0, 0], 0, { metric: "cosine" }), code("invalid_input"));
    await assert.rejects(db.search([1, 0, 0], 1, { metric: "invalid" }), code("invalid_input"));
    await assert.rejects(db.put("1", [NaN, 0, 0]), TypeError);
    await assert.rejects(db.get("01"), code("invalid_input"));
    await assert.rejects(db.get("0"), code("not_found"));
    await assert.rejects(db.put("1", [1, 0]), code("invalid_input"));
    await assert.rejects(db.batch([
      { op: "put", id: "2", vector: [0, 1, 0] }, { op: "put", id: "3", vector: [1] },
    ]), code("invalid_input"));
    assert.equal((await db.info()).records, 1);
    await db.batch([{ op: "put", id: "2", vector: [0, 1, 0] }, { op: "delete", id: "0" }]);
    const records = await db.getMany(["2", "0", "2"]);
    assert.equal(records[0].id, "2");
    assert.equal(records[1], null);
    assert.deepEqual(records[0], records[2]);
    assert.deepEqual(await db.search([1, 0, 0], 2, { filter: [
      { op: "eq", field: "n", value: metadata.n },
    ] }), [{ id: "18446744073709551615", distance: 0 }]);
    await db.buildIndex();
    assert.equal((await db.searchApproximateReady([1, 0, 0], 1))[0].distance, 0);
    await assert.rejects(db.searchApproximateReady([1, 0, 0], 1, { metric: "cosine" }), code("index_not_ready"));
    await db.buildIndex({ metric: "cosine" });
    assert.equal((await db.searchApproximateReady([1, 0, 0], 1, { metric: "cosine" }))[0].distance, 0);
    await assert.rejects(db.searchApproximateReady([1, 0, 0], 1), code("index_not_ready"));
    await assert.rejects(db.buildIndex({ metric: "inner_product" }), code("invalid_input"));
    await assert.rejects(db.searchApproximateReady([0, 0, 0], 0, { metric: "cosine" }), code("invalid_input"));
    assert.equal(await db.delete("2"), true);
    assert.equal(await db.delete("2"), false);
    const write = db.put("2", [0, 1, 0]);
    const close = db.close();
    await Promise.all([write, close]);
    await db.close();
    await assert.rejects(db.info(), code("closed"));
    const reopened = await native.open(path);
    assert.equal((await reopened.get("2")).id, "2");
    // Library close drains accepted writes and closes all remaining handles.
    const pending = reopened.put("3", [0, 0, 1]);
    await Promise.all([pending, native.close()]);
    await native.close();
    await assert.rejects(native.open(path), code("closed"));
    const next = new NativeLibrary();
    try {
      const db2 = await next.open(path);
      assert.equal((await db2.info()).records, 3);
    } finally { await next.close(); }
  } finally {
    await native.close();
    await rm(directory, { recursive: true });
  }
});

test("bounded queue rejects before admission and a rejected close can be retried", { timeout: 30000 }, async () => {
  const directory = await mkdtemp(join(tmpdir(), "vlite-node-busy-"));
  const native = new NativeLibrary({ maxPending: 1 });
  try {
    const db = await native.create(join(directory, "queue.vlite"), 2);
    const pending = db.info();
    await assert.rejects(db.close(), code("busy"));
    await pending;
    assert.equal((await db.info()).records, 0);
    await db.close();
  } finally {
    await native.close();
    await rm(directory, { recursive: true });
  }
});

test("native builds leave the JavaScript event loop responsive", { timeout: 60000 }, async () => {
  const directory = await mkdtemp(join(tmpdir(), "vlite-node-event-loop-"));
  const native = new NativeLibrary();
  try {
    const db = await native.create(join(directory, "event-loop.vlite"), 32);
    await db.batch(Array.from({ length: 600 }, (_, id) => ({
      op: "put", id: String(id), vector: Array.from({ length: 32 }, (_, d) => ((id * 17 + d * 13) % 101) / 101),
    })));
    let ticks = 0;
    const interval = setInterval(() => ticks++, 1);
    try { await db.buildIndex(); }
    finally { clearInterval(interval); }
    assert.ok(ticks > 0, "Main event loop must run timers while native indexing executes");
  } finally {
    await native.close();
    await rm(directory, { recursive: true });
  }
});

test("format v4 collections, revisions, planned/hybrid/quantized search and resumable imports", { timeout: 60000 }, async () => {
  const directory = await mkdtemp(join(tmpdir(), "vlite-node-v4-"));
  let native = new NativeLibrary();
  try {
    const path = join(directory, "v4.vlite");
    await assert.rejects(native.create(join(directory, "bad.vlite"), 2, { format: 6 }), code("invalid_input"));
    const legacy = await native.create(join(directory, "v3.vlite"), 2, { format: 3 });
    await assert.rejects(legacy.createCollection("docs", 2), code("requires_format_v4"));
    await legacy.close();
    let db = await native.create(path, 2, { format: 4 });
    assert.equal((await db.info()).format_version, 4);
    const created = await db.createCollection("docs", 2, {
      metadataIndexes: [{ field: "tag", kind: "eq" }], textFields: ["title"],
      hnsw: { seed: "18446744073709551615" },
    });
    assert.equal(created.name, "docs");
    assert.equal(created.live_records, 0);
    assert.equal(created.settings.hnsw.seed, "18446744073709551615");
    assert.deepEqual(created.settings.text_fields, ["title"]);
    await assert.rejects(db.createCollection("docs", 2), code("collection_exists"));
    await assert.rejects(db.createCollection("x", 2, { unknown: true }), code("invalid_input"));
    assert.deepEqual((await db.listCollections()).map((c) => c.name), ["default", "docs"]);

    // Compare-and-set writes: absent create, exact revision update, explicit failures.
    const first = await db.put("1", [1, 0], { collection: "docs", expectedRevision: "absent" });
    assert.match(first, /^[0-9]+$/);
    assert.equal(await db.revision("1", { collection: "docs" }), first);
    await assert.rejects(db.put("1", [0, 1], { collection: "docs", expectedRevision: "absent" }), code("precondition_failed"));
    const second = await db.put("1", [0, 1], { collection: "docs", expectedRevision: first });
    assert.notEqual(second, first);
    await assert.rejects(db.delete("1", { collection: "docs", expectedRevision: first }), code("precondition_failed"));
    await assert.rejects(db.put("1", [0, 1], { collection: "docs", expectedRevision: "01" }), code("invalid_input"));
    assert.equal(await db.put("1", [0, 1], { collection: "docs" }), undefined);
    assert.equal(await db.delete("1", { collection: "docs", expectedRevision: await db.revision("1", { collection: "docs" }) }), true);
    await assert.rejects(db.get("1", { collection: "missing" }), code("collection_not_found"));

    await db.batch(Array.from({ length: 40 }, (_, i) => ({
      op: "put", id: String(i), vector: [Math.cos(i / 7), Math.sin(i / 7)], metadata: {
        tag: { type: "string", value: i === 3 ? "rare" : "common" },
        title: { type: "string", value: i === 5 ? "red apple" : "green pear" },
      },
    })), { collection: "docs" });
    const exact = await db.search([1, 0], 3, { collection: "docs" });
    assert.equal(exact[0].id, "0");
    const explained = await db.search([1, 0], 3, { collection: "docs", explain: true });
    assert.deepEqual(explained.hits, exact);
    assert.equal(explained.stats.records_scanned, 40);
    const filter = [{ op: "eq", field: "tag", value: { type: "string", value: "rare" } }];
    const indexed = await db.searchAuto([1, 0], 3, { collection: "docs", filter, explain: true });
    assert.equal(indexed.plan.strategy, "exact_indexed");
    assert.equal(indexed.plan.index_field, "tag");
    assert.deepEqual(indexed.hits.map((hit) => hit.id), ["3"]);
    assert.equal((await db.searchAuto([1, 0], 3, { collection: "docs" })).plan, undefined);

    await assert.rejects(db.annMemory({ collection: "docs" }), code("index_not_ready"));
    await db.buildIndex({ collection: "docs", quantization: "scalar_u8", rerankMultiplier: 4 });
    const planned = await db.searchAuto([1, 0], 1, { collection: "docs", quantization: "scalar_u8", explain: true });
    assert.equal(planned.plan.strategy, "ann");
    assert.ok(planned.stats.quantized_distance_evaluations > 0);
    assert.equal(planned.hits[0].id, "0");
    assert.equal((await db.searchApproximateReady([1, 0], 1, { collection: "docs", efSearch: 20, quantization: "scalar_u8" }))[0].id, "0");
    const memory = await db.annMemory({ collection: "docs", quantization: "scalar_u8" });
    assert.equal(memory.code_bytes, 80);
    const quality = await db.annQuality({ collection: "docs", quantization: "scalar_u8", samples: 4, k: 3, efSearch: 40, seed: "7" });
    assert.equal(quality.samples_used, 4);
    assert.ok(quality.recall_at_k > 0.5);
    assert.equal((await db.annQuality({ collection: "docs", quantization: "scalar_u8", maxDistanceEvaluations: 1 })).skipped_reason, "budget");

    const hybrid = await db.searchHybrid("apple", 3, { collection: "docs", vector: [1, 0], explain: true });
    assert.equal(hybrid.hits[0].id, "5");
    assert.equal(hybrid.hits[0].text_rank, 1);
    assert.ok(hybrid.stats);
    const ready = await db.searchHybrid("apple", 3, { collection: "docs", vector: [1, 0], vectorLeg: "approximate_ready",
      quantization: "scalar_u8", efSearch: 60, fusion: { rrfK: 10, vectorWeight: 1, textWeight: 2 } });
    assert.equal(ready.hits.length, 3);
    await assert.rejects(db.searchHybrid("apple", 3, { efSearch: 5, collection: "docs" }), code("invalid_input"));

    assert.deepEqual(await db.setCollectionSettings("docs", { textFields: ["title"], rerankMultiplier: 8 }), { persisted: true });
    assert.equal((await db.listCollections())[1].settings.rerank_multiplier, 8);

    // Resumable import: exactly-once chunks survive close and reopen.
    const job = await db.nextImportJobId();
    const rows = [{ op: "put", id: "100", vector: [1, 1] }, { op: "put", id: "101", vector: [1, 0, 0] }];
    const rejected = await db.importChunk(job, "1", rows, { collection: "docs" });
    assert.equal(rejected.outcome, "rejected");
    assert.deepEqual(rejected.invalid.map((row) => [row.row, row.code]), [[1, "dimension_mismatch"]]);
    const committed = await db.importChunk(job, "1", rows, { collection: "docs", policy: "skip_invalid" });
    assert.equal(committed.outcome, "committed");
    assert.equal(committed.rows_committed, 1);
    assert.equal(committed.rows_skipped, 1);
    await assert.rejects(db.importChunk(job, "1", rows.slice(0, 1), { collection: "docs", policy: "skip_invalid" }), code("import_conflict"));
    await db.close();
    await native.close();

    native = new NativeLibrary();
    db = await native.open(path);
    const status = await db.importStatus(job);
    assert.equal(status.state, "open");
    assert.equal(status.collection, "docs");
    assert.equal(status.last_chunk, "1");
    assert.equal(status.last_digest.length, 64);
    assert.equal((await db.importChunk(job, "1", rows, { collection: "docs", policy: "skip_invalid" })).outcome, "already_committed");
    assert.equal((await db.importFinish(job, { collection: "docs" })).state, "finished");
    await assert.rejects(db.importAbort(job, { collection: "docs" }), code("import_conflict"));
    await assert.rejects(db.importStatus("18446744073709551615"), code("import_not_found"));
    assert.equal((await db.get("100", { collection: "docs" })).id, "100");
    assert.equal((await db.listCollections())[1].settings.rerank_multiplier, 8);
    await assert.rejects(db.dropCollection("docs"), code("collection_not_empty"));
    await db.createCollection("empty", 3);
    await db.dropCollection("empty");
    await assert.rejects(db.dropCollection("default"), code("default_collection_immutable"));
    await db.close();
  } finally {
    await native.close();
    await rm(directory, { recursive: true });
  }
});
