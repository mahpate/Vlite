import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdir, rm, readFile } from "node:fs/promises";
import { randomUUID } from "node:crypto";
import { fileURLToPath } from "node:url";
import { join } from "node:path";
import { NativeLibrary, EmbeddedError } from "./index.js";

const code = expected => error => error instanceof EmbeddedError && error.code === expected;
const metadata = (group = "a", score = "2", latitude = 0) => ({
  group: { type: "string", value: group }, score: { type: "integer", value: score },
  place: { type: "geopoint", value: { latitude, longitude: 0 } },
});
async function fixture(fn) {
  const directory = fileURLToPath(new URL(`./test-data-${randomUUID()}/`, import.meta.url));
  await mkdir(directory);
  const native = new NativeLibrary();
  try { await fn(native, directory); }
  finally { await native.close(); await rm(directory, { recursive: true }); }
}

test("V5 TTL, metadata-only updates, grouped/recommended/faceted/statistical/geo retrieval", async () => fixture(async native => {
  const db = await native.createInMemory(2, { format: 5 });
  await db.createCollection("docs", 2);
  const options = { collection: "docs" };
  const revision = await db.putWithExpiry("1", [1, 0], { ...options, metadata: metadata(), expiresAtMs: "18446744073709551615", payload: Buffer.from([0, 255]) });
  await db.put("2", [0, 1], { ...options, metadata: metadata("b", "4", 1) });
  assert.notEqual(await db.updateMetadata("1", metadata("b"), options), revision);
  assert.equal((await db.get("1", options)).payload_base64, "AP8=");
  assert.equal((await db.searchGrouped([1, 0], 2, 2, "group", options)).length, 1);
  assert.equal((await db.facets("group", options))[0].count, 2);
  assert.deepEqual(await db.numericStats("score", options), { count: 2, min: 2, max: 4, sum: 6, average: 3 });
  assert.equal((await db.recommend(2, { ...options, positiveIds: ["1"] }))[0].id, "2");
  assert.equal((await db.searchGeo([1, 0], 2, "place", 0, 0, 1000, { ...options, sort: "geo_distance" }))[0].id, "1");
  assert.equal((await db.searchGeo([1, 0], 2, "place", 0, 0, 1000, options)).length, 1);
  await db.put("3", [1, 1], { ...options, expiresAtMs: 1 });
  await assert.rejects(db.get("3", options), code("not_found"));
  await assert.rejects(db.putWithExpiry("4", [1], options), code("invalid_input"));
  await assert.rejects(db.put("4", [1, 1], { expectedRevision: "absent", expiresAtMs: 2 }), TypeError);
  await assert.rejects(db.putWithExpiry("4", [1, 1], { expiresAtMs: Number.MAX_SAFE_INTEGER + 1 }), TypeError);
  const legacy = await native.createInMemory(2);
  await assert.rejects(legacy.putWithExpiry("1", [1, 0]), code("requires_format_v5"));
}));

test("named/sparse/dense-sparse vectors, model versions, pruning and byte-image persistence", async () => fixture(async native => {
  const db = await native.createInMemory(2, { format: 5 });
  await db.put("1", [1, 0]); await db.put("2", [0, 1]);
  await db.configureVector("text", 2, { activeVersion: "v1", metric: "cosine", history: 1 });
  await db.configureVector("handle", 2, { activeVersion: "v1" });
  assert.equal((await db.vectorSpecs()).text.active_version, "v1");
  await db.putVectors("1", { dense: { text: [{ version: "v1", vector: [1, 0] }] }, sparse: { lexical: [[1, 2]] } });
  await db.putVectorVersion("2", "text", "v1", [0, 1]);
  await db.putVectorVersion("1", "text", "v0", [0, 1]);
  assert.equal((await db.searchNamed("text", [1, 0], 1))[0].id, "1");
  assert.equal((await db.searchNamed("text", [0, 1], 1, { version: "v0" }))[0].id, "1");
  assert.equal((await db.searchSparse("lexical", [[1, 1]], 1))[0].distance, -2);
  assert.equal((await db.searchDenseSparse("text", [1, 0], "lexical", [[1, 1]], 1, { fusion: { rrfK: 10 } }))[0].id, "1");
  assert.equal((await db.recordVectors("1")).dense.text.length, 2);
  assert.equal(await db.recordVectors("123"), null);
  await assert.rejects(db.putVectors("1", { sparse: { lexical: [[2, 1], [1, 1]] } }), code("invalid_input"));
  const reopened = await native.openFromBytes(await db.toBytes());
  assert.equal((await reopened.searchNamed("text", [1, 0], 1))[0].id, "1");
  await db.pruneVectorHistory("text", 0);
  assert.deepEqual(await db.searchNamed("text", [0, 1], 1, { version: "v0" }), []);
  await db.updateMetadata("1", {});
  assert.ok(await db.recordVectors("1"));
}));

test("exact/MMR rerank and parsed read-only queries", async () => fixture(async native => {
  const db = await native.createInMemory(2, { format: 5 });
  await db.put("1", [1, 0], { metadata: metadata() }); await db.put("2", [0, 1], { metadata: metadata("b") });
  const candidates = [{ id: "2", distance: 0 }, { id: "1", distance: 1 }];
  assert.equal((await db.rerank([1, 0], candidates, 2))[0].id, "1");
  assert.equal((await db.rerank([1, 0], candidates, 2, { method: "mmr", lambda: 1 }))[0].id, "1");
  await assert.rejects(db.rerank([1, 0], candidates, 1, { method: "mmr", lambda: 2 }), code("invalid_input"));
  const result = await db.query("SEARCH default NEAR $q K 2 GROUP BY group FACET group", { q: [1, 0] });
  assert.equal(result.hits[0].id, "1"); assert.equal(result.groups.length, 2); assert.equal(result.facets.length, 2);
  await assert.rejects(db.query("DELETE everything"), code("query_parse_error"));
}));

