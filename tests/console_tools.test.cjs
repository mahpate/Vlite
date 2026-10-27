"use strict";
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
global.window = {};
vm.runInThisContext(fs.readFileSync("src/dashboard/console-tools.js", "utf8"));
const { id, vector, metadata, record, savedQuery, csvParse, csvExport, distances, analyze, pca } = window.VLiteTools.utils;
const query = { version: 1, name: "nearest", query: { vector: [1, 0], k: 10, mode: "exact", metric: "cosine", ef_search: 100,
  config: { m: 16, ef_construction: 100, seed: "18446744073709551615" },
  filter: [{ op: "range", field: "__proto__", min: { type: "integer", value: "-9223372036854775808" }, max: { type: "integer", value: "9223372036854775807" } }] } };
assert.deepEqual(savedQuery(query, 2), query);
const filteredAnnQuery = { ...query, query: { ...query.query, mode: "approximate_ready", metric: "squared_l2" } };
assert.deepEqual(savedQuery(filteredAnnQuery, 2), filteredAnnQuery);
assert.throws(() => savedQuery({ ...query, token: "secret" }, 2));
assert.throws(() => savedQuery(query, 3));
assert.throws(() => savedQuery({ ...query, query: { ...query.query, headers: {} } }, 2));
assert.throws(() => savedQuery({ ...query, query: { ...query.query, filter: [{ op: "range", field: "a", min: { type: "float", value: 2 }, max: { type: "float", value: 1 } }] } }, 2));
assert.throws(() => savedQuery({ ...query, query: { ...query.query, config: { ...query.query.config, seed: "01" } } }, 2));
assert.equal(id("18446744073709551615"), "18446744073709551615");
for (const value of ["01", "-1", "18446744073709551616", 1]) assert.throws(() => id(value));
assert.throws(() => vector([1, 2], 3));
assert.throws(() => vector([Infinity], 1));
assert.throws(() => vector([1e39], 1));
metadata({ count: { type: "integer", value: "9223372036854775807" } });
assert.throws(() => metadata({ count: { type: "integer", value: 1 } }));
assert.throws(() => metadata({ count: { type: "integer", value: "01" } }));
assert.throws(() => metadata({ count: { type: "integer", value: "9223372036854775808" } }));
assert.throws(() => metadata(Object.fromEntries(Array.from({ length: 20 }, (_, index) => [String(index), { type: "string", value: "x".repeat(4096) }]))));
assert.throws(() => record({ id: "1", vector: [1], payload_base64: "bad!" }, 1));
assert.throws(() => record({ id: "1", vector: [1], payload_base64: "AB==" }, 1));
assert.throws(() => record({ id: "1", vector: [1], payload_base64: "AAAA".repeat(349526) }, 1));
const items = [
  { id: "18446744073709551615", vector: [1, 0], payload_base64: "AP8=", metadata: { label: { type: "string", value: 'a,"b"\nc' } } },
  { id: "2", vector: [0, 1], payload_base64: "", metadata: {} },
];
assert.deepEqual(csvParse(csvExport(items)).map((item) => record(item, 2)), items);
assert.throws(() => csvParse('id,vector\n"1,[1]'));
assert.throws(() => csvParse('id,vector\n"1"x,[1]'));
assert.throws(() => csvParse('id,id,vector\n1,2,[1]'));
assert.deepEqual(distances([1, 0], [0, 1]), { squared_l2: 2, cosine: 1, inner_product: -0 });
assert.equal(distances([0, 0], [1, 0]).cosine, null);
const report = analyze([...items, { ...items[0], id: "3" }, { id: "4", vector: [0, 0], metadata: {} }]);
assert.deepEqual(report.duplicate_vector_pairs, [["18446744073709551615", "3"]]);
assert.deepEqual(report.zero_vector_ids, ["4"]);
const points = pca(items);
assert.equal(points.length, 2);
assert.ok(points.every((point) => Number.isFinite(point.x) && Number.isFinite(point.y)));
assert.ok(Math.abs(points[0].x - points[1].x) > 1);
const axes = pca([[2, 0], [-2, 0], [0, 1], [0, -1]].map((values, index) => ({ id: String(index), vector: values })));
assert.ok(Math.abs(Math.abs(axes[0].x) - 2) < 1e-6);
assert.ok(Math.abs(Math.abs(axes[2].y) - 1) < 1e-6);
assert.ok(Math.abs(axes[0].y) < 1e-6);
assert.throws(() => pca([items[0]]));
assert.throws(() => pca([{ id: "1", vector: Array(257).fill(1) }, { id: "2", vector: Array(257).fill(2) }]));
console.log("PASS console helpers: u64/i64, dimensions, typed metadata, CSV round-trip/quoting, distances, sample quality and PCA bounds");
// ---- EPIC-015 helpers ----
const u = window.VLiteTools.utils;
// Collection routing: default keeps legacy routes; names are validated before any path is built.
assert.equal(u.collectionPath("default", "/vectors/1"), "/v1/vectors/1");
assert.equal(u.collectionPath("docs_v2", "/search"), "/v1/collections/docs_v2/search");
for (const bad of ["", "Docs", "1docs", "a/b", "a".repeat(64), "../x", "a?b"]) assert.throws(() => u.collectionPath(bad, "/search"));
assert.throws(() => u.collectionPath("docs", "search"));
// Settings bounds (partial objects; unknown fields refused).
assert.deepEqual(u.settings({}), {});
u.settings({ metric: "cosine", hnsw: { m: 16, ef_construction: 100, seed: "42" }, quantization: "scalar_u8", rerank_multiplier: 16,
  planner: { exact_max_candidates: 1000, exact_max_selectivity_ppm: 0 }, metadata_indexes: [{ field: "year", kind: "range" }], text_fields: ["title"] });
