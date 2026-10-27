// Property-graph API (compiled into every library since 0.5.1). Rejection tests only run against an older library
// built without graph. Set
// VLITE_TEST_EXPECT_GRAPH=1 or 0 to assert which variant VLITE_TEST_LIBRARY is.
import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdir, rm } from "node:fs/promises";
import { randomUUID } from "node:crypto";
import { fileURLToPath } from "node:url";
import { join } from "node:path";
import { NativeLibrary, EmbeddedError } from "./index.js";

const nativeOptions = { libraryPath: process.env.VLITE_TEST_LIBRARY };
const code = expected => error => error instanceof EmbeddedError && error.code === expected;
const person = name => ({ name: { type: "string", value: name } });
const probe = new NativeLibrary(nativeOptions);
const GRAPH = await probe.supportsGraph();
await probe.close();

async function fixture(fn) {
  const directory = fileURLToPath(new URL(`./test-data-${randomUUID()}/`, import.meta.url));
  await mkdir(directory);
  const native = new NativeLibrary(nativeOptions);
  try { await fn(native, directory); }
  finally { await native.close(); await rm(directory, { recursive: true }); }
}

test("capabilities report the loaded library variant", async () => fixture(async native => {
  assert.deepEqual(await native.capabilities(), { graph: GRAPH });
  const expected = process.env.VLITE_TEST_EXPECT_GRAPH;
  if (expected !== undefined) assert.equal(GRAPH, expected === "1");
}));

async function populate(native, directory) {
  const path = join(directory, "graph.vlite");
  const db = await native.create(path, 2);
  assert.equal(await db.isGraphEnabled(), true);
  assert.deepEqual(await db.listGraphs(), []);
  assert.deepEqual(await db.createGraph("g"), { name: "g", node_count: 0, edge_count: 0 });
  await db.put("7", [1, 0]);
  const result = await db.graphWriteBatch("g", [
    { op: "create_node", key: "a", labels: ["Person"], properties: person("Ada") },
    { op: "create_node", key: "b", labels: ["Person"], properties: person("Bob") },
    { op: "create_node", key: "c", labels: ["City"] },
    { op: "create_edge", key: "ab", source: "a", relationship: "KNOWS", destination: "b" },
    { op: "create_edge", source: "b", relationship: "LIVES_IN", destination: "c" },
    { op: "attach", node: "a", collection: "default", record: "7" },
    { op: "put_vector", collection: "default", id: "8", vector: [0, 1], payload_base64: "AP8=" },
  ]);
  return { path, db, result };
}

