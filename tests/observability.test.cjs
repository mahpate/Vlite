"use strict";
const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const { parsePrometheus, counterRate, deriveRates, histogramQuantile, prometheusSummary, filterLogs, formatLog, mergeLogs } = require("../src/dashboard/observability.js");

test("Prometheus parser handles comments, escapes, labels, timestamps and histogram series", () => {
  const samples = parsePrometheus('# HELP requests Help\n# TYPE requests counter\nvlite_requests_total{route="/a,\\\"b",method="GET",target="x\\\\y\\nz"} 1.2e2 123\nlatency_bucket{le="+Inf"} 4\nlatency_sum .3\nlatency_count 4\n');
  assert.equal(samples.length, 4);
  assert.equal(samples[0].labels.route, '/a,"b');
  assert.equal(samples[0].labels.target, 'x\\y\nz');
  assert.equal(samples[0].value, 120);
  assert.equal(samples[2].value, .3);
  assert.equal(parsePrometheus('broken line\nmetric{bad="unterminated} 4\nmetric{a="x",a="y"} 4').length, 0);
  assert.equal(parsePrometheus('metric{__proto__="safe"} 4')[0].labels.__proto__, "safe");
  assert.equal(parsePrometheus('metric NaN\nmetric2 +Inf')[1].value, Infinity);
});
test("counter rates require two ordered scrapes and reject resets and missing values", () => {
  assert.equal(counterRate(40, 20, 5), 4);
  for (const [a, b, seconds] of [[null, 1, 2], [1, null, 2], [1, 2, 2], [3, 2, 0], [3, 2, -1]]) assert.equal(counterRate(a, b, seconds), null);
  const previous = { ts_ms: 1000, requests: { total: 10, errors: 1 }, search: { total: 4 } };
  const current = { ts_ms: 3000, requests: { total: 20, errors: 3 }, search: { total: 10 } };
  assert.deepEqual(deriveRates(current, previous), { requests: 5, errors: 1, errorPercent: 20, searches: 3 });
  assert.equal(deriveRates(current, null).requests, null);
  assert.equal(deriveRates({ ts_ms: 4000 }, current).errorPercent, null);
});
test("histogram quantiles interpolate buckets, not sums, with bounded infinity", () => {
  const buckets = [{ bound: .1, count: 5 }, { bound: .2, count: 9 }, { bound: Infinity, count: 10 }];
  assert.equal(histogramQuantile(buckets, .5), .1);
  assert.equal(histogramQuantile(buckets, .95), .2);
  assert.equal(histogramQuantile([], .5), null);
  assert.equal(histogramQuantile([{ bound: .1, count: 0 }], .5), null);
});
test("Prometheus fallback derives known legacy counters without inventing percentiles", () => {
  const samples = parsePrometheus('vlite_requests_total 100\nvlite_request_failures_total 4\nvlite_searches_total{strategy="exact"} 20\nvlite_searches_total{strategy="hnsw"} 10\nvlite_live_records 1000\nvlite_collections_total 3\nvlite_database_bytes 4096\nvlite_request_duration_seconds_total 8');
  const result = prometheusSummary(samples, 1234);
  assert.equal(result.requests.total, 100);
  assert.equal(result.search.total, 30);
  assert.equal(result.database.vectors, 1000);
  assert.equal(result.requests.p95_ms, null);
  assert.equal(result.ts_ms, 1234);
  const histogram = prometheusSummary(parsePrometheus('vlite_request_duration_seconds_bucket{le=".1"} 5\nvlite_request_duration_seconds_bucket{le=".2"} 9\nvlite_request_duration_seconds_bucket{le="+Inf"} 10'));
  assert.equal(histogram.requests.p50_ms, 100);
  assert.equal(histogram.requests.p95_ms, 200);
  assert.equal(prometheusSummary([]).database.vectors, null);
});
test("route counters aggregate statuses and explicit errors", () => {
  const result = prometheusSummary(parsePrometheus('vlite_requests_total{method="GET",route="/a",status="200"} 8\nvlite_requests_total{method="GET",route="/a",status="500"} 2'));
  assert.equal(result.requests.total, 10);
  assert.equal(result.requests.by_route[0].count, 10);
  assert.equal(result.requests.by_route[0].errors, 2);
});
test("log filtering matches severity, text, structured fields and exact targets", () => {
  const entries = [{ seq: 1, level: "debug", target: "http", message: "query" }, { seq: 2, level: "WARN", target: "db", message: "Slow", fields: { code: "DELAY" } }, { seq: 3, level: "error", target: "http", message: "Failed" }];
  assert.deepEqual(filterLogs(entries, { level: "warn" }).map((entry) => entry.seq), [2, 3]);
  assert.deepEqual(filterLogs(entries, { q: "delay", target: "db" }).map((entry) => entry.seq), [2]);
  assert.equal(filterLogs(entries, { target: "d" }).length, 0);
  assert.equal(filterLogs(null).length, 0);
});
test("log formatting keeps untrusted content literal and handles invalid timestamps", () => {
  const formatted = formatLog({ ts_ms: 0, message: '<img src=x onerror="alert(1)">', fields: { html: "<script>" } });
  assert.equal(formatted.time, "1970-01-01T00:00:00.000Z");
  assert.equal(formatted.message, '<img src=x onerror="alert(1)">');
  assert.equal(formatted.level, "info");
  assert.equal(formatLog({ ts_ms: "bad" }).time, "Unknown time");
  assert.equal(formatLog({}).fields, "{}");
});
test("bounded log merging deduplicates sequences without rounding string u64 IDs", () => {
  const entries = mergeLogs([{ seq: "18446744073709551614" }], [{ seq: "18446744073709551614" }, { seq: "18446744073709551615" }]);
  assert.equal(entries.length, 2);
  assert.deepEqual(mergeLogs([{ seq: 1 }, { seq: 2 }], [{ seq: 2 }, { seq: 3 }, { seq: 4 }], 2).map((entry) => entry.seq), [3, 4]);
});
test("dashboard loads a CSP-compatible module with no HTML injection sinks", () => {
  const html = fs.readFileSync("src/dashboard/index.html", "utf8");
  const js = fs.readFileSync("src/dashboard/observability.js", "utf8");
  assert.match(html, /id="observability"/);
  assert.match(html, /src="\/dashboard\/observability.js" defer/);
  assert.match(html, /href="#observability"/);
  assert.doesNotMatch(js, /\.innerHTML\s*=|insertAdjacentHTML|\.style\./);
});
test("route histograms preserve method boundaries and derive p95 in milliseconds", () => {
  const samples = parsePrometheus('vlite_request_duration_seconds_bucket{method="GET",route="/a",le=".1"} 5\nvlite_request_duration_seconds_bucket{method="GET",route="/a",le=".2"} 10\nvlite_request_duration_seconds_bucket{method="GET",route="/a",le="+Inf"} 10\nvlite_request_duration_seconds_bucket{method="POST",route="/a",le="1"} 10\nvlite_request_duration_seconds_bucket{method="POST",route="/a",le="+Inf"} 10');
  const rows = prometheusSummary(samples).requests.by_route;
  assert.equal(rows.find((row) => row.method === "GET").p95_ms, 190);
  assert.equal(rows.find((row) => row.method === "POST").p95_ms, 950);
});

