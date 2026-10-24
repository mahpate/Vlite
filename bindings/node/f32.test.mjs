import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdir, rm } from "node:fs/promises";
import { randomUUID } from "node:crypto";
import { fileURLToPath } from "node:url";
import { join } from "node:path";
import { NativeLibrary, EmbeddedError } from "./index.js";

const errorCode = (expected) => (error) => error instanceof EmbeddedError && error.code === expected;
const testDirectory = () => fileURLToPath(new URL(`./test-data-${randomUUID()}/`, import.meta.url));

async function withDatabase(fn, options = {}) {
  const directory = testDirectory();
  await mkdir(directory);
  const libraryPath = options.libraryPath ?? process.env.VLITE_TEST_LIBRARY;
  const native = new NativeLibrary({ ...(libraryPath ? { libraryPath } : {}) });
  try {
    const db = await native.createInMemory(2, { format: 4 });
    await fn(native, db, directory);
  } finally {
    await native.close();
    await rm(directory, { recursive: true });
  }
}

test("Float32Array batch puts, exact search parity and immediate mutation snapshots", async () => withDatabase(async (_native, db) => {
  const mutable = new Float32Array([1, 0]);
  const write = db.batchPutF32([
    { id: "1", vector: mutable, payload: Buffer.from([0, 255]), metadata: { tag: { type: "string", value: "typed" } } },
    { id: "2", vector: new Float32Array([0, 1]) },
  ]);
  mutable.fill(0);
  await write;

  assert.deepEqual((await db.get("1")).vector, [1, 0]);
  assert.equal((await db.get("1")).payload_base64, "AP8=");
  assert.equal((await db.get("1")).metadata.tag.value, "typed");
  assert.deepEqual(await db.searchF32(new Float32Array([1, 0]), 2), await db.search([1, 0], 2));
  assert.deepEqual(await db.searchF32(new Float32Array([1, 0]), 1, { explain: true }),
    await db.search([1, 0], 1, { explain: true }));

  const query = new Float32Array([1, 0]);
  const search = db.searchF32(query, 1);
  query.fill(0);
  assert.equal((await search)[0].id, "1");
  assert.equal(query.byteLength, 8, "caller buffer must not be detached");
  const backing = new Float32Array([99, 0, 1, 99]);
  const offset = backing.subarray(1, 3);
  const pendingMany = db.searchManyF32([offset, new Float32Array([1, 0])], 1);
  backing.fill(0);
  assert.deepEqual((await pendingMany).map((hits) => hits[0].id), ["2", "1"]);
}));

test("bounded multi-query search matches individual searches and preserves engine errors", async () => withDatabase(async (_native, db) => {
  await db.batchPutF32([
    { id: "1", vector: new Float32Array([1, 0]) },
    { id: "2", vector: new Float32Array([0, 1]) },
  ]);
  const queries = [new Float32Array([1, 0]), new Float32Array([0, 1])];
  const many = await db.searchManyF32(queries, 2);
  assert.deepEqual(many, await Promise.all(queries.map((vector) => db.search(Array.from(vector), 2))));
  const explained = await db.searchManyF32(queries, 1, { explain: true });
  assert.deepEqual(explained.map((result) => result.hits), await Promise.all(queries.map((vector) => db.search(Array.from(vector), 1))));
  await assert.rejects(db.searchF32(new Float32Array([1]), 1), errorCode("invalid_input"));
  const filter = [{ op: "eq", field: "tag", value: { type: "string", value: "missing" } }];
  assert.deepEqual(await db.searchF32(queries[0], 2, { filter }), []);
  for (const metric of ["cosine", "inner_product"]) {
    assert.deepEqual(await db.searchManyF32(queries, 2, { metric }),
      await Promise.all(queries.map((vector) => db.search(Array.from(vector), 2, { metric }))));
  }
  await assert.rejects(db.searchManyF32([queries[0], new Float32Array([0, 0])], 1, { metric: "cosine" }),
    errorCode("invalid_input"));
  await assert.rejects(db.searchF32(queries[0], 1, { metric: "bad" }), errorCode("invalid_input"));
  await db.createCollection("typed", 2);
  await db.batchPutF32([{ id: "3", vector: queries[0] }], { collection: "typed" });
  assert.equal((await db.searchF32(queries[0], 1, { collection: "typed" }))[0].id, "3");
  await assert.rejects(db.batchPutF32([{ id: "4", vector: queries[0] }], { collection: "missing" }),
    errorCode("collection_not_found"));
}));