test("graph create, atomic write, reads, explain and read-only reopen", { skip: !GRAPH && "library built without graph" }, async () => fixture(async (native, directory) => {
  const { path, db, result } = await populate(native, directory);
  assert.equal(result.graph, "g");
  assert.equal(typeof result.revision, "string");
  assert.equal(result.outcomes.length, 7);
  assert.equal(result.outcomes[0].node.created, true);
  assert.equal(result.outcomes[6], "vector");
  assert.equal((await db.get("8")).payload_base64, "AP8=");
  const nodeA = result.outcomes[0].node.id;

  const got = await db.graphGetNode("g", BigInt(nodeA));
  assert.equal(got.node.key, "a");
  assert.deepEqual(got.node.properties, person("Ada"));
  assert.equal(got.complete, true);
  assert.equal(typeof got.view_sequence, "string");
  assert.equal((await db.graphGetNode("g", Number(nodeA))).node.key, "a");
  assert.equal((await db.graphGetNode("g", { id: nodeA })).node.key, "a");
  assert.deepEqual((await db.graphGetNode("g", "a", { projection: "none" })).node.properties, {});
  assert.equal((await db.graphGetNode("g", "missing")).node, null);
  assert.equal((await db.graphGetEdge("g", "ab")).edge.target_key, "b");

  const page = await db.graphFindNodes("g", { label: "Person", limit: 1 });
  assert.equal(page.nodes.length, 1);
  const following = await db.graphFindNodes("g", { label: "Person", limit: 1, cursor: page.cursor });
  assert.notEqual(following.nodes[0].key, page.nodes[0].key);
  const matched = await db.graphFindNodes("g", { conditions: [{ op: "eq", field: "name", value: { type: "string", value: "Bob" } }] });
  assert.deepEqual(matched.nodes.map(node => node.key), ["b"]);

  assert.equal((await db.graphNeighbors("g", "b", { direction: "both" })).edges.length, 2);
  assert.equal((await db.graphNeighbors("g", "a", { types: ["LIVES_IN"] })).edges.length, 0);
  const traversal = await db.graphTraverse("g", ["a"], { includePaths: true, limits: { maxHops: 2 } });
  assert.equal(traversal.nodes.length, 3);
  assert.ok(traversal.paths.length > 0);
  const returned = await db.graphTraverse("g", ["a"], { returnFilter: { labels: ["City"] } });
  assert.deepEqual(returned.nodes.map(item => item.node.key), ["c"]);
  const found = (await db.graphShortestPath("g", "a", "c")).shortest_path;
  assert.equal(found.outcome, "found");
  assert.equal(found.path.edges.length, 2);
  assert.equal((await db.graphShortestPath("g", "c", "a")).shortest_path.outcome, "no_path_within_hops");
  assert.equal((await db.graphShortestPath("g", "c", "a", { direction: "in" })).shortest_path.outcome, "found");

  const plan = await db.graphExplain({ op: "neighbors", graph: "g", node: { key: "a" }, types: ["KNOWS"] });
  assert.equal(plan.query.op, "neighbors");
  assert.equal(plan.projection, "all");
  assert.ok(plan.indexes.length > 0);

  await db.createGraph("empty");
  assert.equal(await db.dropGraph("empty"), undefined);
  assert.deepEqual((await db.listGraphs()).map(graph => graph.name), ["g"]);
  await db.close();

  const reader = await native.open(path, { readOnly: true });
  assert.deepEqual(await reader.listGraphs(), [{ name: "g", node_count: 3, edge_count: 2 }]);
  assert.deepEqual((await reader.graphGetNode("g", "b")).node.properties, person("Bob"));
  await assert.rejects(reader.graphWriteBatch("g", [{ op: "create_node", key: "z" }]), code("read_only"));
  await reader.close();
}));

test("change feed reports graph events in log order", { skip: !GRAPH && "library built without graph" }, async () => fixture(async (native, directory) => {
  const { db, result } = await populate(native, directory);
  await db.createGraph("empty");
  await db.dropGraph("empty");
  const { changes } = await db.changesAfter();
  assert.deepEqual(changes.map(change => change.kind),
    ["graph_created", "put", "graph_batch", "put", "graph_created", "graph_dropped"]);
  assert.equal(changes[0].graph, "g");
  assert.equal(changes[0].collection, "");
  assert.equal(changes[2].revision, result.revision);
  assert.deepEqual(changes[2].operations.map(op => op.op),
    ["node_upserted", "node_upserted", "node_upserted", "edge_upserted", "edge_upserted", "attached"]);
  assert.deepEqual(changes[2].operations[0].properties, person("Ada"));
  assert.equal(changes[2].sequence, changes[3].sequence);
  assert.equal(changes[5].graph, "empty");
  await db.close();
}));

