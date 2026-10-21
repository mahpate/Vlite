// Live format v4 checks for the JavaScript client; started by tests/clients_e2e.py
// against a fresh `--create-format 4` server with VLITE_ADMIN_DIR and a read-only token.
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Client, ImportRejectedError, VLiteError } from "./index.js";

const url = process.env.VLITE_URL;
const client = new Client(url, process.env.VLITE_AUTH_TOKEN);
const fails = (status, code) => (e) => e instanceof VLiteError && e.status === status && (code === undefined || e.code === code);
const titled = (vector, title, category) => ({ vector, metadata: {
  title: { type: "string", value: title }, category: { type: "string", value: category } } });
const descending = (hits) => hits.every((h, i) => i === 0 || hits[i - 1].score >= h.score);

const caps = await client.capabilities();
assert.equal(caps.format_version, 4);
for (const flag of ["collections", "imports", "per_record_etags", "search_auto", "search_hybrid", "search_explain",
  "quantization", "scoped_credentials", "persistent_audit", "prometheus_metrics"]) {
  assert.equal(caps.features[flag], true, flag);
}

// Collections: independent dimensions and IDs.
await client.createCollection("docs", 3, { metric: "cosine", text_fields: ["title"],
  metadata_indexes: [{ field: "category", kind: "eq" }] });
await client.createCollection("wide", 768);
await assert.rejects(client.createCollection("docs", 3), fails(409, "collection_exists"));
await assert.rejects(client.collection("missing").get("1"), fails(404, "collection_not_found"));
const docs = client.collection("docs");
const wide = client.collection("wide");
await client.put("1", { vector: [0, 0, 1] });
await docs.batch([["1", [1, 0, 0], "red apple pie", "fruit"], ["2", [0.9, 0.1, 0], "green apple", "fruit"],
  ["3", [0, 1, 0], "banana bread", "fruit"], ["4", [0, 0, 1], "carrot soup", "veg"],
  ["5", [0.1, 0, 1], "apple carrot salad", "veg"]].map(([id, v, t, c]) => ({ op: "put", id, ...titled(v, t, c) })));
await wide.put("1", { vector: Array(768).fill(0.25) });
assert.deepEqual((await client.get("1")).vector, [0, 0, 1]);
assert.deepEqual((await docs.get("1")).vector, [1, 0, 0]);
assert.equal((await wide.get("1")).vector.length, 768);
await assert.rejects(wide.put("2", { vector: [1, 0, 0] }), fails(422));
assert.equal((await client.getCollection("docs")).live_records, 5);
assert.equal((await client.database()).records, 1);

// Per-record tags and revisions.
const first = await docs.getVersioned("1");
await docs.put("2", titled([0.9, 0.1, 0], "green apple", "fruit"));
await docs.put("1", titled([1, 0, 0], "red apple pie", "fruit"), { ifMatch: first.etag });
const second = await docs.getVersioned("1");
assert.ok(BigInt(second.record.revision) > BigInt(first.record.revision));
await assert.rejects(docs.delete("1", { ifMatch: first.etag }), fails(412, "precondition_failed"));
await assert.rejects(client.put("1", { vector: [0, 0, 1] }, { ifMatch: second.etag }), fails(412, "precondition_failed"));
console.log("PASS JavaScript live collections and revisions");

// Auto/explain, filters and hybrid.
const fruit = [{ op: "eq", field: "category", value: { type: "string", value: "fruit" } }];
const exact = await docs.search({ vector: [1, 0, 0], k: 3, filter: fruit });
assert.deepEqual(exact.map((h) => h.id), ["1", "2", "3"]);
const auto = await docs.searchDetailed({ vector: [1, 0, 0], k: 3, filter: fruit, mode: "auto", explain: true });
assert.deepEqual(auto.hits, exact);
assert.ok(["exact_scan", "exact_indexed"].includes(auto.plan.strategy), JSON.stringify(auto.plan));
assert.equal(typeof auto.stats.distance_evaluations, "number");
const text = await docs.search({ mode: "hybrid", text: "apple", k: 5 });
assert.deepEqual(new Set(text.map((h) => h.id)), new Set(["1", "2", "5"]));
assert.ok(descending(text), JSON.stringify(text));
const fused = await docs.search({ mode: "hybrid", vector: [0, 0, 1], text: "apple", k: 5, vector_leg: "exact" });
// Default fusion is weighted RRF (rrf_k 60, weights 1/1) sorted by score descending.
const rrf = (rank) => (rank === null ? 0 : 1 / (60 + rank));
for (const hit of fused) assert.ok(Math.abs(hit.score - rrf(hit.vector_rank) - rrf(hit.text_rank)) < 1e-12, JSON.stringify(hit));
assert.ok(descending(fused), JSON.stringify(fused));
const both = fused.find((hit) => hit.id === "5");
assert.ok(both && both.distance !== null && both.bm25 !== null, JSON.stringify(fused));
console.log("PASS JavaScript live auto/explain and hybrid search");

