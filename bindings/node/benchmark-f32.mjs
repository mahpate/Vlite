import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile, realpath, writeFile } from "node:fs/promises";
import { execFileSync } from "node:child_process";
import { cpus } from "node:os";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { performance } from "node:perf_hooks";
import { fileURLToPath } from "node:url";
import { EmbeddedError, NativeLibrary } from "./index.js";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "..", "..");
const name = process.platform === "win32" ? "vlite_ffi.dll"
  : process.platform === "darwin" ? "libvlite_ffi.dylib" : "libvlite_ffi.so";
const prebuild = join(here, "prebuilds", `${process.platform}-${process.arch}`);
const libraryPath = await realpath(process.env.VLITE_TEST_LIBRARY ?? join(prebuild, name));
const addonPath = await realpath(process.env.VLITE_TEST_ADDON ?? join(prebuild, "vlite_node.node"));
const addon = createRequire(import.meta.url)(addonPath);
addon.load(libraryPath);
assert.equal(addon.typedAvailable(), true, "Benchmark requires both typed C exports, not JSON fallback");
const output = process.argv[2] ? resolve(process.argv[2]) : undefined;
const git = (...args) => execFileSync("git", args, { cwd: root, encoding: "utf8" }).trim();
const hash = async (path) => createHash("sha256").update(await readFile(path)).digest("hex");
const sourcePaths = [
  "bindings/node/benchmark-f32.mjs", "bindings/node/index.js", "bindings/node/worker.js",
  "bindings/node/addon.cc", "bindings/c/src/typed.rs", "bindings/c/src/lib.rs",
];
const dimensions = 384;
const records = 512;
const queryCount = 40;
const rounds = 3;
const result = {
  generated_at: new Date().toISOString(),
  environment: { node: process.version, platform: process.platform, arch: process.arch, cpu: cpus()[0]?.model },
  provenance: {
    command: [process.execPath, fileURLToPath(import.meta.url), ...(output ? [output] : [])],
    cwd: process.cwd(),
    environment_overrides: { VLITE_TEST_LIBRARY: process.env.VLITE_TEST_LIBRARY ?? null,
      VLITE_TEST_ADDON: process.env.VLITE_TEST_ADDON ?? null },
    git_head: git("rev-parse", "HEAD"),
    git_status_porcelain: git("status", "--porcelain=v1", "--untracked-files=normal"),
    tracked_diff_sha256: createHash("sha256").update(git("diff", "--binary", "HEAD")).digest("hex"),
    native_library: { path: libraryPath, sha256: await hash(libraryPath) },
    addon: { path: addonPath, sha256: await hash(addonPath) },
    source_sha256: Object.fromEntries(await Promise.all(sourcePaths.map(async (path) =>
      [path, await hash(resolve(root, path))]))),
    note: "Working-tree/source hashes are recorded at run time. They do not prove an existing binary was built from that tree; retain the preceding successful build log separately.",
  },
  workload: { dimensions, records, batch_values: dimensions * records, queries: queryCount, k: 10, rounds,
    search_warmup: "5 JSON single queries, 5 typed single queries, 1 typed three-query call",
    order: "Fixed each round: JSON bulk then typed bulk; JSON singles then typed singles then typed multi-query",
    timing: "Wall-clock awaited calls, including main-thread validation/encoding/snapshot, worker scheduling, native work and response decoding. Input fixtures constructed outside timers; each bulk uses a fresh database.",
  },
  caveats: [
    "Single shared host; uncontrolled scheduling, CPU load and caches. Small bounded sample, not a production guarantee.",
    "In-memory databases; no disk sync, storage throughput or durable ingest claim.",
    "Fixed order can favor later variants through JIT/cache warming; no statistical speedup claim.",
    "Typed inputs are copied into private snapshots; this is not zero-copy.",
    "Native C ABI still performs one serialized engine call per query; multi-query calls do not provide a transaction-wide snapshot across other owners.",
    "Only exact squared-L2 search, one dimension/record-count configuration and this platform are measured.",
  ],
  correctness: {},
  batch_put_ms: { json: [], typed_f32: [] },
  search_ms: { json_one_request_per_query: [], typed_f32_one_request_per_query: [], typed_f32_many_one_request: [] },
};
const vectors = Array.from({ length: records }, (_, id) =>
  Float32Array.from({ length: dimensions }, (_, d) => ((id * 17 + d * 13) % 101) / 101));