test("graph error codes, atomic rejection and selector validation", { skip: !GRAPH && "library built without graph" }, async () => fixture(async (native, directory) => {
  const { path, db } = await populate(native, directory);
  await assert.rejects(native.create(path, 2), code("io_error"));
  await assert.rejects(db.createGraph("g"), code("graph_exists"));
  await assert.rejects(db.createGraph("Bad"), code("invalid_graph_name"));
  await assert.rejects(db.graphGetNode("nope", "a"), code("graph_not_found"));
  await assert.rejects(db.dropGraph("g"), code("graph_not_empty"));
  await assert.rejects(db.graphWriteBatch("g", [{ op: "create_node", key: "d" }, { op: "create_node", key: "a" }]), code("graph_key_exists"));
  assert.equal((await db.graphGetNode("g", "d")).node, null);
  await assert.rejects(db.graphWriteBatch("g", [{ op: "create_node", key: "x", extra: 1 }]), code("invalid_input"));
  await assert.rejects(db.graphFindNodes("g", { cursor: "XYZ" }), code("invalid_input"));
  await assert.rejects(db.graphGetNode("g", { id: "07" }), code("invalid_input"));
  await assert.rejects(db.graphExplain({ op: "list_graphs" }), code("invalid_input"));
  await assert.rejects(db.graphTraverse("g", ["a"], { limits: { maxHops: 1000000 } }), code("graph_limit_exceeds_ceiling"));
  await assert.rejects(db.graphGetNode("g", 1.5), TypeError);
  await assert.rejects(db.graphGetNode("g", -1n), TypeError);
  await assert.rejects(db.graphTraverse("g", "a"), TypeError);

  const plain = await native.create(join(directory, "plain.vlite"), 2, { format: 4 });
  assert.equal(await plain.isGraphEnabled(), false);
  await assert.rejects(plain.listGraphs(), code("graphs_not_enabled"));
  await assert.rejects(plain.createGraph("g"), code("graphs_not_enabled"));
  await plain.close();
  await db.close();
  await assert.rejects(db.listGraphs(), code("closed"));
}));

test("encrypted graph files require credentials", { skip: !GRAPH && "library built without graph" }, async () => fixture(async (native, directory) => {
  const path = join(directory, "secret.vlite");
  const key = Uint8Array.from({ length: 32 }, (_, index) => index);
  const db = await native.create(path, 2, { key, commitTimestamps: true });
  await db.createGraph("g");
  await db.graphWriteBatch("g", [{ op: "create_node", key: "a" }]);
  assert.equal(await db.isEncrypted(), true);
  await db.close();
  await assert.rejects(native.open(path), code("encryption_key_required"));
  const reopened = await native.open(path, { key });
  assert.equal((await reopened.listGraphs())[0].node_count, 1);
  await reopened.close();
}));

test("library shutdown closes graph-created handles", { skip: !GRAPH && "library built without graph" }, async () => fixture(async (_, directory) => {
  const path = join(directory, "left-open.vlite");
  const first = new NativeLibrary(nativeOptions);
  const db = await first.create(path, 2);
  await db.createGraph("g");
  await first.close();
  const second = new NativeLibrary(nativeOptions);
  try {
    const reopened = await second.open(path);
    assert.deepEqual(await reopened.listGraphs(), [{ name: "g", node_count: 0, edge_count: 0 }]);
  } finally { await second.close(); }
}));

test("every graph method rejects with unsupported without graph", { skip: GRAPH && "library built with graph" }, async () => fixture(async (native, directory) => {
  const db = await native.create(join(directory, "plain.vlite"), 2);
  for (const action of [
    () => db.isGraphEnabled(), () => db.listGraphs(), () => db.createGraph("g"), () => db.dropGraph("g"),
    () => db.graphWriteBatch("g", [{ op: "create_node", key: "a" }]),
    () => db.graphGetNode("g", "a"), () => db.graphGetEdge("g", 1),
    () => db.graphFindNodes("g"), () => db.graphNeighbors("g", "a"),
    () => db.graphTraverse("g", ["a"]), () => db.graphShortestPath("g", "a", "b"),
    () => db.graphExplain({ op: "get_node", graph: "g", node: { key: "a" } }),
  ]) {
    await assert.rejects(action(), code("unsupported"));
  }
  await db.put("1", [1, 0]);
  assert.equal((await db.get("1")).id, "1");
  await db.close();
}));