test("change feed cursors preserve whole batches and typed mutation data", async () => fixture(async native => {
  const db = await native.createInMemory(2, { format: 5 });
  const cursor = await db.feedCursor();
  await db.putWithExpiry("1", [1, 0], { metadata: metadata(), expiresAtMs: "18446744073709551615" });
  await db.updateMetadata("1", metadata("b")); await db.delete("1");
  const batch = await db.changesAfter(cursor, 1);
  assert.equal(batch.changes[0].kind, "put"); assert.equal(batch.changes[0].expires_at_ms, "18446744073709551615");
  assert.ok(batch.has_more);
  const rest = await db.changesAfter(batch.next);
  assert.deepEqual(rest.changes.map(c => c.kind), ["metadata_update", "delete"]);
  assert.equal((await db.changesAfter()).changes.length, 3);
  await assert.rejects(db.changesAfter("bad"), code("invalid_cursor"));
  const other = await native.createInMemory(2, { format: 5 });
  await assert.rejects(other.changesAfter(cursor), code("cursor_compacted"));
  await db.batch([{ op: "put", id: "2", vector: [1, 0] }, { op: "put", id: "3", vector: [0, 1] }]);
  assert.equal((await db.changesAfter(rest.next, 1)).changes.length, 2);
}));

test("encryption raw/passphrase create/open, wrong keys, and encrypted byte images", async () => fixture(async (native, directory) => {
  const key = new Uint8Array(32).fill(7);
  for (const credentials of [{ key }, { passphrase: "test-only secret passphrase" }]) {
    const path = join(directory, "key" in credentials ? "raw.vlite" : "pass.vlite");
    const db = await native.create(path, 2, credentials);
    assert.ok(await db.isEncrypted()); assert.equal(typeof await db.encryptionKeyId(), "string");
    await db.put("1", [1, 0], { payload: Buffer.from("sensitive") }); await db.close();
    assert.ok(!(await readFile(path)).includes("sensitive"));
    await assert.rejects(native.open(path), code("encryption_key_required"));
    await assert.rejects(native.open(path, { key: new Uint8Array(32).fill(8) }), code("encryption_key_mismatch"));
    const reopened = await native.open(path, credentials); assert.deepEqual((await reopened.get("1")).vector, [1, 0]);
  }
  const db = await native.createInMemory(2, { key });
  const image = await db.toBytes();
  await assert.rejects(native.openFromBytes(image), code("encryption_key_required"));
  assert.ok(await (await native.openFromBytes(image, { key })).isEncrypted());
  await assert.rejects(native.createInMemory(2, { key: new Uint8Array(1) }), code("invalid_input"));
  await assert.rejects(native.createInMemory(2, { key, passphrase: "x" }), TypeError);
  const plain = await native.createInMemory(2); assert.equal(await plain.isEncrypted(), false); assert.equal(await plain.encryptionKeyId(), null);
}));

test("point-in-time sequence/timestamp restore and bounded-cache disk ANN with stale-index rejection", async () => fixture(async (native, directory) => {
  const path = join(directory, "log.vlite");
  const db = await native.create(path, 2, { format: 5, commitTimestamps: true });
  await db.put("1", [1, 0]); const first = await db.revision("1"); await db.put("2", [0, 1]);
  const timestampMs = (await db.changesAfter()).changes[0].committed_at_ms;
  const sidecar = join(directory, "index.dann");
  assert.equal((await db.buildDiskAnn(sidecar)).live_records, 2);
  assert.equal((await db.searchDiskAnn(sidecar, [1, 0], 1, { efSearch: 10, cacheBytes: 1 }))[0].id, "1");
  await db.put("3", [1, 1]); await assert.rejects(db.searchDiskAnn(sidecar, [1, 0], 1), code("stale_index"));
  await assert.rejects(db.toBytes(), code("invalid_input")); await db.close();
  const destination = join(directory, "restored.vlite");
  assert.equal((await native.restoreTo(path, destination, { sequence: first })).restored_sequence, first);
  const restored = await native.open(destination); assert.equal((await restored.info()).records, 1);
  await native.restoreTo(path, join(directory, "time.vlite"), { timestampMs });
  await assert.rejects(native.restoreTo(path, join(directory, "bad.vlite")), code("invalid_input"));
  await assert.rejects(native.restoreTo(path, join(directory, "beyond.vlite"), { sequence: "1000" }), code("invalid_restore_target"));
  const secret = await native.createInMemory(2, { key: new Uint8Array(32) });
  await assert.rejects(secret.buildDiskAnn(join(directory, "secret.dann")), code("sidecar_unavailable"));
}));

test("native TLS destructors remain valid across repeated disk-index worker shutdowns", async () => {
  for (let attempt = 0; attempt < 3; attempt++) {
    await fixture(async (native, directory) => {
      const db = await native.create(join(directory, "data.vlite"), 2, { format: 5 });
      await db.put("1", [1, 0]);
      const sidecar = join(directory, "index.dann");
      await db.buildDiskAnn(sidecar);
      assert.equal((await db.searchDiskAnn(sidecar, [1, 0], 1))[0].id, "1");
      // Closing every handle does not remove Rust's thread-local destructors.
      await db.close();
    });
  }
});
