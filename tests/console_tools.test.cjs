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
