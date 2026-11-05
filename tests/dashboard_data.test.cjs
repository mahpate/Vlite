"use strict";
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const test = require("node:test");
const context = { window: {}, TextEncoder, atob, btoa, URLSearchParams };
vm.runInNewContext(fs.readFileSync("src/dashboard/console-tools.js", "utf8"), context);
const u = context.window.VLiteTools.utils;
const plain = value => JSON.parse(JSON.stringify(value));

test("KV paths and conditions preserve uint64 strings and reject invalid addresses", () => {
  assert.equal(u.kvAddress("cache", "preferences", "a&b"), "/v1/collections/cache/kv");
  assert.equal(u.kvAddress("cache", "a".repeat(255), "a".repeat(1024)), "/v1/collections/cache/kv");
  for (const args of [["../cache", "n"], ["cache", ""], ["cache", "é".repeat(128)], ["cache", "n", ""], ["cache", "n", "a".repeat(1025)]])
    assert.throws(() => u.kvAddress(...args));
  assert.deepEqual(plain(u.kvExpected("revision", "18446744073709551615")), { revision: "18446744073709551615" });
  assert.equal(u.kvExpected("revision", "18446744073709551615", true), "18446744073709551615");
  for (const value of ["01", "18446744073709551616", 1, "-1"]) assert.throws(() => u.kvExpected("revision", value));
  assert.equal(u.kvExpected("absent", ""), "absent");
  assert.equal(u.kvExpected("any", ""), "any");
});

test("KV text, JSON and binary encodings are canonical, bounded and lossless", () => {
  const text = "こんにちは 🌍";
  assert.equal(u.kvValue(text, "text"), Buffer.from(text).toString("base64"));
  assert.equal(u.kvValue('{"a":1}', "json"), "eyJhIjoxfQ==");
  assert.equal(u.kvValue("AP8=", "base64"), "AP8=");
  assert.equal(u.kvValue("", "base64"), "");
  for (const value of ["AB==", "AA", "AA-_", "AA==\n"]) assert.throws(() => u.kvValue(value, "base64"));
  assert.throws(() => u.kvValue("{", "json"));
  assert.throws(() => u.kvValue("", "unknown"));
  assert.equal(atob(u.kvValue("a".repeat(1047290), "text")).length, 1047290);
  assert.throws(() => u.kvValue("a".repeat(1047291), "text"));
});

const draft = {
  agent: "agent-a", user: "user-a", session: "session-a", seeds: "one\ntwo", text: "", vector: "",
  idempotency: "request-key", id: "summary", kind: "entity", content: "Remember this",
  source: "manual", sourceKind: "dashboard", transformation: "summary-v1",
  references: '[{"id":"one","revision":"18446744073709551615"}]', operation: "forget",
};
test("Memory requests use exact scope, bounded reads and revision-pinned logical deletion", () => {
  const recall = plain(u.memoryRequest("recall", draft, 123));
  assert.deepEqual(recall, { scope: { agent: "agent-a", user: "user-a", session: "session-a" }, entity_seeds: ["one", "two"], limits: { deadline_ms: 5000 } });
  const remember = plain(u.memoryRequest("remember", draft, 123));
  assert.equal(remember.items[0].observed_at_ms, "123");
  assert.equal(remember.idempotency_key, "request-key");
  assert.equal(remember.deadline_ms, 5000);
  assert.equal(remember.items[0].kind, "entity");
  const consolidate = plain(u.memoryRequest("consolidate", draft, 123));
  assert.equal(consolidate.output.kind, "episodic");
  assert.equal(consolidate.output.source.transformation_id, "summary-v1");
  assert.deepEqual(consolidate.inputs, [{ id: "one", revision: "18446744073709551615" }]);
  const forget = plain(u.memoryRequest("forget", draft, 123));
  assert.deepEqual(forget.target.ids, consolidate.inputs);
  assert.equal(forget.mode, "logical_delete");
  assert.ok(!("allow_job" in forget));
  assert.ok(!("deadline_ms" in u.memoryRequest("receipts/lookup", draft)));
  assert.throws(() => u.memoryRequest("remember", { ...draft, idempotency: "" }));
  assert.throws(() => u.memoryRequest("consolidate", { ...draft, transformation: "" }));
  assert.throws(() => u.memoryRequest("forget", { ...draft, references: '[{"id":"one","revision":1}]' }));
  assert.throws(() => u.memoryRequest("forget", { ...draft, references: '[{"id":"one","revision":"0"}]' }));
  assert.throws(() => u.memoryRequest("forget", { ...draft, references: '[{"id":"one","revision":"1","disposition":"logical_delete"}]' }));
  assert.throws(() => u.memoryRequest("recall", { ...draft, seeds: "" }));
  assert.throws(() => u.memoryRequest("receipts/lookup", { ...draft, operation: "remember" }));
});

test("custom reranking accepts only a provider alias and survives saved-query round trips", () => {
  for (const path of ["/v1/collections/docs/embed", "/v1/memory/agent/recall", "/v1/memory/agent/receipts/lookup"])
    assert.ok(u.isSearch(path), path);
  for (const path of ["/v1/memory/agent/remember", "/v1/memory/agent/forget", "/v1/memory/agent/consolidate"])
    assert.ok(!u.isSearch(path), path);
  assert.deepEqual(plain(u.rerank("custom", 50, "", 10, "exact", "my-ranker")),
    { rerank: { mode: "custom", candidates: 50, provider: "my-ranker" } });
  for (const provider of ["", "https://provider.test", "../alias", "bound:alias"])
    assert.throws(() => u.rerank("custom", 50, "", 10, "exact", provider));
  assert.throws(() => u.rerank("custom", 50, "", 10, "hybrid", "alias"));
  const saved = { version: 2, name: "ranked", collection: "docs", query: {
    vector: [1, 0], text: "", k: 10, mode: "exact", metric: "squared_l2", ef_search: 100,
    explain: false, filter: [], config: { m: 16, ef_construction: 100, seed: "0" },
    quantization: "none", rerank_multiplier: 4, vector_leg: "exact",
    fusion: { rrf_k: 60, vector_weight: 1, text_weight: 1, candidates: 100 },
    rerank: { mode: "custom", provider: "my-ranker", candidates: 50 },
  } };
  assert.deepEqual(plain(u.savedQueryV2(saved, 2, "docs")), saved);
  assert.throws(() => u.savedQueryV2({ ...saved, query: { ...saved.query, rerank: { ...saved.query.rerank, token: "secret" } } }, 2));
});