for (const bad of [{ token: "x" }, { metric: "l1" }, { rerank_multiplier: 17 }, { quantization: "pq" }, { hnsw: { m: 1, ef_construction: 1, seed: "1" } },
  { metadata_indexes: [{ field: "a", kind: "eq" }, { field: "a", kind: "range" }] }, { text_fields: Array(9).fill("t").map((v, i) => v + i) }]) assert.throws(() => u.settings(bad));
// Score direction is explicit per mode.
assert.equal(u.scoreDirection("hybrid", "cosine").better, "higher");
for (const mode of ["exact", "approximate", "approximate_ready", "auto"]) assert.equal(u.scoreDirection(mode, "cosine").better, "lower");
// Redaction: credential fields and scoped tokens never reach display/export surfaces.
const tokenValue = "vlk_abcdefghijk2_" + "S".repeat(43);
const redacted = u.redact({ token: tokenValue, credential: { id: "abcdefghijk2", name: "ci" }, nested: [{ Authorization: "Bearer x", refresh_token: "y" }], message: `bad ${tokenValue} here`, enabled: true });
assert.equal(redacted.token, "[redacted]");
assert.equal(redacted.nested[0].Authorization, "[redacted]");
assert.equal(redacted.nested[0].refresh_token, "[redacted]");
assert.equal(redacted.credential.id, "abcdefghijk2");
assert.ok(!JSON.stringify(redacted).includes("S".repeat(20)));
assert.equal(u.redact(`HTTP 401: ${tokenValue}`), "HTTP 401: [redacted credential]");
const masked = u.maskSecret(tokenValue);
assert.ok(masked.startsWith("vlk_abcdefghijk2_") && !masked.includes("SSSS") && masked.includes("43-character"));
assert.ok(!u.maskSecret("plain-secret-value").includes("plain"));
// Credential requests: bounded expiry, known operations, validated scope, `name` not `label`.
const now = 1700000000000;
assert.deepEqual(u.credentialRequest({ collections: "docs, default", operations: ["read", "search", "read"], days: "30", name: "ci" }, now),
  { collections: ["docs", "default"], operations: ["read", "search"], expires_unix_ms: now + 30 * 86400000, name: "ci" });
assert.deepEqual(u.credentialRequest({ collections: "*", operations: ["backup"], days: 1, name: "ops" }, now).collections, ["*"]);
for (const bad of [{ days: 0 }, { days: 366 }, { operations: [] }, { operations: ["admin"] }, { collections: "" }, { collections: "Bad" }, { collections: "a,a" }, { name: "x".repeat(65) }, { name: "" }])
  assert.throws(() => u.credentialRequest({ collections: "docs", operations: ["read"], days: 30, name: "ci", ...bad }, now));
