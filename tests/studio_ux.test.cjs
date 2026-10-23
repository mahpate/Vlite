"use strict";
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const { test } = require("node:test");
const { spawnSync } = require("node:child_process");
global.window = {};
vm.runInThisContext(fs.readFileSync("src/dashboard/console-tools.js", "utf8"));
const { jsonDocuments, collectionDraft, codeSnippet, pca, responseSummary } = window.VLiteTools.utils;
const document = { id: "18446744073709551615", vector: [1, 0], metadata: { category: { type: "string", value: "a'\\\"\n雪" }, ok: { type: "boolean", value: true } }, payload: "AP8=" };

test("Write JSON normalizes base64 alias and preserves full-width IDs and typed fields", () => {
  const result = jsonDocuments(JSON.stringify([document]), 2);
  assert.equal(result.count, 1);
  assert.equal(result.invalid, 0);
  assert.equal(result.records[0].id, document.id);
  assert.equal(result.records[0].payload_base64, "AP8=");
  assert.equal(result.records[0].payload, undefined);
  assert.deepEqual(result.records[0].metadata, document.metadata);
});

test("Write JSON reports individual errors, rejects duplicates and never returns a partial writable preview", () => {
  const bad = [document, document, { id: 1, vector: [1, 0] }, { id: "2", vector: [0] }, { id: "3", vector: [1, 1], payload: "AB==" }, { id: "4", vector: [0, 1], metadata: { raw: "x" } }, { id: "5", vector: [0, 1], payload: "", payload_base64: "" }, { id: "6", vector: [0, 1], surprise: true }];
  const result = jsonDocuments(JSON.stringify(bad), 2);
  assert.equal(result.invalid, 7);
  assert.equal(result.valid, 1);
  assert.deepEqual(result.records, []);
  assert.match(result.errors[0], /Document 2: Duplicate ID/);
  assert.match(result.errors[2], /Dimension/);
  const many = jsonDocuments(JSON.stringify(Array.from({ length: 25 }, () => ({ id: "bad", vector: [1, 0] }))), 2);
  assert.equal(many.invalid, 25);
  assert.equal(many.errors.length, 20);
});

test("Write JSON bounds size, count, dimensionality, number precision and f32 values", () => {
  for (const source of ["null", "{}", "[]", "broken", " ".repeat(4 * 1024 * 1024 + 1), JSON.stringify(Array.from({ length: 10001 }, () => document))]) assert.throws(() => jsonDocuments(source, 2));
  assert.throws(() => jsonDocuments("[]", null), /Connect/);
  assert.equal(jsonDocuments('[{"id":"1","vector":[1e39,0]}]', 2).invalid, 1);
  assert.equal(jsonDocuments('[{"id":9007199254740993,"vector":[1,0]}]', 2).invalid, 1);
  const records = Array.from({ length: 101 }, (_, index) => ({ id: String(index), vector: [1, 0] }));
  assert.equal(jsonDocuments(JSON.stringify(records), 2).valid, 101);
});

test("Collection review validates declarations, advanced fields and overrides explicitly", () => {
  const body = collectionDraft({ name: " demo ", dimensions: "768", metric: "cosine", indexes: '[{"field":"category","kind":"eq"}]', settings: '{"metric":"squared_l2","text_fields":["title"]}' });
  assert.equal(body.name, "demo"); assert.equal(body.dimensions, 768); assert.equal(body.settings.metric, "cosine");
  assert.deepEqual(body.settings.text_fields, ["title"]);
  assert.equal(body.settings.metadata_indexes[0].kind, "eq");
  const draft = { name: "x", dimensions: "3", settings: "{}" };
  for (const extra of [{ dimensions: 0 }, { name: "../x" }, { metric: "bad" }, { indexes: '[{"field":"x","kind":"unknown"}]' }, { indexes: '[{"field":"x","kind":"eq"},{"field":"x","kind":"range"}]' }, { settings: '{"surprise":true}' }]) assert.throws(() => collectionDraft({ ...draft, ...extra }));
  assert.deepEqual(collectionDraft(draft), { name: "x", dimensions: 3, settings: {} });
});

test("Raw JSON summaries surface safe counts without replacing response details", () => {
  assert.match(responseSummary({ valid: 3, invalid: 1, errors: ["bad row"] }), /3 valid, 1 invalid/);
  assert.match(responseSummary({ hits: [{ id: "1" }, { id: "2" }] }), /2 hits returned/);
  assert.match(responseSummary({ state: "open", rows_committed: 42 }), /Job state: open; 42 rows committed/);
  assert.match(responseSummary([]), /0 items/);
  assert.match(responseSummary({ token: "secret" }), /1 field/);
  assert.doesNotMatch(responseSummary({ token: "secret" }), /secret/);
});