test("typed input validation, bounded sizes and atomic native errors", async () => withDatabase(async (_native, db) => {
  await assert.rejects(db.searchF32(new Float64Array([1, 0]), 1), TypeError);
  await assert.rejects(db.searchF32(new Float32Array([NaN, 0]), 1), TypeError);
  await assert.rejects(db.searchF32(new Float32Array(2097153), 1), RangeError);
  await assert.rejects(db.searchF32(new Float32Array(), 1), RangeError);
  const detached = new Float32Array([1, 0]);
  structuredClone(detached, { transfer: [detached.buffer] });
  await assert.rejects(db.searchF32(detached, 1), RangeError);
  await assert.rejects(db.searchManyF32([], 1), RangeError);
  await assert.rejects(db.searchManyF32(Array.from({ length: 129 }, () => new Float32Array([1, 0])), 1), RangeError);
  await assert.rejects(db.searchManyF32([new Float32Array([1, 0]), new Float32Array([1])], 1), RangeError);
  await assert.rejects(db.searchManyF32([new Float32Array([Infinity, 0])], 1), TypeError);
  await assert.rejects(db.batchPutF32([{ id: "1", vector: new Float64Array([1, 0]) }]), TypeError);
  await assert.rejects(db.batchPutF32([]), RangeError);
  await assert.rejects(db.batchPutF32(Array.from({ length: 4097 }, () => ({ id: "1", vector: new Float32Array([1, 0]) }))), RangeError);
  await assert.rejects(db.batchPutF32([{ id: "1", vector: new Float32Array([1, 0]), expectedRevision: "absent" }]), TypeError);
  await assert.rejects(db.batchPutF32([{ id: "1", vector: new Float32Array([Infinity, 0]) }]), TypeError);
  await assert.rejects(db.searchF32(new Float32Array([1, 0]), 1, { collection: "x".repeat(8 * 1024 * 1024) }), RangeError);
  await assert.rejects(db.batchPutF32([{ id: "1", vector: new Float32Array([1, 0]) }, {
    id: "2", vector: new Float32Array([1]),
  }]), RangeError);
  await assert.rejects(db.batchPutF32([{ id: "1", vector: new Float32Array([1, 0]) }, {
    id: "02", vector: new Float32Array([0, 1]),
  }]), errorCode("invalid_input"));
  await assert.rejects(db.get("1"), errorCode("not_found"));
  if (typeof SharedArrayBuffer !== "undefined") {
    await assert.rejects(db.searchF32(new Float32Array(new SharedArrayBuffer(8)), 1), TypeError);
  }
}));

test("typed calls preserve close, queue rejection, read-only and request errors", async () => withDatabase(async (native, _db, directory) => {
  const path = join(directory, "read-only.vlite");
  const writer = await native.create(path, 2);
  const vector = new Float32Array([1, 0]);
  const payload = new Uint8Array([0, 255]);
  const metadata = { tag: { type: "string", value: "before" } };
  const put = writer.batchPutF32([{ id: "1", vector, payload, metadata }]);
  vector.fill(0); payload.fill(0); metadata.tag.value = "after";
  await put;
  assert.equal((await writer.get("1")).payload_base64, "AP8=");
  assert.equal((await writer.get("1")).metadata.tag.value, "before");
  await writer.close();
  await assert.rejects(writer.searchF32(new Float32Array([1, 0]), 1), errorCode("closed"));
  const reader = await native.open(path, { readOnly: true });
  await assert.rejects(reader.batchPutF32([{ id: "2", vector: new Float32Array([0, 1]) }]), errorCode("read_only"));
  assert.equal((await reader.info()).records, 1);
  await reader.close();
  const queued = new NativeLibrary({ maxPending: 1, libraryPath: process.env.VLITE_TEST_LIBRARY });
  try {
    const db = await queued.createInMemory(2);
    const first = db.batchPutF32([{ id: "1", vector: new Float32Array([1, 0]) }]);
    await assert.rejects(db.searchF32(new Float32Array([1, 0]), 1), errorCode("busy"));
    await first;
    const query = db.searchF32(new Float32Array([1, 0]), 1);
    await assert.rejects(db.batchPutF32([{ id: "2", vector: new Float32Array([0, 1]) }]), errorCode("busy"));
    await query;
    assert.equal((await db.info()).records, 1);
  } finally { await queued.close(); }
}));

test("typed methods use JSON fallback with older ABI-1 libraries", {
  skip: !process.env.VLITE_TEST_LEGACY_LIBRARY,
}, async () => withDatabase(async (_native, db) => {
  await db.batchPutF32([
    { id: "1", vector: new Float32Array([1, 0]) },
    { id: "2", vector: new Float32Array([0, 1]) },
  ]);
  assert.equal((await db.searchF32(new Float32Array([1, 0]), 1))[0].id, "1");
  assert.equal((await db.searchManyF32([new Float32Array([0, 1])], 1))[0][0].id, "2");
}, { libraryPath: process.env.VLITE_TEST_LEGACY_LIBRARY }));