const credentialDraft = { collections: "docs", operations: ["read"], days: 30, name: "ci" };
const tenant = "0123456789abcdef0123456789abcdef";
const graphDraft = { ...credentialDraft, collections: "", operations: ["graph_read", "graph_write"], graphs: "pool", tenant_id: tenant };
assert.deepEqual(u.credentialRequest(graphDraft, now), {
  collections: ["*"], operations: ["graph_read", "graph_write"], graphs: ["pool"], tenant_id: tenant,
  expires_unix_ms: now + 30 * 86400000, name: "ci",
});
assert.deepEqual(u.credentialRequest({ ...graphDraft, graphs: " * " }, now).graphs, ["*"]);
assert.deepEqual(u.credentialRequest({ ...graphDraft, operations: ["graph_read"], graphs: "pool, road-map_2" }, now).graphs, ["pool", "road-map_2"]);
assert.equal(u.credentialRequest({ ...graphDraft, graphs: Array.from({ length: 64 }, (_, index) => `g${index}`).join(",") }, now).graphs.length, 64);
assert.deepEqual(u.credentialRequest({ ...graphDraft, collections: "docs", operations: ["read", "graph_write"] }, now).collections, ["docs"]);
assert.ok(!("graphs" in u.credentialRequest({ ...credentialDraft, graphs: " , " }, now)));
for (const graphs of ["", " , ", "Pool", "1pool", "../pool", "pool?", "pool,pool", "*,pool", "a".repeat(64),
  Array.from({ length: 65 }, (_, index) => `g${index}`).join(",")])
  assert.throws(() => u.credentialRequest({ ...graphDraft, graphs }, now));
assert.throws(() => u.credentialRequest({ ...credentialDraft, graphs: "pool" }, now), /Select graph_read/);
assert.throws(() => u.credentialRequest({ ...graphDraft, graphs: "a".repeat(29) }, now), /28 characters/);
assert.equal(u.credentialRequest({ ...graphDraft, graphs: "a".repeat(28) }, now).graphs[0].length, 28);
assert.equal(u.credentialRequest({ ...graphDraft, tenant_id: "", graphs: "a".repeat(63) }, now).graphs[0].length, 63);
assert.match(u.credentialScope(u.credentialRequest(graphDraft, now)), /graphs \[pool\]/);
assert.match(u.credentialScope(u.credentialRequest(credentialDraft, now)), /graphs \[none\]/);
assert.deepEqual(u.credentialRequest({ ...credentialDraft, tenant_id: ` ${tenant} `, requests_per_second: "1000000",
  max_vectors: "0", max_vectors_per_collection: "9007199254740991" }, now), {
  collections: ["docs"], operations: ["read"], expires_unix_ms: now + 30 * 86400000, name: "ci", tenant_id: tenant,
  requests_per_second: 1000000, max_vectors: 0, max_vectors_per_collection: Number.MAX_SAFE_INTEGER,
});
for (const blank of [undefined, "", "  "]) {
  assert.deepEqual(u.credentialRequest({ ...credentialDraft, tenant_id: blank, requests_per_second: blank,
    max_vectors: blank, max_vectors_per_collection: blank }, now), u.credentialRequest(credentialDraft, now));
}
for (const empty of [null, "null", " null "]) {
  const result = u.credentialRequest({ ...credentialDraft, tenant_id: empty, requests_per_second: empty,
    max_vectors: empty, max_vectors_per_collection: empty }, now);
  for (const key of ["tenant_id", "requests_per_second", "max_vectors", "max_vectors_per_collection"]) assert.equal(result[key], null);
}
assert.equal(u.credentialRequest({ ...credentialDraft, requests_per_second: 1, max_vectors: 0, max_vectors_per_collection: 1 }, now).max_vectors, 0);
for (const bad of ["not-a-tenant", "a".repeat(31), "g".repeat(32), true, 0, {}, []])
  assert.throws(() => u.credentialRequest({ ...credentialDraft, tenant_id: bad }, now));
for (const key of ["requests_per_second", "max_vectors", "max_vectors_per_collection"]) {
  for (const bad of [-1, 1.5, NaN, Infinity, true, false, {}, [], "01", "1.0", "1e3", "0x10", "-0",
    "1,000", "undefined", Number.MAX_SAFE_INTEGER + 1, "9007199254740992", "18446744073709551615"]) {
    assert.throws(() => u.credentialRequest({ ...credentialDraft, [key]: bad }, now), `${key}: ${String(bad)}`);
  }
}
for (const bad of [0, "0", 1000001, "4294967295"])
  assert.throws(() => u.credentialRequest({ ...credentialDraft, requests_per_second: bad }, now));