test("read-only connection bridge reuses bearer auth without touching foreground request state", async () => {
  const vm = require("node:vm");
  const source = fs.readFileSync("src/dashboard/app.js", "utf8");
  const start = source.indexOf("  window.VLiteObservabilityConnection = {");
  const end = source.indexOf('  window.addEventListener("pagehide",', start);
  assert.ok(start >= 0 && end > start);
  const calls = [];
  const context = { window: {}, connected: true, token: "test-bearer", apiBase: "", AbortController, setTimeout, clearTimeout,
    fetch: async (url, options) => { calls.push({ url, options }); return { ok: true, json: async () => ({ ok: true }), text: async () => "text metrics" }; } };
  vm.runInNewContext(source.slice(start, end), context);
  const bridge = context.window.VLiteObservabilityConnection;
  assert.equal(bridge.connected(), true);
  await bridge.request("/v1/admin/metrics/summary");
  assert.equal(calls[0].options.method, "GET");
  assert.equal(calls[0].options.headers.Authorization, "Bearer test-bearer");
  assert.equal(calls[0].options.credentials, "omit");
  assert.equal(calls[0].options.redirect, "error");
  assert.equal(await bridge.request("/metrics", { text: true }), "text metrics");
  await assert.rejects(bridge.request("https://example.com/metrics"), /Unsupported/);
  await assert.rejects(bridge.request("/v1/admin/tokens"), /Unsupported/);
  context.fetch = async () => ({ ok: false, status: 403 });
  await assert.rejects(bridge.request("/v1/admin/logs"), (error) => error.status === 403);
  context.connected = false;
  await assert.rejects(bridge.request("/metrics"), /Sign in before/);
  assert.equal(bridge.token, undefined);
  // A password session sends the same-origin cookie and the request header, never an Authorization header.
  const sessionCalls = [];
  context.connected = true;
  context.token = "";
  context.fetch = async (url, options) => { sessionCalls.push(options); return { ok: true, json: async () => ({ ok: true }) }; };
  await bridge.request("/v1/admin/audit?limit=5");
  assert.equal(sessionCalls[0].credentials, "same-origin");
  assert.equal(sessionCalls[0].headers["X-VLite-Request"], "1");
  assert.equal(sessionCalls[0].headers.Authorization, undefined);
});
