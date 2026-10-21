import assert from "node:assert/strict";
import { Client, VLiteError } from "./index.js";

const client = new Client(process.env.VLITE_URL, process.env.VLITE_AUTH_TOKEN);
assert.equal((await client.health()).status, "ok");
assert.equal((await client.ready()).status, "ready");
assert.equal((await client.database()).dimensions, 3);
assert.equal((await client.indexStatus()).ready, false);
await client.put("0", { vector: [0, 0, 1] });
await assert.rejects(client.search({ vector: [1, 0, 0], k: 1, mode: "approximate_ready" }),
  (e) => e instanceof VLiteError && e.code === "index_not_ready");
const record = { vector: [1, 0, 0], payload_base64: "AP8=",
  metadata: { n: { type: "integer", value: "-9223372036854775808" } } };
assert.equal(await client.put("18446744073709551615", record), undefined);
assert.deepEqual(await client.get("18446744073709551615"), { id: "18446744073709551615", ...record });
assert.equal((await client.search({ vector: [1, 0, 0], k: 1, metric: "cosine" }))[0].distance, 0);
assert.equal((await client.search({ vector: [1, 0, 0], k: 1, metric: "inner_product" }))[0].distance, -1);
await assert.rejects(client.search({ vector: [1, 0, 0], k: 1, metric: "inner_product", mode: "approximate_ready" }),
  (e) => e instanceof VLiteError && e.status === 422 && e.code === "invalid_input");
await client.batch([{ op: "put", id: "2", vector: [0, 1, 0] }, { op: "delete", id: "0" }]);
const first = await client.listVectors({ limit: 1 });
assert.deepEqual(first.items.map((item) => item.id), ["2"]);
assert.equal(first.next_after, "2");
const last = await client.listVectors({ limit: 1, after: first.next_after });
assert.equal(last.items[0].id, "18446744073709551615");
assert.equal(last.next_after, null);
assert.equal((await client.search({ vector: [1, 0, 0], k: 2, filter: [
  { op: "eq", field: "n", value: record.metadata.n },
] }))[0].id, "18446744073709551615");
assert.equal(await client.buildIndex(), undefined);
assert.equal((await client.indexStatus()).ready, true);
for (const mode of ["approximate", "approximate_ready"]) {
  assert.equal((await client.search({ vector: [1, 0, 0], k: 1, mode }))[0].distance, 0);
}
await client.buildIndex({ metric: "cosine" });
assert.equal((await client.indexStatus()).metric, "cosine");
assert.equal((await client.search({ vector: [1, 0, 0], k: 1, metric: "cosine", mode: "approximate_ready" }))[0].distance, 0);
await assert.rejects(client.search({ vector: [1, 0, 0], k: 1, mode: "approximate_ready" }),
  (e) => e instanceof VLiteError && e.code === "index_not_ready");
assert.equal((await client.metrics()).database_healthy, true);
await assert.rejects(new Client(process.env.VLITE_URL, "b".repeat(32)).database(),
  (e) => e instanceof VLiteError && e.status === 401 && Boolean(e.requestId));
await assert.rejects(client.batch([{ op: "put", id: "3", vector: [1, 2] }]),
  (e) => e instanceof VLiteError && e.status === 422);
assert.equal((await client.database()).records, 2);
assert.equal(await client.delete("2"), true);
assert.equal(await client.delete("2"), false);
await assert.rejects(client.get("2"), (e) => e instanceof VLiteError && e.status === 404);

// Conditional writes on separate records; the record count is restored afterward.
const is412 = (e) => e instanceof VLiteError && e.status === 412 && e.code === "precondition_failed" && Boolean(e.requestId);
const before = (await client.database()).records;
const wide = "18446744073709551614", other = "100", gone = "101";
await client.put(wide, { vector: [1, 0, 0] });
const initial = await client.getVersioned(wide);
assert.deepEqual(initial.record, { id: wide, vector: [1, 0, 0], payload_base64: "", metadata: {} });
assert.deepEqual(initial.record, await client.get(wide));
assert.match(initial.etag, /^"[\x21\x23-\x7e]+"$/);
assert.equal((await client.getVersioned(wide)).etag, initial.etag);

assert.equal(await client.put(wide, { vector: [0, 1, 0] }, { ifMatch: initial.etag }), undefined);
assert.deepEqual((await client.get(wide)).vector, [0, 1, 0]);
await assert.rejects(client.put(wide, { vector: [0, 0, 1] }, { ifMatch: initial.etag }), is412);
await assert.rejects(client.delete(wide, { ifMatch: initial.etag }), is412);
assert.deepEqual((await client.get(wide)).vector, [0, 1, 0]);

let current = (await client.getVersioned(wide)).etag;
await client.put(other, { vector: [1, 1, 0] }); // unrelated write invalidates every tag
await assert.rejects(client.delete(wide, { ifMatch: current }), is412);
current = (await client.getVersioned(wide)).etag;
await client.batch([{ op: "put", id: gone, vector: [1, 0, 1] }]);
await assert.rejects(client.put(wide, { vector: [0, 0, 1] }, { ifMatch: current }), is412);
assert.deepEqual((await client.get(wide)).vector, [0, 1, 0]);

const stale = (await client.getVersioned(gone)).etag;
assert.equal(await client.delete(gone), true);
await assert.rejects(client.put(gone, { vector: [1, 0, 0] }, { ifMatch: stale }), is412);
await assert.rejects(client.get(gone), (e) => e instanceof VLiteError && e.status === 404);

async function race(calls) {
  const results = await Promise.allSettled(calls.map((call) => call()));
  const winners = results.flatMap((r, i) => (r.status === "fulfilled" ? [i] : []));
  assert.equal(winners.length, 1, JSON.stringify(results));
  for (const r of results) if (r.status === "rejected") assert.ok(is412(r.reason), String(r.reason));
  return { index: winners[0], value: results[winners[0]].value };
}
const writers = 8;
let tag = (await client.getVersioned(wide)).etag;
const put = await race(Array.from({ length: writers }, (_, i) => () => client.put(wide, { vector: [i + 2, 0, 0] }, { ifMatch: tag })));
assert.deepEqual((await client.get(wide)).vector, [put.index + 2, 0, 0]);
tag = (await client.getVersioned(wide)).etag;
const del = await race(Array.from({ length: writers }, () => () => client.delete(wide, { ifMatch: tag })));
assert.equal(del.value, true);
await assert.rejects(client.delete(wide, { ifMatch: tag }), is412);

tag = (await client.getVersioned(other)).etag;
assert.equal(await client.delete(other, { ifMatch: tag }), true);
assert.equal((await client.database()).records, before);
console.log("PASS JavaScript live conditional writes");
console.log("PASS JavaScript live REST client");