const boundedCredential = u.credentialRequest({ ...credentialDraft, max_vectors: "9007199254740991" }, now);
assert.equal(JSON.parse(JSON.stringify(boundedCredential)).max_vectors, Number.MAX_SAFE_INTEGER);
assert.ok(!("requests_per_minute" in boundedCredential), "The server accepts requests_per_second, not requests_per_minute");
assert.equal(u.graceSeconds("86400"), 86400);
assert.throws(() => u.graceSeconds(-1));
// Import parsing and deterministic chunking.
const rows = u.importRows("rows.ndjson", '{"id":"1","vector":[1,0]}\n\n{"id":"2","vector":[0,1],"metadata":{"a":{"type":"string","value":"x"}}}\n');
assert.deepEqual(rows, [{ op: "put", id: "1", vector: [1, 0] }, { op: "put", id: "2", vector: [0, 1], metadata: { a: { type: "string", value: "x" } } }]);
assert.equal(u.importRows("rows.json", '[{"id":"3","vector":[1]}]')[0].id, "3");
assert.equal(u.importRows("rows.csv", 'id,vector\n4,"[1,2]"')[0].id, "4");
assert.throws(() => u.importRows("rows.json", '[{"id":"1","vector":[1],"token":"x"}]'));
assert.throws(() => u.importRows("rows.jsonl", '{"id":"01","vector":[1]}'));
assert.throws(() => u.importRows("rows.json", "[]"));
const many = Array.from({ length: 10 }, (_, i) => ({ op: "put", id: String(i), vector: [i] }));
const chunks = u.chunk(many, 4);
assert.deepEqual(chunks.map((c) => c.length), [4, 4, 2]);
assert.deepEqual(JSON.stringify(u.chunk(many, 4)), JSON.stringify(chunks));
assert.throws(() => u.chunk(many, 0));
assert.throws(() => u.chunk(many, 4097));
// Resume decisions come only from durable server status.
assert.equal(u.resumePlan({ state: "open", last_chunk: 0 }, 3).next, 1);
assert.equal(u.resumePlan({ state: "open", last_chunk: 0 }, 3).verify, null);
const middle = u.resumePlan({ state: "open", last_chunk: "2" }, 3);
assert.deepEqual([middle.action, middle.verify, middle.next], ["continue", 2, 3]);
assert.equal(u.resumePlan({ state: "open", last_chunk: 3 }, 3).action, "finish");
assert.equal(u.resumePlan({ state: "open", last_chunk: 4 }, 3).action, "mismatch");
assert.equal(u.resumePlan({ state: "aborted", last_chunk: 2 }, 3).action, "closed");
assert.match(u.resumePlan({ state: "aborted", last_chunk: 2 }, 3).message, /retained/);
assert.throws(() => u.resumePlan({ state: "weird", last_chunk: 0 }, 3));
assert.throws(() => u.resumePlan(null, 3));
// Saved query v2 is collection-bound and refuses credentials or another collection.
const v2 = { version: 2, name: "hybrid", collection: "docs", query: { vector: null, text: "vector database", k: 5, mode: "hybrid", metric: "cosine", ef_search: 64,
  filter: [], config: { m: 16, ef_construction: 100, seed: "42" }, explain: true, quantization: "none", rerank_multiplier: 4, vector_leg: "exact",
  fusion: { rrf_k: 60, vector_weight: 1, text_weight: 0.5, candidates: 100 } } };
assert.deepEqual(u.savedQueryV2(v2, 2, "docs"), v2);
assert.throws(() => u.savedQueryV2(v2, 2, "default"), /belongs to collection/);
assert.throws(() => u.savedQueryV2({ ...v2, token: "x" }, 2, "docs"));
assert.throws(() => u.savedQueryV2({ ...v2, query: { ...v2.query, text: "" } }, 2, "docs"));
assert.throws(() => u.savedQueryV2({ ...v2, query: { ...v2.query, mode: "exact" } }, 2, "docs"));
assert.throws(() => u.savedQueryV2({ ...v2, query: { ...v2.query, fusion: { ...v2.query.fusion, rrf_k: 0 } } }, 2, "docs"));
u.savedQueryV2({ ...v2, query: { ...v2.query, vector: [1, 0], mode: "auto" } }, 2, "docs");
assert.throws(() => u.savedQueryV2({ ...v2, query: { ...v2.query, vector: [1, 0, 0], mode: "auto" } }, 2, "docs"));
// Scheduler bodies: partial merges with server bounds; GET interval_ms maps to PUT interval_secs.
assert.deepEqual(u.autoMaintenance({ enabled: false }), { enabled: false });
u.autoMaintenance({ enabled: true, interval_secs: 60, stale_ratio: 0.2, rebuild_on_low_recall: false, min_recall: 0.9, quality_samples: 8, limits: {} });
for (const bad of [{}, { interval_secs: 9 }, { interval_secs: 3601 }, { stale_ratio: 0 }, { stale_ratio: 1.5 }, { enabled: "yes" }, { quality_samples: 33 }, { extra: 1 }, { limits: [] }])
  assert.throws(() => u.autoMaintenance(bad));
