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
const FAR = { op: "eq", field: "group", value: { type: "string", value: "far" } };
// A filter never turns ready-only search into a build or exact fallback; malformed filters fail first.
await assert.rejects(client.search({ vector: [1, 0, 0], k: 1, mode: "approximate_ready", filter: [FAR] }),
  (e) => e instanceof VLiteError && e.status === 409 && e.code === "index_not_ready");
await assert.rejects(client.search({ vector: [1, 0, 0], k: 1, mode: "approximate_ready",
  filter: [{ op: "range", field: "rank", min: { type: "string", value: "1" } }] }),
(e) => e instanceof VLiteError && e.status === 422 && e.code === "invalid_input");
const record = { vector: [1, 0, 0], payload_base64: "AP8=",
  metadata: { n: { type: "integer", value: "-9223372036854775808" } } };
const caps = await client.capabilities();
const perRecord = caps.features?.per_record_etags === true;
// Strip the additive per-record revision (newer servers) before comparing records.
const plain = ({ revision, ...rest }) => {
  if (revision !== undefined) assert.match(revision, /^(0|[1-9][0-9]{0,19})$/);
  return rest;
};
assert.equal(await client.put("18446744073709551615", record), undefined);
assert.deepEqual(plain(await client.get("18446744073709551615")), { id: "18446744073709551615", ...record });
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
assert.deepEqual(plain(initial.record), { id: wide, vector: [1, 0, 0], payload_base64: "", metadata: {} });
assert.deepEqual(initial.record, await client.get(wide));
assert.equal("revision" in initial.record, perRecord);
assert.match(initial.etag, /^"[\x21\x23-\x7e]+"$/);
assert.equal((await client.getVersioned(wide)).etag, initial.etag);

assert.equal(await client.put(wide, { vector: [0, 1, 0] }, { ifMatch: initial.etag }), undefined);
assert.deepEqual((await client.get(wide)).vector, [0, 1, 0]);
await assert.rejects(client.put(wide, { vector: [0, 0, 1] }, { ifMatch: initial.etag }), is412);
await assert.rejects(client.delete(wide, { ifMatch: initial.etag }), is412);
assert.deepEqual((await client.get(wide)).vector, [0, 1, 0]);

let current = (await client.getVersioned(wide)).etag;
await client.put(other, { vector: [1, 1, 0] });
// Per-record tags survive unrelated writes and batches; legacy database-wide tags do not.
if (perRecord) await client.put(wide, { vector: [0, 1, 0] }, { ifMatch: current });
else await assert.rejects(client.delete(wide, { ifMatch: current }), is412);
current = (await client.getVersioned(wide)).etag;
await client.batch([{ op: "put", id: gone, vector: [1, 0, 1] }]);
if (perRecord) await client.put(wide, { vector: [0, 1, 0] }, { ifMatch: current });
else await assert.rejects(client.put(wide, { vector: [0, 0, 1] }, { ifMatch: current }), is412);
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

// Filtered ANN: every eligible record lies outside the unfiltered top k.
const RESTRICTIVE = [FAR, { op: "range", field: "rank", min: { type: "integer", value: "2" } }];
const NO_MATCH = [[{ op: "eq", field: "group", value: { type: "string", value: "none" } }],
  [FAR, { op: "range", field: "rank", min: { type: "integer", value: "1000" } }],
  [{ op: "eq", field: "group", value: { type: "integer", value: "1" } }]]; // strict type match
const tagged = (vector, group, rank) => ({ vector, metadata: {
  group: { type: "string", value: group }, rank: { type: "integer", value: String(rank) } } });
