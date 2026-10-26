"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const { init, utils } = require("../src/dashboard/graph.js");

const { graphPath, selector, selectorList, conditions, typedValue, limits, projection, cursor, body, request, errorText, openapiHasGraphs,
  availability, truncationText, indexText, propertiesText, normalize, summary, explainRows, statsCards, truncationRows, layout, STATS_ROUTE } = utils;

test("graph paths are encoded and require a graph", () => {
  assert.equal(graphPath("g", "nodes/get"), "/v1/graphs/g/nodes/get");
  assert.equal(graphPath("a b/c", "explain"), "/v1/graphs/a%20b%2Fc/explain");
  assert.throws(() => graphPath("", "traverse"), /Choose a graph/);
});

test("selectors send u64 IDs as canonical strings and keys verbatim", () => {
  assert.deepEqual(selector("id", " 18446744073709551615 "), { id: "18446744073709551615" });
  assert.deepEqual(selector("key", " n 1 "), { key: " n 1 " });
  for (const bad of ["", "01", "-1", "1.5", "18446744073709551616", "abc"]) assert.throws(() => selector("id", bad), /canonical unsigned/, bad);
  assert.throws(() => selector("key", ""), /key is required/);
  assert.deepEqual(selectorList("key", "n1\r\n\n n2 \n"), [{ key: "n1" }, { key: " n2 " }]);
  assert.throws(() => selectorList("id", " \n"), /at least one start node/);
});

test("conditions, typed values, limits, projection and cursor are validated before sending", () => {
  assert.deepEqual(conditions(""), []);
  assert.deepEqual(conditions('[{"op":"eq","field":"a","value":{"type":"string","value":"x"}}]').length, 1);
  assert.throws(() => conditions("{}"), /JSON array/);
  assert.throws(() => conditions("[1]"), /JSON array/);
  assert.throws(() => conditions("[nope"), /JSON array/);
  assert.deepEqual(typedValue("integer", "-12"), { type: "integer", value: "-12" });
  assert.deepEqual(typedValue("float", "1.5"), { type: "float", value: 1.5 });
  assert.deepEqual(typedValue("boolean", "true"), { type: "boolean", value: true });
  assert.throws(() => typedValue("integer", "1.5"));
  assert.throws(() => typedValue("float", "NaN"));
  assert.deepEqual(limits({ max_hops: "3", deadline_ms: "", max_output_nodes: "0" }), { max_hops: 3, max_output_nodes: 0 });
  assert.throws(() => limits({ max_hops: "-1" }), /max_hops/);
  assert.throws(() => limits({ max_hops: "9007199254740992" }), /max_hops/);
  assert.equal(projection("all"), "all");
  assert.equal(projection("none"), "none");
  assert.deepEqual(projection("only", "rank, title"), { only: ["rank", "title"] });
  assert.throws(() => projection("only", ""), /at least one/);
  assert.equal(cursor("0a1b"), "0a1b");
  for (const bad of ["0A", "abc", "zz"]) assert.throws(() => cursor(bad), /lowercase hex/);
});

