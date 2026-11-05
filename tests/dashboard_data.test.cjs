"use strict";
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const test = require("node:test");
const context = { window: {}, TextEncoder, TextDecoder, atob, btoa, URLSearchParams };
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

test("KV presentation refuses lossy textarea encodings and converts formats losslessly", () => {
  for (const value of ["a\rb", "a\r\nb", "\uFEFFhello", "\u0000", "é\r\n"]) {
    const encoded = Buffer.from(value).toString("base64");
    assert.throws(() => u.kvPresentation(encoded, "text"));
    assert.deepEqual(plain(u.kvPresentation(encoded, "base64")), { value: encoded, format: "base64" });
  }
  assert.throws(() => u.kvPresentation("AP8="));
  assert.equal(u.kvPresentation(u.kvValue("a\nb", "text")).value, "a\nb");
  assert.equal(u.kvAddress("cache", "n", "line\nbreak"), "/v1/collections/cache/kv");
  const encoded = u.kvValue('{"hello":"世界"}', "json");
  assert.equal(u.kvPresentation(encoded, "json").value, '{"hello":"世界"}');
  assert.equal(u.kvValue(u.kvPresentation(encoded).value, "text"), encoded);
  assert.throws(() => u.kvPresentation(u.kvValue("not json", "text"), "json"));
  assert.throws(() => u.kvValue("", "json"), /Enter valid JSON, such as null/);
  assert.match(u.displayKey("  line\nbreak  "), /␠␠line\\nbreak␠␠/);
});

test("Memory observation timestamps require server time, never the device clock", () => {
  assert.equal(u.serverTime("Thu, 05 Nov 2026 22:00:00 GMT"), 1793915998000);
  assert.equal(u.serverTime(null), null);
});

const draft = {
  agent: "agent-a", user: "user-a", session: "session-a", seeds: "one\ntwo", text: "", vector: "",
  idempotency: "request-key", id: "summary", kind: "entity", content: "Remember this",
  source: "manual", sourceKind: "dashboard", transformation: "summary-v1",
  references: '[{"id":"one","revision":"18446744073709551615"}]', operation: "forget",
};
test("MemoryAI ranking guidance matches the exact/MMR cosine contract", () => {
  const html = fs.readFileSync("src/dashboard/index.html", "utf8");
  const guide = fs.readFileSync("docs/CONSOLE.md", "utf8");
  assert.match(html, /Weighted ranking uses squared Euclidean vector distance/);
  assert.match(html, /exact and MMR ranking use cosine distance/);
  assert.match(guide, /exact\/MMR use cosine distance/);
  assert.match(html, /Exact allows optional query text; MMR does not support text/);
  assert.doesNotMatch(html, /Distance is squared Euclidean, not cosine/);
});

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
  const mmr = plain(u.memoryRequest("recall", { ...draft, seeds: "", vector: "[2,0]", ranking: "mmr", limit: "50", lambda: "0.25" }));
  assert.deepEqual(mmr.ranking, { selection: "mmr", weights: [1, 0, 0], lambda: .25 });
  assert.equal(mmr.limit, 50);
  const exact = u.memoryRequest("recall", { ...draft, seeds: "", vector: "[2,0]", ranking: "exact" });
  assert.equal(exact.ranking.selection, "exact");
  assert.equal(u.memoryRequest("recall", { ...draft, seeds: "", vector: "[2,0]", text: "query", ranking: "exact" }).text, "query");
  assert.throws(() => u.memoryRequest("remember", draft), /Server time is unavailable/);
  const refs = count => JSON.stringify(Array.from({ length: count }, (_, n) => ({ id: `m${n}`, revision: "1" })));
  assert.equal(u.memoryRequest("consolidate", { ...draft, references: refs(64) }, 123).inputs.length, 64);
  for (const count of [65, 256]) assert.throws(() =>
    u.memoryRequest("consolidate", { ...draft, references: refs(count) }, 123), /1\.\.64/);
  assert.throws(() => u.memoryRequest("recall", { ...draft, vector: "[1]", ranking: "exact" }));
  assert.throws(() => u.memoryRequest("recall", { ...draft, seeds: "", text: "query", vector: "[1]", ranking: "mmr", lambda: .5 }));
  assert.throws(() => u.memoryRequest("recall", { ...draft, seeds: "", vector: "[1]", ranking: "mmr", lambda: "" }));
  assert.throws(() => u.memoryRequest("recall", { ...draft, seeds: "", vector: "[1]", ranking: "mmr", lambda: 1.1 }));
  assert.throws(() => u.memoryRequest("recall", { ...draft, limit: "101" }));
});

test("named vector declarations and replacements validate complete wire shapes", () => {
  const declaration = { name: "text", dimensions: 65536, metric: "cosine", active_version: "v1", history: 64 };
  assert.deepEqual(plain(u.vectorDeclaration(declaration)), declaration);
  for (const change of [{ dimensions: 0 }, { dimensions: 65537 }, { history: 65 }, { metric: "dot" },
    { name: "" }, { active_version: "" }, { name: "é".repeat(129) }, { secret: "no" }])
    assert.throws(() => u.vectorDeclaration({ ...declaration, ...change }));
  const values = { dense: { text: [{ version: "v1", vector: [1, 0] }] }, sparse: { terms: [[1, .5], [4294967295, 1]] } };
  assert.deepEqual(plain(u.namedVectorValues(values)), values);
  assert.deepEqual(plain(u.namedVectorValues({ dense: {}, sparse: {} })), { dense: {}, sparse: {} });
  for (const invalid of [{}, { dense: [] }, { dense: { text: [{ version: "v1", vector: [Infinity] }] } },
    { dense: { text: [{ version: "v1", vector: [] }] } }, { dense: { text: [{ version: "v1", vector: [1], dimensions: 1 }] } },
    { dense: { text: [{ version: "v1", vector: [1] }, { version: "v1", vector: [2] }] } },
    { sparse: { terms: [[2, 1], [1, 1]] } }, { sparse: { terms: [[0, 1e39]] } },
    { dense: Object.fromEntries(Array.from({ length: 33 }, (_, i) => ["name" + i, []])) }, { token: "secret" }])
    assert.throws(() => u.namedVectorValues(invalid));
});

test("custom reranking accepts only a provider alias and survives saved-query round trips", () => {
  for (const path of ["/v1/collections/docs/embed", "/v1/memory/agent/recall", "/v1/memory/agent/receipts/lookup"])
    assert.ok(u.isSearch(path), path);
  for (const path of ["/v1/memory/agent/remember", "/v1/memory/agent/forget", "/v1/memory/agent/consolidate"])
    assert.ok(!u.isSearch(path), path);
  assert.deepEqual(plain(u.rerank("custom", 50, "", 10, "exact", "my-ranker")),
    { rerank: { mode: "custom", candidates: 50, provider: "my-ranker" } });
  for (const provider of ["", "https://provider.test", "../alias", "invalid:alias", "a".repeat(257)])
    assert.throws(() => u.rerank("custom", 50, "", 10, "exact", provider));
  for (const provider of ["Managed.Ranker_V2-1", "0", "a".repeat(256)])
    assert.equal(u.providerAlias(provider), u.rerank("custom", 50, "", 10, "exact", provider).rerank.provider);
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
