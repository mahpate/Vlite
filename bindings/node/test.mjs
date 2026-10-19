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
    assert.deepEqual(await db.get("18446744073709551615"), {
      id: "18446744073709551615", vector: [1, 0, 0], payload_base64: "AP8=", metadata,
    });
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