const query = { vector: [1, 0], k: 4, mode: "hybrid", text: "it's snowing 雪", filter: [], explain: true, fusion: { rrf_k: 60 }, rerank: { type: "mmr" } };
test("curl snippets shell-quote parameters and use environment credentials and correct routes", () => {
  const source = codeSnippet({ kind: "search", collection: "demo", body: query, language: "curl" });
  assert.match(source, /Bearer \$VLITE_TOKEN/);
  assert.match(source, /\$VLITE_BASE\/v1\/collections\/demo\/search/);
  assert.match(source, /it'\\''s snowing/);
  assert.match(source, /--fail-with-body/);
  assert.match(source, /\|\| exit 1/);
  const legacy = codeSnippet({ kind: "search", collection: "default", legacy: true, body: query, language: "curl" });
  assert.match(legacy, /\$VLITE_BASE\/v1\/search/);
  for (const extra of [{ collection: "evil/path" }, { language: "bash" }, { kind: "delete" }]) assert.throws(() => codeSnippet({ kind: "search", collection: "demo", body: query, language: "curl", ...extra }));
});

test("JavaScript SDK examples execute against mock SDK with unchanged search and upsert values", async () => {
  const calls = [];
  const target = { searchDetailed: async (body) => { calls.push(["search", body]); return {}; }, put: async (...args) => { calls.push(["put", ...args]); } };
  class Client { constructor(base, token) { assert.equal(base, "http://localhost:8080"); assert.equal(token, "environment-only"); } collection(name) { assert.equal(name, "demo"); return target; } }
  const context = vm.createContext({ Client, process: { env: { VLITE_BASE: "http://localhost:8080", VLITE_TOKEN: "environment-only" } }, console: { log() {} } });
  for (const options of [{ kind: "search", body: query }, { kind: "upsert", recordId: "42", body: { vector: [1, 0], metadata: document.metadata, ttl_ms: 50 } }]) {
    const source = codeSnippet({ ...options, collection: "demo", language: "javascript" });
    assert.ok(!source.includes("environment-only"));
    await vm.runInContext(`(async () => { ${source.replace('import { Client } from "@vlite/client";', "")} })()`, context);
  }
  assert.deepEqual(JSON.parse(JSON.stringify(calls)), [["search", query], ["put", "42", { vector: [1, 0], metadata: document.metadata, ttl_ms: 50 }]]);
});

test("Write snippets preserve batches of twenty and use SDK batch operations arrays", async () => {
  const operations = Array.from({ length: 41 }, (_, i) => ({ op: "put", id: String(i), vector: [1, 0], metadata: {}, payload_base64: "" }));
  const calls = [];
  class Client { collection(name) { assert.equal(name, "demo"); return { batch: async (items) => calls.push(items) }; } }
  const source = codeSnippet({ kind: "write", collection: "demo", body: { operations }, language: "javascript" });
  await vm.runInNewContext(`(async () => { ${source.replace('import { Client } from "@vlite/client";', "")} })()`, { Client, process: { env: {} } });
  assert.deepEqual(calls.map((items) => items.length), [20, 20, 1]);
  assert.deepEqual(JSON.parse(JSON.stringify(calls.flat())), operations);
  const curl = codeSnippet({ kind: "write", collection: "default", legacy: true, body: { operations }, language: "curl" });
  assert.equal((curl.match(/curl --fail-with-body/g) || []).length, 3);
  assert.match(curl, /\/v1\/batches/);
});

test("Python examples parse and call the real SDK-shaped interface, including booleans, quotes and Unicode", (t) => {
  const options = [{ kind: "search", body: query }, { kind: "upsert", recordId: "42", body: { vector: [1, 0], metadata: document.metadata } }, { kind: "write", body: { operations: [{ op: "put", id: "3", vector: [1, 0] }] } }];
  const sources = options.map((option) => codeSnippet({ ...option, collection: "demo", language: "python" }));
  const harness = `import ast,json,sys,types,os\nsources=json.load(sys.stdin)\ncalls=[]\nclass Client:\n def __init__(self,*args): pass\n def collection(self,name):\n  assert name=='demo'\n  return self\n def search_detailed(self,body): calls.append(['search',body]); return {}\n def put(self,id,body): calls.append(['put',id,body])\n def batch(self,body): calls.append(['batch',body])\nmodule=types.ModuleType('vlite_client')\nmodule.Client=Client\nsys.modules['vlite_client']=module\nos.environ['VLITE_BASE']='http://localhost:8080'\nos.environ['VLITE_TOKEN']='environment-only'\nfor source in sources:\n ast.parse(source)\n exec(source,{})\nprint(json.dumps(calls))\n`;
  const result = spawnSync("python", ["-c", harness], { input: JSON.stringify(sources), encoding: "utf8" });
  if (result.error?.code === "ENOENT") return t.skip("Python interpreter not available");
  assert.equal(result.status, 0, result.stderr);
  const calls = JSON.parse(result.stdout.trim().split(/\r?\n/).at(-1));
  assert.deepEqual(calls, [["search", query], ["put", "42", options[1].body], ["batch", options[2].body.operations]]);
});

test("snippet redaction removes credential-shaped fields/values", () => {
  const source = codeSnippet({ kind: "search", collection: "demo", language: "javascript", body: { ...query, token: "sensitive-credential", text: "vlk_abcdefghijkl_" + "x".repeat(30) } });
  assert.ok(!source.includes("sensitive-credential"));
  assert.ok(!source.includes("vlk_abcdefghijkl_"));
});

test("existing PCA keeps centered finite points, and rejects oversized or mismatched samples", () => {
  const points = pca([{ id: "1", vector: [0, 0] }, { id: "2", vector: [1, 0] }, { id: "3", vector: [2, 0] }]);
  assert.equal(points.length, 3);
  assert.ok(points.every((point) => Number.isFinite(point.x) && Number.isFinite(point.y)));
  assert.ok(Math.abs(points.reduce((sum, point) => sum + point.x, 0)) < 1e-10);
  assert.ok(Math.abs(points[0].x - points[2].x) > 1);
  assert.throws(() => pca(Array.from({ length: 2001 }, () => ({ id: "1", vector: [1] }))), /2000/);
  assert.throws(() => pca([{ id: "1", vector: [1] }, { id: "2", vector: [1, 0] }]), /Dimension/);
  assert.throws(() => pca([{ id: "1", vector: [NaN] }, { id: "2", vector: [1] }]), /finite/);
  const identical = pca([{ id: "1", vector: [1, 1] }, { id: "2", vector: [1, 1] }]);
  assert.ok(identical.every((point) => point.x === 0 && point.y === 0));
});
