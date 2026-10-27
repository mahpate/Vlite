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

test("editing requires graph_write and an exact graph binding, never vector write", () => {
  const { canWrite } = utils;
  assert.equal(canWrite({ role: "admin" }, "g"), true);
  for (const role of ["read_only", "unknown", "scoped"]) assert.equal(canWrite({ role }, "g"), false);
  assert.equal(canWrite({ role: "scoped", scope: { operations: ["write"], graphs: ["*"] } }, "g"), false);
  const caps = { role: "scoped", scope: { operations: ["graph_write"], graphs: ["g"] } };
  assert.equal(canWrite(caps, "g"), true);
  assert.equal(canWrite(caps, "other"), false);
});

test("guided edits match batch shapes and require revisions for updates and deletes", () => {
  const { mutation } = utils;
  const plain = (value) => JSON.parse(JSON.stringify(value));
  assert.deepEqual(plain(mutation("create_node", { key: "n", labels: "Doc, Guide", properties: [{ name: "rank", type: "integer", value: "9007199254740993" }] })),
    { ops: [{ op: "create_node", key: "n", labels: ["Doc", "Guide"], properties: { rank: { type: "integer", value: "9007199254740993" } } }] });
  assert.deepEqual(plain(mutation("patch_node", { key: "n", revision: "18446744073709551615", labels: "Doc", unset: "rank" })),
    { ops: [{ op: "patch_node", key: "n", add_labels: ["Doc"], remove_labels: [], expected: { revision: "18446744073709551615" }, set: {}, unset: ["rank"] }] });
  assert.deepEqual(plain(mutation("create_edge", { source: "n", destination: "m", relationship: "LINKS" })),
    { ops: [{ op: "create_edge", source: "n", destination: "m", relationship: "LINKS", properties: {} }] });
  assert.deepEqual(mutation("delete_edge", { edgeKind: "id", key: "18446744073709551615", revision: "4" }),
    { ops: [{ op: "delete_edge", edge: { id: "18446744073709551615" }, expected: { revision: "4" } }] });
  assert.deepEqual(mutation("attach", { key: "n", collection: "docs", record: "18446744073709551615" }),
    { ops: [{ op: "attach", node: "n", collection: "docs", record: "18446744073709551615" }] });
  assert.deepEqual(mutation("detach", { key: "n", collection: "docs" }),
    { ops: [{ op: "detach", node: "n", collection: "docs" }] });
  for (const action of ["patch_node", "delete_node", "patch_edge", "delete_edge"]) assert.throws(() => mutation(action, { key: "n" }), /revision/);
  assert.throws(() => mutation("create_edge", { source: "n" }), /Destination/);
  assert.throws(() => mutation("create_node", { key: "n", properties: [{ name: "", type: "string", value: "x" }] }), /name/);
  assert.throws(() => mutation("create_node", { key: "n", properties: [{ name: "x", type: "string" }, { name: "x", type: "string" }] }), /repeat/);
  assert.throws(() => mutation("unknown", {}), /supported/);
});

test("3D projection is bounded, deterministic and rotates without changing graph identities", () => {
  const result = { nodes: Array.from({ length: 60 }, (_, i) => ({ id: String(i), key: `n${i}` })), edges: [{ id: "1", source: "1", target: "2" }] };
  const view = layout(result, { mode: "3d", yaw: 25, pitch: 15 });
  assert.deepEqual(view, layout(result, { mode: "3d", yaw: 25, pitch: 15 }));
  assert.equal(view.nodes.length, 40);
  assert.equal(view.omitted, 20);
  assert.equal(view.edges.length, 1);
  for (const node of view.nodes) {
    assert.ok(Number.isFinite(node.z) && node.scale > 0);
    assert.ok(node.x > 0 && node.x < view.width && node.y > 0 && node.y < view.height);
  }
  assert.notDeepEqual(view.nodes, layout(result, { mode: "3d", yaw: 60 }).nodes);
  assert.deepEqual(layout({ nodes: [] }, { mode: "3d" }).nodes, []);
});

test("account graph knobs are validated and only differences are sent", () => {
  const { accountGraphNames, accountGraphRequest, accountGraphChanges, accountGraphFields, accountGraphView } = utils;
  assert.equal(accountGraphNames(" ", false), null);
  assert.deepEqual(accountGraphNames("[]", true), []);
  assert.deepEqual(accountGraphNames("g, h\nk", true), ["g", "h", "k"]);
  assert.deepEqual(accountGraphNames("*", true), ["*"]);
  assert.deepEqual(accountGraphNames("a".repeat(63), false), ["a".repeat(63)]);
  for (const bad of ["G", "* g", "g g", "1g", "a".repeat(29)]) assert.throws(() => accountGraphNames(bad, true), /no request sent/, bad);
  assert.throws(() => accountGraphNames(Array.from({ length: 65 }, (_, i) => `g${i}`).join(" "), false), /at most 64/);

  assert.deepEqual(accountGraphRequest({ graphs: "", access: "" }, "editor", false), {});
  assert.deepEqual(accountGraphRequest({ graphs: "g", access: "read" }, "editor", true), { graphs: ["g"], graph_access: "read" });
  assert.throws(() => accountGraphRequest({ access: "write" }, "viewer", false), /Viewers cannot write/);
  assert.throws(() => accountGraphRequest({ graphs: "g" }, "super_admin", false), /Super admins/);
  assert.throws(() => accountGraphRequest({ access: "admin" }, "editor", false), /none, read or write/);

  const current = { role: "editor", graphs: ["g"], graph_access: "read" };
  assert.deepEqual(accountGraphChanges(current, accountGraphFields(current), "editor", false), {});
  assert.deepEqual(accountGraphChanges(current, { graphs: "", access: "read" }, "editor", false), { graphs: null });
  assert.deepEqual(accountGraphChanges({ role: "editor" }, { graphs: "[]", access: "" }, "editor", false), { graphs: [] });
  assert.deepEqual(accountGraphFields({ graphs: [] }), { graphs: "[]", access: "" });

  assert.equal(accountGraphView({ role: "super_admin" }), "every graph");
  assert.equal(accountGraphView({ role: "editor" }), "read and write: every graph");
  assert.equal(accountGraphView({ role: "viewer", tenant_id: "a".repeat(32) }), "read: every tenant graph");
  assert.equal(accountGraphView(current), "read: g");
  assert.equal(accountGraphView({ role: "editor", graphs: [] }), "no graph access");
  assert.equal(accountGraphView({ role: "viewer", graph_access: "none" }), "no graph access");
});