// ANN returns only eligible current records (at most k) and, on this tiny graph
// with ef_search above the live count, the same hits as the exact oracle.
async function checkFiltered(metric, k, filter, eligible, expected) {
  const query = { vector: [1, 0, 0], k, metric };
  const exact = await client.search({ ...query, filter });
  assert.deepEqual(exact.map((h) => h.id), expected, `${metric} exact`);
  for (const mode of ["approximate", "approximate_ready"]) {
    for (const extra of [{}, { ef_search: 100 }]) {
      const hits = await client.search({ ...query, mode, filter, ...extra });
      const label = `${metric} ${mode} ${JSON.stringify(hits)}`;
      assert.ok(hits.length <= k && hits.every((h) => eligible.includes(h.id)), label);
      for (let i = 1; i < hits.length; i++) {
        const [a, b] = [hits[i - 1], hits[i]];
        assert.ok(a.distance < b.distance || (a.distance === b.distance && BigInt(a.id) < BigInt(b.id)), label);
      }
      if (extra.ef_search) {
        assert.deepEqual(hits.map((h) => h.id), expected, label);
        hits.forEach((h, i) => assert.ok(Math.abs(h.distance - exact[i].distance) <= 1e-9, label));
      }
    }
    const unfiltered = await client.search({ ...query, mode });
    assert.ok(unfiltered.length === k && !unfiltered.some((h) => eligible.includes(h.id)), "filter must be restrictive");
  }
}
const filteredBefore = (await client.database()).records;
const near = Array.from({ length: 16 }, (_, n) => String(200 + n));
const seed = [...near.map((id, n) => ({ op: "put", id, ...tagged([1, 0.05 * n, 0], "near", n) })),
  ...[["300", [0, 1, 0], 1], ["301", [0, 0, 1], 2], ["302", [0.1, 1, 0], 3], ["303", [-1, 0.2, 0], 4], ["304", [0, 1, 1], 5]]
    .map(([id, vector, rank]) => ({ op: "put", id, ...tagged(vector, "far", rank) }))];
for (const metric of ["squared_l2", "cosine"]) {
  await client.batch(seed);
  await client.buildIndex({ metric });
  const built = await client.indexStatus();
  assert.ok(built.ready && built.metric === metric);
  // Same order for both metrics (cosine 301/304 tie by ID).
  await checkFiltered(metric, 3, RESTRICTIVE, ["301", "302", "303", "304"], ["302", "301", "304"]);
  // k above the eligible count underfills rather than padding with ineligible records.
  await checkFiltered(metric, 10, RESTRICTIVE, ["301", "302", "303", "304"], ["302", "301", "304", "303"]);
  for (const filter of NO_MATCH) await checkFiltered(metric, 3, filter, [], []);
  // Buffered updates/deletes without rebuilding: old graph entries must not leak.
  await client.put("215", tagged([1, 0.75, 0], "far", 9)); // becomes eligible; still outside unfiltered top 3
  await client.put("301", tagged([0, 0, 1], "near", 2)); // no longer eligible
  assert.equal(await client.delete("302"), true);
  await client.batch([{ op: "put", id: "305", ...tagged([0.5, 1, 0], "far", 2) }, { op: "delete", id: "304" }]);
  const status = await client.indexStatus();
  assert.ok(status.ready && status.metric === metric && status.buffered_ids > 0, JSON.stringify(status));
  await checkFiltered(metric, 3, RESTRICTIVE, ["215", "303", "305"], ["215", "305", "303"]);
  for (const filter of NO_MATCH) await checkFiltered(metric, 3, filter, [], []);
  for (const mode of ["approximate", "approximate_ready"]) {
    await assert.rejects(client.search({ vector: [1, 0, 0], k: 1, mode, metric: "inner_product", filter: RESTRICTIVE }),
      (e) => e instanceof VLiteError && e.status === 422 && e.code === "invalid_input");
  }
  await client.batch([...seed.filter((op) => !["302", "304"].includes(op.id)).map(({ id }) => ({ op: "delete", id })),
    { op: "delete", id: "305" }]);
  assert.equal((await client.database()).records, filteredBefore);
}
console.log("PASS JavaScript live filtered ANN");
console.log("PASS JavaScript live REST client");