assert.deepEqual(u.autoMaintenanceForm({ enabled: false, interval_ms: 60000, stale_ratio: 0.2, rebuild_on_low_recall: false, min_recall: 0.9, quality_samples: 8, limits: { x: 1 } }),
  { enabled: false, stale_ratio: 0.2, rebuild_on_low_recall: false, min_recall: 0.9, quality_samples: 8, interval_secs: 60 });
u.backupSchedule({ enabled: false, interval_secs: 86400, retain_last: 7, retain_days: 0, retention_enabled: false, drill_every: 1, drill_max_bytes: 4294967296 });
u.backupSchedule({ retention_enabled: true });
for (const bad of [{}, { interval_secs: 299 }, { retain_last: 0 }, { retain_days: 3651 }, { drill_every: 101 }, { drill_max_bytes: 0 }, { retention_enabled: 1 }, { token: "x" }])
  assert.throws(() => u.backupSchedule(bad));
// Memory display: only *_bytes components plus their sum (graph_nodes is a count), never presented as process memory.
const memory = u.memoryRows({ memory: { graph_nodes: 9, graph_link_bytes: 100, code_bytes: 20, codebook_bytes: 8, buffer_vector_bytes: 0, resident_vector_bytes: 400 } });
assert.equal(memory.length, 6);
assert.deepEqual(memory.at(-1), ["sum of listed components (not process memory)", 528]);
assert.deepEqual(u.memoryRows({ memory: null }), []);
// Playground allowlist: collection routes allowed; credential minting and foreign paths are not.
assert.ok(u.requestAllowed("GET", "/v1/collections/docs/vectors/5"));
assert.ok(u.requestAllowed("POST", "/v1/collections/docs/search"));
assert.ok(u.requestAllowed("GET", "/v1/admin/credentials"));
for (const [method, path] of [["POST", "/v1/admin/credentials"], ["POST", "/v1/admin/credentials/abc/rotate"], ["GET", "//evil.example/v1/database"],
  ["GET", "/v1/collections/Docs"], ["DELETE", "/v1/collections/docs"], ["GET", "/v1/vectors?limit=1"], ["PUT", "/v1/collections/docs/settings"]]) assert.ok(!u.requestAllowed(method, path), `${method} ${path}`);
assert.ok(u.isSearch("/v1/search") && u.isSearch("/v1/collections/a/search") && !u.isSearch("/v1/collections/a/searchx"));
console.log("PASS EPIC-015 console helpers: collection routing, settings bounds, score direction, secret redaction/masking, credential requests, import parsing/chunking/resume, saved query v2, scheduler bodies, memory rows, playground allowlist");
// V5 request builders reject unsafe numbers, malformed vectors and secret-bearing URLs.
assert.deepEqual(u.expiry(""), {});
assert.deepEqual(u.expiry("0"), { ttl_ms: 0 });
assert.deepEqual(u.expiry("60000"), { ttl_ms: 60000 });
for (const value of ["-1", "1.5", "9007199254740992", "NaN", " ", "01"]) assert.throws(() => u.expiry(value));
assert.deepEqual(u.rerank("", "", "", 10, "exact"), {});
assert.deepEqual(u.rerank("exact", "50", "", 10, "exact"), { rerank: { mode: "exact", candidates: 50 } });
assert.deepEqual(u.rerank("mmr", "100", "0.25", 10, "auto"), { rerank: { mode: "mmr", candidates: 100, lambda: 0.25 } });
for (const args of [["mmr", 9, .5, 10, "exact"], ["exact", 1001, 0, 10, "exact"], ["mmr", 50, 1.1, 10, "exact"], ["exact", 50, 0, 10, "hybrid"], ["bad", 50, 0, 10, "exact"], ["mmr", 50, "", 10, "exact"]]) assert.throws(() => u.rerank(...args));
assert.deepEqual(u.sparseVector([[0, 1], [4294967295, -2]]), [[0, 1], [4294967295, -2]]);
for (const value of [[[1, 1], [1, 2]], [[2, 1], [1, 1]], [[-1, 1]], [[4294967296, 1]], [[1, Infinity]], [[1, 1e39]], [[1, "1"]], [[0, 1, 2]], {}]) assert.throws(() => u.sparseVector(value));
const hook = { url: "https://example.test/hook", secret: "random-signing-secret", collections: "docs,default", events: "put,metadata_update" };
assert.deepEqual(u.webhookRequest(hook), { url: hook.url, secret: hook.secret, collections: ["docs", "default"], events: ["put", "metadata_update"] });
assert.equal(u.webhookRequest({ ...hook, collections: "" }).collections, undefined);
for (const bad of [{ url: "javascript:alert(1)" }, { url: "https://user:pass@example.test" }, { url: "https://example.test/#secret" }, { secret: "short" }, { secret: "x".repeat(257) }, { events: "put,put" }, { events: "expire" }, { events: "graph_batch,graph_batch" }, { events: "graph_updated" }, { collections: "../docs" }, { collections: "docs,docs" }, { graphs: "Articles" }, { graphs: "1graph" }, { graphs: "a".repeat(64) }, { graphs: "articles,articles" }]) assert.throws(() => u.webhookRequest({ ...hook, ...bad }));
assert.deepEqual(u.webhookRequest({ ...hook, events: " , ", collections: "" }), { url: hook.url, secret: hook.secret }, "Blank filters are omitted so the server sends every event");
assert.deepEqual(u.webhookRequest({ ...hook, events: "graph_created, graph_batch,put", graphs: "articles, road-map_2" }),
  { url: hook.url, secret: hook.secret, events: ["graph_created", "graph_batch", "put"], collections: ["docs", "default"], graphs: ["articles", "road-map_2"] });