const payload = new Uint8Array([0, 255]);
const metadata = { tag: { type: "string", value: "paired" } };
const typedOperations = vectors.map((vector, id) => ({ id: String(id), vector, payload, metadata }));
const jsonOperations = typedOperations.map(({ id, vector }) => ({
  op: "put", id, vector: Array.from(vector), payload_base64: "AP8=", metadata,
}));
const queries = Array.from({ length: queryCount }, (_, id) => vectors[(id * 11) % records]);
const jsonQueries = queries.map((vector) => Array.from(vector));
const elapsed = async (action) => {
  const start = performance.now();
  await action();
  return +(performance.now() - start).toFixed(3);
};
const native = new NativeLibrary({ libraryPath, addonPath });
try {
  for (let round = 0; round < rounds; round++) {
    const pair = [];
    try {
      for (const kind of ["json", "typed_f32"]) {
        const db = await native.createInMemory(dimensions, { format: 4 });
        pair.push(db);
        result.batch_put_ms[kind].push(await elapsed(() =>
          kind === "json" ? db.batch(jsonOperations) : db.batchPutF32(typedOperations)));
        assert.equal((await db.info()).records, records);
      }
      for (let start = 0; start < records; start += 100) {
        const ids = Array.from({ length: Math.min(100, records - start) }, (_, i) => String(start + i));
        assert.deepEqual(await pair[0].getMany(ids), await pair[1].getMany(ids));
      }
    } finally { for (const db of pair) await db.close(); }
  }
  result.correctness.bulk = "All records, vectors, payloads, metadata and revisions matched JSON for every round";
  const db = await native.createInMemory(dimensions, { format: 4 });
  await db.batch(jsonOperations);
  for (let i = 0; i < 5; i++) {
    await db.search(jsonQueries[0], 10);
    await db.searchF32(queries[0], 10);
  }
  await db.searchManyF32(queries.slice(0, 3), 10);
  for (let round = 0; round < rounds; round++) {
    result.search_ms.json_one_request_per_query.push(await elapsed(async () => {
      for (const vector of jsonQueries) await db.search(vector, 10);
    }));
    result.search_ms.typed_f32_one_request_per_query.push(await elapsed(async () => {
      for (const vector of queries) await db.searchF32(vector, 10);
    }));
    result.search_ms.typed_f32_many_one_request.push(await elapsed(() => db.searchManyF32(queries, 10)));
  }
  const expected = [];
  for (const query of jsonQueries) expected.push(await db.search(query, 10));
  const typed = [];
  for (const query of queries) typed.push(await db.searchF32(query, 10));
  assert.deepEqual(typed, expected);
  assert.deepEqual(await db.searchManyF32(queries, 10), expected);
  result.correctness.search = "All 40 ordered hit IDs and distances matched JSON for single and multi-query typed calls";
  await assert.rejects(db.batchPutF32([
    { id: "10000", vector: vectors[0] }, { id: "010001", vector: vectors[1] },
  ]), (error) => error instanceof EmbeddedError && error.code === "invalid_input");
  assert.equal((await db.info()).records, records);
  assert.equal((await db.getMany(["10000"]))[0], null);
  result.correctness.atomic_rejection = "Invalid ID rejected with invalid_input; no rows committed";
  await db.close();
} finally { await native.close(); }
assert.equal(await hash(libraryPath), result.provenance.native_library.sha256, "Native binary changed during benchmark");
assert.equal(await hash(addonPath), result.provenance.addon.sha256, "Addon changed during benchmark");
const encoded = JSON.stringify(result, null, 2) + "\n";
if (output) await writeFile(output, encoded, "utf8");
process.stdout.write(encoded);