test("bodies match the REST read contract for every operation", () => {
  assert.deepEqual(body("get-node", { selectorKind: "key", selector: "n1", projection: "only", projectionFields: "rank" }),
    { node: { key: "n1" }, projection: { only: ["rank"] } });
  assert.deepEqual(body("get-edge", { selectorKind: "id", selector: "7" }), { edge: { id: "7" } });
  assert.deepEqual(body("find-nodes", { label: " Doc ", eqField: "rank", eqType: "integer", eqValue: "2", limit: "2", cursor: "00ff" }),
    { label: "Doc", conditions: [{ op: "eq", field: "rank", value: { type: "integer", value: "2" } }], limit: 2, cursor: "00ff" });
  assert.deepEqual(body("find-nodes", {}), {});
  assert.deepEqual(body("neighbors", { selectorKind: "key", selector: "n2", direction: "both", types: "LINKS, CITES", edgeConditions: "" }),
    { node: { key: "n2" }, direction: "both", types: ["LINKS", "CITES"] });
  assert.deepEqual(body("traverse", { selectorKind: "key", start: "n1\nn2", direction: "out", returnLabels: "Doc", expandLabels: "",
    edgeConditions: '[{"op":"eq","field":"w","value":{"type":"integer","value":"1"}}]', includePaths: true, limits: { max_hops: "2" } }),
  { start: [{ key: "n1" }, { key: "n2" }], direction: "out", return_filter: { labels: ["Doc"] },
    edge_filter: [{ op: "eq", field: "w", value: { type: "integer", value: "1" } }], include_paths: true, limits: { max_hops: 2 } });
  assert.deepEqual(body("shortest-path", { selectorKind: "key", selector: "n1", destination: "n3" }),
    { source: { key: "n1" }, destination: { key: "n3" }, direction: "out" });
  assert.ok(!("limit" in body("traverse", { start: "n1", limit: "5" })), "page size is only sent for paged reads");
  assert.throws(() => body("neighbors", { selector: "n1", direction: "sideways" }), /Direction/);
  assert.throws(() => body("drop-graph", {}), /Unknown graph operation/);
});

test("requests target the read routes and explain adds the operation", () => {
  assert.deepEqual(request("neighbors", "g", { selector: "n1" }), { method: "POST", path: "/v1/graphs/g/neighbors", body: { node: { key: "n1" }, direction: "out" } });
  assert.deepEqual(request("shortest-path", "g", { selector: "a", destination: "b" }, true).body,
    { operation: "shortest-path", source: { key: "a" }, destination: { key: "b" }, direction: "out" });
  assert.equal(request("find-nodes", "g", {}, true).path, "/v1/graphs/g/explain");
  assert.equal(request("find-nodes", "g", {}).path, "/v1/graphs/g/nodes/find");
  assert.equal(STATS_ROUTE, "/v1/admin/graph/stats");
});

test("availability hides controls unless graphs are reachable", () => {
  assert.equal(availability(200, "", false), "available");
  assert.equal(availability(409, "graphs_not_enabled", false), "disabled");
  assert.equal(availability(404, "not_found", false), "absent");
  assert.equal(availability(403, "forbidden", false), "absent");
  assert.equal(availability(403, "forbidden_scope", true), "denied");
  assert.equal(availability(401, "unauthorized", false), "signed-out");
  assert.equal(availability(0, "network_error", false), "offline");
  assert.equal(availability(500, "internal_error", false), "error");
  assert.equal(openapiHasGraphs({ paths: { "/v1/graphs/{graph}/traverse": {} } }), true);
  assert.equal(openapiHasGraphs({ paths: { "/v1/collections": {} } }), false);
  assert.equal(openapiHasGraphs(null), false);
});

test("errors show the REST status, code and message", () => {
  assert.equal(errorText(422, { error: { code: "graph_limit_exceeds_ceiling", message: "max_hops above ceiling" } }),
    "HTTP 422 graph_limit_exceeds_ceiling: max_hops above ceiling");
  assert.equal(errorText(0, { error: { code: "timeout", message: "timeout" } }), "timeout: timeout");
  assert.equal(errorText(502, "bad gateway"), "HTTP 502: bad gateway");
  assert.equal(errorText(500, null), "HTTP 500");
});