assert.equal(u.WEBHOOK_EVENTS.length, 10);
assert.equal(u.webhookRequest({ ...hook, events: u.WEBHOOK_EVENTS.join(",") }).events.length, 10);
assert.equal(u.webhookRequest({ ...hook, graphs: "a".repeat(63) }).graphs[0].length, 63);
assert.equal(u.webhookRequest({ ...hook, graphs: Array.from({ length: 64 }, (_, n) => `g${n}`).join(",") }).graphs.length, 64);
assert.throws(() => u.webhookRequest({ ...hook, graphs: Array.from({ length: 65 }, (_, n) => `g${n}`).join(",") }), /at most 64 graphs/);
assert.deepEqual(u.feedRows([
  { sequence: "7", collection: "docs", change: { type: "put", id: "42" } },
  { sequence: "8", collection: "docs", change: { type: "collection_created", dimensions: 3 } },
  { sequence: "9", collection: "", change: { type: "graph_batch", graph: "articles", revision: "12", operations: [{ op: "node_upserted" }, { op: "edge_upserted" }, { op: "node_upserted" }] } },
  { sequence: "10", collection: "", change: { type: "graph_dropped", graph: "articles" } },
  { sequence: "11", collection: "docs", change: { type: "vectors_updated" } },
]), [
  ["7", "put", "docs", null, "Record 42"],
  ["8", "collection_created", "docs", null, "3 dimensions"],
  ["9", "graph_batch", null, "articles", "Revision 12: 3 operations (2 node_upserted, 1 edge_upserted)"],
  ["10", "graph_dropped", null, "articles", null],
  ["11", "vectors_updated", "docs", null, null],
]);
assert.deepEqual(u.feedRows([{ sequence: "1", collection: "", change: { type: "graph_batch", graph: "g", revision: "1", operations: [{ op: "edge_deleted" }] } }])[0][4], "Revision 1: 1 operation (1 edge_deleted)");
assert.deepEqual(u.feedRows(undefined), []);
assert.deepEqual(u.feedRows([null]), [[null, "unknown", null, null, null]]);
assert.equal(u.webhookRequest({ ...hook, collections: Array.from({ length: 64 }, (_, n) => `c${n}`).join(",") }).collections.length, 64);
assert.throws(() => u.webhookRequest({ ...hook, collections: Array.from({ length: 65 }, (_, n) => `c${n}`).join(",") }));
assert.throws(() => u.webhookRequest({ ...hook, secret: "😀".repeat(65) }), "Signing secret limit is UTF-8 bytes");
assert.ok(!JSON.stringify(u.redact(u.webhookRequest(hook))).includes(hook.secret));
assert.deepEqual(u.pitrRequest("vlite-backup-1.vlite", "sequence", "42"), { backup: "vlite-backup-1.vlite", sequence: 42 });
assert.deepEqual(u.pitrRequest("vlite-backup-1.vlite", "timestamp_ms", "0"), { backup: "vlite-backup-1.vlite", timestamp_ms: 0 });
for (const args of [["../backup", "sequence", "1"], ["backup", "sequence", "9007199254740992"], ["backup", "sequence", "01"], ["backup", "sequence", ""], ["backup", "other", "1"]]) assert.throws(() => u.pitrRequest(...args));
assert.equal(u.feedPath("a+b/=&?", "100"), "/v1/changes?limit=100&cursor=a%2Bb%2F%3D%26%3F");
assert.throws(() => u.feedPath("", 0));
for (const suffix of ["/search/grouped", "/search/named", "/search/geo", "/search/dense-sparse", "/search/disk-ann", "/recommend", "/facets", "/stats/numeric", "/query"]) assert.ok(u.isSearch("/v1/collections/docs" + suffix));
for (const path of ["/v1/collections/docs/index/disk-ann", "/v1/admin/restores", "/v1/admin/webhooks", "/v1/collections/docs/query/extra"]) assert.ok(!u.isSearch(path));
const ranked = { ...v2, query: { ...v2.query, vector: [1, 0], mode: "exact", rerank: { mode: "mmr", candidates: 50, lambda: 0.5 } } };
assert.deepEqual(u.savedQueryV2(ranked, 2, "docs"), ranked);
assert.throws(() => u.savedQueryV2({ ...ranked, query: { ...ranked.query, rerank: { ...ranked.query.rerank, secret: "x" } } }, 2, "docs"));
assert.throws(() => u.savedQueryV2({ ...ranked, query: { ...ranked.query, mode: "hybrid" } }, 2, "docs"));
console.log("PASS V5 console helpers: TTL, exact/MMR reranking, sparse pairs, webhook validation/redaction including graph events and filters, feed rows, PITR safe integers, feed cursor encoding and read-only route classification");