// Quantization, ANN quality and memory; no exact fallback in ready-only mode.
await assert.rejects(docs.search({ vector: [1, 0, 0], k: 1, mode: "approximate_ready" }), fails(409, "index_not_ready"));
await assert.rejects(docs.annQuality({ samples: 4, k: 2 }), fails(409, "index_not_ready"));
const updated = await client.setCollectionSettings("docs", { quantization: "scalar_u8", rerank_multiplier: 4 });
assert.equal(updated.persisted, true);
assert.equal(updated.collection.settings.metric, "cosine");
await docs.buildIndex();
const status = await docs.indexStatus();
assert.ok(status.ready && status.quantization === "scalar_u8" && (status.memory?.code_bytes ?? 0) > 0, JSON.stringify(status));
const ann = await docs.searchDetailed({ vector: [1, 0, 0], k: 2, mode: "approximate_ready", explain: true });
assert.deepEqual(ann.hits.map((h) => h.id), ["1", "2"]);
assert.ok(ann.stats.quantized_distance_evaluations > 0);
const quality = await docs.annQuality({ samples: 4, k: 2 });
assert.ok(quality.skipped_reason === null && quality.recall_at_k >= 0.5, JSON.stringify(quality));
console.log("PASS JavaScript live quantization and ANN quality");

// Imports: explicit replay, rejection and NDJSON resume.
const rows = [100, 101].map((n) => ({ op: "put", id: String(n), vector: [1, 0, 0] }));
const job = await client.beginImport("docs");
assert.equal((await client.importChunk(job.job, "1", rows)).outcome, "committed");
assert.equal((await client.importChunk(job.job, "1", rows)).outcome, "already_committed");
await assert.rejects(client.importChunk(job.job, "1", rows.slice(1)), fails(409, "import_conflict"));
const bad = [{ op: "put", id: "110", vector: [1, 0, 0] }, { op: "put", id: "111", vector: [1, 0] }];
await assert.rejects(client.importChunk(job.job, "2", bad),
  (e) => e instanceof ImportRejectedError && e.status === 422 && e.report.invalid.length === 1);
await assert.rejects(docs.get("110"), fails(404));
const file = join(mkdtempSync(join(tmpdir(), "vlite-js-import-")), "rows.ndjson");
writeFileSync(file, [...rows, { op: "put", id: "120", vector: [0, 1, 0] }, { op: "put", id: "121", vector: [0, 1, 0] }]
  .map((row) => JSON.stringify(row)).join("\n") + "\n");
// Resume replays chunk 1 (already_committed), commits chunk 2 and finishes.
const finished = await client.importNdjson(file, "docs", { chunkSize: 2, job: job.job });
assert.equal(finished.state, "finished");
assert.deepEqual((await docs.get("121")).vector, [0, 1, 0]);
console.log("PASS JavaScript live import replay and resume");

// Roles: read-only and scoped credentials.
const reader = new Client(url, process.env.VLITE_READ_ONLY_TOKEN).collection("docs");
assert.equal((await reader.search({ vector: [1, 0, 0], k: 1, mode: "auto" }))[0].id, "1");
await assert.rejects(reader.search({ vector: [1, 0, 0], k: 1, mode: "approximate" }), fails(403, "read_only"));
const issued = await client.createCredential({ name: "js", collections: ["docs"], operations: ["read", "search"],
  expires_unix_ms: Date.now() + 3_600_000 });
assert.match(issued.token, /^vlk_[a-z2-7]{12}_[A-Za-z0-9_-]{43}$/);
assert.ok(!JSON.stringify(await client.listCredentials()).includes(issued.token));
const scoped = new Client(url, issued.token);
assert.equal((await scoped.capabilities()).role, "scoped");
assert.equal((await scoped.collection("docs").get("1")).id, "1");
await assert.rejects(scoped.get("1"), fails(403, "forbidden_scope"));
await assert.rejects(scoped.collection("docs").put("9", { vector: [1, 0, 0] }), fails(403, "forbidden_scope"));
const rotated = await client.rotateCredential(issued.credential.id, { graceSeconds: 0 });
await assert.rejects(scoped.collection("docs").get("1"), fails(401));
assert.equal((await client.revokeCredential(rotated.credential.id)).credential.state, "revoked");
await assert.rejects(new Client(url, rotated.token).collection("docs").get("1"), fails(401));
const audit = await client.persistentAudit({ limit: 100 });
assert.ok(audit.items.length > 0 && !JSON.stringify(audit).includes(rotated.token));
assert.match(await client.prometheusMetrics(), /# TYPE/);
console.log("PASS JavaScript live roles, credentials and Prometheus");
console.log("PASS JavaScript live v4 REST client");