test("responses are normalised into nodes, edges, paths and a summary", () => {
  const node = (id, key) => ({ id, key, labels: ["Doc"], properties: { rank: { type: "integer", value: id } }, revision: "1" });
  const edge = { id: "9", key: "e12", relationship: "LINKS", source: "1", source_key: "n1", target: "2", target_key: "n2", properties: {} };
  const traversal = normalize({ nodes: [{ depth: 0, node: node("1", "n1") }, { depth: 1, node: node("2", "n2") }], edges: [edge], paths: [{ nodes: ["1", "2"], edges: ["9"] }] });
  assert.equal(traversal.nodes[1].depth, 1);
  assert.equal(traversal.edges.length, 1);
  assert.equal(traversal.paths.length, 1);
  assert.equal(normalize({ node: null }).missing, true);
  const path = normalize({ shortest_path: { outcome: "found", path: { nodes: ["1", "2"], edges: ["9"] }, nodes: [node("1", "n1"), node("2", "n2")], edges: [edge] } });
  assert.equal(path.outcome, "found");
  assert.equal(path.paths.length, 1);
  assert.equal(normalize({ shortest_path: { outcome: "exhausted", truncation: ["visited_limit"] } }).outcomeDetail, "visited_limit");
  assert.equal(propertiesText({ rank: { type: "integer", value: "2" }, ok: { type: "boolean", value: true } }), "rank=2; ok=true");
  assert.equal(truncationText(["deadline", { degree_limit: { node_ids: ["1", "2"] } }]), "deadline; degree_limit (nodes 1, 2)");
  assert.equal(indexText(["label_index_scan", { adjacency_range_scan: { direction: "out", typed: true } }]), "label_index_scan, adjacency_range_scan (out, typed)");
  const line = summary({ complete: false, truncation: ["output_limit"], stats: { nodes_visited: 3, elapsed_ms: 1, indexes: [] }, cursor: "00", view_sequence: "5" });
  assert.match(line, /^Incomplete/);
  assert.match(line, /output_limit/);
  assert.match(line, /Next page/);
});

test("explain and admin stats are rendered as rows", () => {
  const rows = explainRows({ explain: { query: { operation: "traverse", start: [{ key: "n1" }] }, limits: { max_hops: 3 }, projection: "all", indexes: ["node_key_index"] } });
  assert.deepEqual(rows.map((row) => row[0]), ["Operation", "Query", "Limits", "Projection", "Indexes"]);
  assert.equal(rows[0][1], "traverse");
  assert.equal(rows[1][1], '{"start":[{"key":"n1"}]}');
  assert.equal(rows[2][1], "max_hops=3");
  const cards = statsCards({ read_guard: { p50_us: 3, p99_us: 9 }, write_guard: null, requests: { read: 4, write: 1 } });
  assert.deepEqual(cards.map((card) => card[1]), ["3 / 9 µs", "n/a", "4", "1"]);
  assert.deepEqual(truncationRows({ truncations: { output_limit: 2, deadline: 1 } }), [["deadline", "1"], ["output_limit", "2"]]);
});

test("layout places traversal depths in columns, caps nodes and keeps edges between drawn nodes", () => {
  const nodes = [{ id: "1", key: "a", depth: 0 }, { id: "2", key: "b", depth: 1 }, { id: "3", key: "c", depth: 1 }];
  const edges = [{ id: "e", relationship: "R", source: "1", target: "2" }, { id: "f", relationship: "R", source: "1", target: "3" }];
  const view = layout({ nodes, edges });
  assert.equal(view.nodes.length, 3);
  assert.equal(view.edges.length, 2);
  assert.ok(view.nodes[0].x < view.nodes[1].x);
  assert.equal(view.nodes[1].x, view.nodes[2].x);
  const circle = layout({ nodes: [], edges: [{ id: "e", relationship: "R", source: "1", source_key: "a", target: "2", target_key: "b" }] });
  assert.deepEqual(circle.nodes.map((node) => node.text), ["a", "b"]);
  const many = layout({ nodes: Array.from({ length: 50 }, (_, index) => ({ id: String(index) })), edges: [{ id: "x", source: "0", target: "49" }] }, { cap: 10 });
  assert.equal(many.nodes.length, 10);
  assert.equal(many.omitted, 40);
  assert.equal(many.edges.length, 0);
  for (const node of many.nodes) assert.ok(node.x >= 0 && node.x <= many.width && node.y >= 0 && node.y <= many.height);
});

test("init tolerates a page without the graph section", () => {
  assert.equal(init({ document: { getElementById: () => null } }), null);
});