// Exercise real registered admin handlers without a server, browser, timers or network.
async function adminHandlerChecks() {
  class Element {
    constructor() { this.value = ""; this.textContent = ""; this.children = []; this.dataset = {}; this.listeners = {}; this.selectors = new Map(); }
    addEventListener(name, action) { this.listeners[name] = action; }
    append(...children) { this.children.push(...children); }
    replaceChildren(...children) { this.children = children; }
    querySelector(selector) {
      if (!this.selectors.has(selector)) this.selectors.set(selector, new Element());
      return this.selectors.get(selector);
    }
  }
  const elements = new Map([...fs.readFileSync("src/dashboard/index.html", "utf8").matchAll(/\bid="([^"]+)"/g)]
    .map((match) => [match[1], new Element()]));
  let selectedOps = ["read"], listedKeys = [];
  const document = { getElementById: (name) => elements.get(name) ?? null, documentElement: new Element(),
    querySelectorAll: (selector) => selector === 'input[name="cred-op"]:checked' ? selectedOps.map((value) => ({ value })) : [],
    createElement: () => new Element() };
  const featureNames = ["scoped_credentials", "tenants", "credential_rate_limits", "credential_vector_quotas", "backup_restore", "backup_verify_drill", "webhooks"];
  const caps = { role: "admin", features: Object.fromEntries(featureNames.map((name) => [name, true])) };
  const requests = [], confirmations = [];
  let confirmed = true, failure = null, copied = null;
  const ctx = {
    capabilities: () => caps, feature: (name) => caps.features[name] === true, run: (action) => action(),
    confirm: async (message) => { confirmations.push(message); return confirmed; }, status() {},
    api: async (method, path, body, options) => {
      requests.push({ method, path, body, options });
      if (failure) throw failure;
      if (method === "GET" && path === "/v1/admin/credentials") return { items: listedKeys, capacity: 64 };
      return { token: tokenValue, credential: { id: "abcdefghijk2", name: "ci", collections: ["*"],
        operations: selectedOps, ...(selectedOps.includes("graph_read") ? { graphs: ["pool"] } : {}) } };
    },
  };
  const sandbox = { window: { addEventListener() {}, matchMedia: () => ({ addEventListener() {}, removeEventListener() {} }) },
    document, Node: Element, TextEncoder, URL, URLSearchParams, setInterval: () => 1, clearInterval() {},
    localStorage: { getItem: (key) => key === "vlite-studio-tour" ? "dismissed" : null },
    navigator: { clipboard: { writeText: async (value) => { copied = value; } } } };
  vm.runInNewContext(fs.readFileSync("src/dashboard/console-tools.js", "utf8"), sandbox);
  const tools = sandbox.window.VLiteTools.init(ctx);
  const field = (name, value) => { elements.get(name).value = value; };
  const click = (name) => elements.get(name).listeners.click();
  function draft() {
    field("cred-name", "ci"); field("cred-collections", "docs"); field("cred-days", "30");
    selectedOps = ["read"];
    for (const name of ["cred-graphs", "cred-tenant", "cred-rpm", "cred-max-vectors", "cred-max-vectors-per-collection"]) field(name, "");
  }
  draft();
  await click("cred-create");
  assert.equal(requests[0].options.sensitive, true);
  assert.equal(requests[0].options.data, false);
  for (const key of ["tenant_id", "requests_per_second", "max_vectors", "max_vectors_per_collection"])
    assert.ok(!(key in requests[0].body), `${key} blank input must not be sent`);
  assert.ok(!elements.get("cred-secret").textContent.includes(tokenValue));
  await click("cred-copy");
  assert.equal(copied, tokenValue);
  await click("cred-clear");
  await assert.rejects(click("cred-copy"), /No secret/);
  requests.length = 0;
  draft(); selectedOps = ["graph_write"]; field("cred-graphs", "");
  await assert.rejects(click("cred-create"), /Graph operations require/);
  assert.equal(requests.length, 0, "Invalid graph scope must not issue a request");
  draft(); selectedOps = ["graph_read", "graph_write"];
  field("cred-graphs", "pool"); field("cred-tenant", tenant); field("cred-collections", "");
  await click("cred-create");
  assert.deepEqual(Array.from(requests[0].body.graphs), ["pool"]);
  assert.deepEqual(Array.from(requests[0].body.collections), ["*"]);
  assert.match(confirmations.at(-1), /graphs \[pool\]/);
  assert.match(elements.get("cred-secret").textContent, /graphs \[pool\]/);
  listedKeys = [{ id: "abcdefghijk2", name: "ci", collections: ["*"], graphs: ["pool"], tenant_id: tenant,
    operations: selectedOps, state: "active", expires_unix_ms: now + 86400000 }];
  await click("cred-list");
  const keyRow = elements.get("cred-table").querySelector("tbody").children[0];
  assert.match(keyRow.children[2].textContent, /graphs \[pool\]/);
  field("cred-grace", "0");
  await keyRow.children[6].children[0].children[0].listeners.click();
  assert.match(confirmations.at(-1), /Scope stays unchanged:.*graphs \[pool\]/);
  assert.match(elements.get("cred-secret").textContent, /graphs \[pool\]/);
  assert.equal(requests.at(-2).path, "/v1/admin/credentials/abcdefghijk2/rotate");
  assert.deepEqual(Object.keys(requests.at(-2).body), ["grace_seconds"]);
  listedKeys = [];
  requests.length = 0;
  draft(); field("cred-tenant", tenant); field("cred-rpm", "12"); field("cred-max-vectors", "0"); field("cred-max-vectors-per-collection", "9");
  await click("cred-create");
  assert.equal(requests[0].body.tenant_id, tenant);
  assert.equal(requests[0].body.requests_per_second, 12);
  assert.equal(requests[0].body.max_vectors, 0);
  assert.equal(requests[0].body.max_vectors_per_collection, 9);
  assert.match(confirmations.at(-1), /rate 12 requests\/s; vectors 0 total \/ 9 per collection/);
  for (const [input, feature, value] of [["cred-tenant", "tenants", tenant], ["cred-rpm", "credential_rate_limits", "1"],
    ["cred-max-vectors", "credential_vector_quotas", "0"], ["cred-max-vectors-per-collection", "credential_vector_quotas", "0"]]) {
    requests.length = 0; draft(); field(input, value); caps.features[feature] = false;
    await assert.rejects(click("cred-create"), /Server feature/);
    assert.equal(requests.length, 0);
    caps.features[feature] = true;
  }
  requests.length = 0; draft(); confirmed = false;
  await click("cred-create");
  assert.equal(requests.length, 0, "Cancelled credential creation must not make a request");
  confirmed = true;
  caps.role = "read_only";
  await assert.rejects(click("cred-create"), /admin credential/);
  caps.role = "admin";
  caps.features.webhooks = false;
  await assert.rejects(click("webhook-list"), /Server feature webhooks/);
  caps.features.backup_restore = false;
  await assert.rejects(click("pitr-run"), /Server feature backup_restore/);
  await assert.rejects(click("list-backups"), /Server feature backup_restore/);
  caps.features.backup_restore = true;
  field("backup-name", "backup-1.vlite"); confirmed = false;
  await click("backup-verify");
  assert.match(confirmations.at(-1), /verification manifest/);
  assert.equal(requests.length, 0, "Cancelled backup verification must not make a request");
  confirmed = true; draft();
  failure = Object.assign(new Error("Durability is not confirmed"), { code: "credentials_commit_indeterminate" });
  await assert.rejects(click("cred-create"), /Durability/);
  assert.match(elements.get("cred-result").textContent, /change IS in effect/);
  await assert.rejects(click("cred-copy"), /No secret/);
  failure = null;
  tools.clear();
  for (const name of ["cred-graphs", "cred-tenant", "cred-rpm", "cred-max-vectors", "cred-max-vectors-per-collection"])
    assert.equal(elements.get(name).value, "");
  console.log("PASS admin handlers: blank/zero credential scopes and limits, capability/role gates, confirmations, secret clearing, indeterminate durability");
}
adminHandlerChecks().catch((error) => { console.error(error); process.exitCode = 1; });
