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
assert.equal((await client.metrics()).database_healthy, true);
await assert.rejects(new Client(process.env.VLITE_URL, "b".repeat(32)).database(),
  (e) => e instanceof VLiteError && e.status === 401 && Boolean(e.requestId));
await assert.rejects(client.batch([{ op: "put", id: "3", vector: [1, 2] }]),
  (e) => e instanceof VLiteError && e.status === 422);
assert.equal((await client.database()).records, 2);
assert.equal(await client.delete("2"), true);
assert.equal(await client.delete("2"), false);
await assert.rejects(client.get("2"), (e) => e instanceof VLiteError && e.status === 404);
console.log("PASS JavaScript live REST client");
