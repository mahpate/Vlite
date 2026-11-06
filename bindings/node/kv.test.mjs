// Key-value API (kvGet, kvPut, kvDelete, kvList). tests/fixtures/kv/kv_dialect_goldens.json, the file the C ABI, Python,
// WASM and the MCP/shell tools replay too, runs against a fresh database per sequence twice: through the raw JSON
// dialect of the native `kv` op (every step, via the library's internal request channel) and through the typed wrappers
// (the steps they can express), so the wrappers cannot drift from the other surfaces.
import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdir, rm, readFile } from "node:fs/promises";
import { randomUUID } from "node:crypto";
import { fileURLToPath } from "node:url";
import { join } from "node:path";
import { NativeLibrary, Database, EmbeddedError } from "./index.js";

const nativeOptions = { libraryPath: process.env.VLITE_TEST_LIBRARY };
const GOLDENS = JSON.parse(await readFile(
  fileURLToPath(new URL("../../tests/fixtures/kv/kv_dialect_goldens.json", import.meta.url)), "utf8"));
const U64_MAX = 2n ** 64n - 1n;
const code = expected => error => error instanceof EmbeddedError && error.code === expected;

async function fixture(fn) {
  const directory = fileURLToPath(new URL(`./test-data-${randomUUID()}/`, import.meta.url));
  await mkdir(directory);
  const native = new NativeLibrary(nativeOptions);
  try { await fn(native, directory); }
  finally { await native.close(); await rm(directory, { recursive: true }); }
}

// `$ref:NAME` becomes the bound version and { $repeat: [text, count], $suffix: text } a long string.
function expand(template, bound) {
  if (typeof template === "string") return template.startsWith("$ref:") ? bound[template.slice(5)] : template;
  if (Array.isArray(template)) return template.map(item => expand(item, bound));
  if (template !== null && typeof template === "object") {
    if ("$repeat" in template) return template.$repeat[0].repeat(template.$repeat[1]) + (template.$suffix ?? "");
    return Object.fromEntries(Object.entries(template).map(([name, item]) => [name, expand(item, bound)]));
  }
  return template;
}

// Exact equality, except that `$bind:NAME[>OTHER]` binds a commit version (a canonical decimal u64 above zero).
function matches(expected, actual, bound) {
  if (typeof expected === "string" && expected.startsWith("$bind:")) {
    const [name, floor] = expected.slice(6).split(">");
    if (typeof actual !== "string" || !/^[1-9][0-9]{0,19}$/.test(actual) || BigInt(actual) > U64_MAX) return false;
    if (floor !== undefined && BigInt(actual) <= BigInt(bound[floor])) return false;
    bound[name] = actual;
    return true;
  }
  if (Array.isArray(expected)) {
    return Array.isArray(actual) && expected.length === actual.length && expected.every((item, index) => matches(item, actual[index], bound));
  }
  if (expected !== null && typeof expected === "object") {
    if (actual === null || typeof actual !== "object" || Array.isArray(actual)) return false;
    const names = Object.keys(expected);
    return names.length === Object.keys(actual).length && names.every(name => name in actual && matches(expected[name], actual[name], bound));
  }
  return expected === actual;
}

const skipped = node => (node.skip ?? []).includes("node");
const rawCall = (native, handle, _db, request) => native._request(handle, { op: "kv", request });
const optionalText = value => value === null ? null : value.toString();

function dialectEntry(entry) {
  assert.ok(entry.value instanceof Uint8Array && !Buffer.isBuffer(entry.value), "value must be a plain Uint8Array");
  assert.equal(typeof entry.version, "bigint");
  assert.ok(entry.expires_at_ms === null || typeof entry.expires_at_ms === "bigint");
  return { key: entry.key, value_base64: Buffer.from(entry.value).toString("base64"), version: entry.version.toString(),
    expires_at_ms: optionalText(entry.expires_at_ms) };
}

const typedExpected = expected => typeof expected === "object" ? { revision: BigInt(expected.revision) } : expected;

// Runs a golden request through the typed wrappers; the result comes back in the dialect's JSON shape.
async function typedCall(_native, _handle, db, request) {
  const { op, collection, namespace, ...rest } = request;
  const address = { collection, namespace };
  if (op === "get") return dialectEntry(await db.kvGet({ ...address, key: rest.key }));
  if (op === "put") {
    const result = await db.kvPut({ ...address, key: rest.key, value: new Uint8Array(Buffer.from(rest.value_base64, "base64")),
      expected: typedExpected(rest.expected ?? "any"),
      expiresAtMs: rest.expires_at_ms === undefined || rest.expires_at_ms === null ? null : BigInt(rest.expires_at_ms) });
    assert.equal(typeof result.version, "bigint");
    return { version: result.version.toString(), expires_at_ms: optionalText(result.expires_at_ms) };
  }
  if (op === "delete") {
    const deleted = await db.kvDelete({ ...address, key: rest.key, expected: typedExpected(rest.expected ?? "any") });
    assert.equal(typeof deleted, "boolean");
    return { deleted };
  }
  const page = await db.kvList({ ...address, prefix: rest.prefix ?? "", after: rest.after, limit: rest.limit ?? 100 });
  return { items: page.items.map(dialectEntry), next_after: page.next_after };
}

// A raw handle for a sequence: a fresh in-memory database, or a read-only reopen of a seeded file.
async function open(native, directory, sequence) {
  if (sequence.mode === "read_only") {
    const path = join(directory, `${sequence.name}.vlite`);
    const writer = (await native._request("0", { op: "create", path, dimensions: 1 })).handle;
    for (const seed of sequence.seed) await native._request(writer, { op: "kv", request: expand(seed, {}) });
    await native._request(writer, { op: "close" });
    return (await native._request("0", { op: "open", path, read_only: true })).handle;
  }
  const { handle } = await native._request("0", { op: "features", request: { op: "create", path: null, dimensions: 1 } });
  for (const collection of sequence.collections ?? []) {
    await native._request(handle, { op: "create_collection", name: collection.name, dimensions: collection.dimensions });
  }
  return handle;
}

async function replay(native, directory, call, typed) {
  let replayed = 0;
  for (const sequence of GOLDENS.sequences) {
    if (skipped(sequence) || (typed && sequence.typed === false)) continue;
    const handle = await open(native, directory, sequence);
    const db = new Database(native, handle);
    const bound = {};
    try {
      for (const step of sequence.steps) {
        if (skipped(step) || (typed && step.typed === false)) continue;
        const request = expand(step.request, bound);
        const context = `${sequence.name}/${step.name}`;
        replayed++;
        let result;
        try { result = await call(native, handle, db, request); }
        catch (error) {
          assert.ok(error instanceof EmbeddedError, `${context}: ${error?.stack ?? error}`);
          assert.equal(error.code, step.error, `${context}: ${error.message.slice(0, 300)}`);
          continue;
        }
        assert.ok(step.result !== undefined, `${context}: succeeded unexpectedly with ${JSON.stringify(result).slice(0, 300)}`);
        const expected = expand(step.result, bound);
        assert.ok(matches(expected, result, bound), `${context}: ${JSON.stringify(result).slice(0, 300)}`);
      }
    } finally {
      await db.close();
    }
  }
  return replayed;
}

test("the raw dialect replays every golden step", async () => fixture(async (native, directory) => {
  const expected = GOLDENS.sequences.filter(sequence => !skipped(sequence))
    .reduce((count, sequence) => count + sequence.steps.filter(step => !skipped(step)).length, 0);
  assert.equal(await replay(native, directory, rawCall, false), expected);
}));

test("the typed wrappers replay every expressible golden step", async () => fixture(async (native, directory) => {
  assert.ok(await replay(native, directory, typedCall, true) > 100);
}));

test("values are Uint8Array and versions and deadlines are bigint", async () => fixture(async native => {
  const db = await native.createInMemory(1);
  const address = { collection: "default", namespace: "n" };
  const put = await db.kvPut({ ...address, key: "a", value: Buffer.from([0, 255]), expiresAtMs: 2n ** 53n + 1n });
  assert.equal(put.expires_at_ms, 2n ** 53n + 1n);
  assert.equal(typeof put.version, "bigint");
  const entry = await db.kvGet({ ...address, key: "a" });
  assert.deepEqual(entry, { key: "a", value: new Uint8Array([0, 255]), version: put.version, expires_at_ms: 2n ** 53n + 1n });
  assert.equal(Object.getPrototypeOf(entry.value), Uint8Array.prototype);
  assert.equal((await db.kvPut({ ...address, key: "b", value: new Uint8Array(), expiresAtMs: U64_MAX })).expires_at_ms, U64_MAX);
  assert.equal((await db.kvPut({ ...address, key: "c", value: new Uint8Array(), expiresAtMs: 0n })).expires_at_ms, 0n);
  await assert.rejects(db.kvGet({ ...address, key: "c" }), code("kv_not_found"));
  const page = await db.kvList(address);
  assert.deepEqual(page.items.map(item => item.key), ["a", "b"]);
  assert.equal(page.next_after, null);
  assert.equal((await db.kvPut({ ...address, key: "a", value: new Uint8Array(), expiresAtMs: null })).expires_at_ms, null);
  assert.deepEqual((await db.kvGet({ ...address, key: "a" })).value, new Uint8Array());
  await db.close();
}));

test("compare-and-set takes bigint revisions", async () => fixture(async native => {
  const db = await native.createInMemory(1);
  const address = { collection: "default", namespace: "n", key: "a" };
  const first = (await db.kvPut({ ...address, value: new Uint8Array([1]), expected: "absent" })).version;
  await assert.rejects(db.kvPut({ ...address, value: new Uint8Array([2]), expected: "absent" }), code("precondition_failed"));
  const second = (await db.kvPut({ ...address, value: new Uint8Array([2]), expected: { revision: first } })).version;
  assert.ok(second > first);
  await assert.rejects(db.kvDelete({ ...address, expected: { revision: first } }), code("precondition_failed"));
  assert.equal(await db.kvDelete({ ...address, expected: { revision: second } }), true);
  assert.equal(await db.kvDelete(address), false);
  assert.equal(await db.kvDelete({ ...address, expected: "absent" }), false);
  await db.close();
}));

test("paging follows next_after", async () => fixture(async native => {
  const db = await native.createInMemory(1);
  const address = { collection: "default", namespace: "n" };
  for (let index = 0; index < 25; index++) {
    await db.kvPut({ ...address, key: `k${String(index).padStart(2, "0")}`, value: new Uint8Array([index]) });
  }
  const seen = [];
  let after;
  for (;;) {
    const page = await db.kvList({ ...address, prefix: "k", after, limit: 10 });
    assert.ok(page.items.length <= 10);
    seen.push(...page.items.map(item => [item.key, item.value[0]]));
    after = page.next_after;
    if (after === null) break;
  }
  assert.deepEqual(seen, Array.from({ length: 25 }, (_, index) => [`k${String(index).padStart(2, "0")}`, index]));
  await db.close();
}));

test("arguments are checked before the native call", async () => fixture(async native => {
  const db = await native.createInMemory(1);
  const address = { collection: "default", namespace: "n", key: "a" };
  const value = new Uint8Array();
  const rejected = [
    [TypeError, () => db.kvPut({ ...address, value: "text" })],
    [TypeError, () => db.kvPut({ ...address, value: [1, 2] })],
    [TypeError, () => db.kvPut({ ...address, value: undefined })],
    [TypeError, () => db.kvGet({ ...address, collection: undefined })],
    [TypeError, () => db.kvGet({ ...address, key: new Uint8Array() })],
    [TypeError, () => db.kvPut({ ...address, value, expiresAtMs: 1 })],
    [TypeError, () => db.kvPut({ ...address, value, expiresAtMs: "1" })],
    [RangeError, () => db.kvPut({ ...address, value, expiresAtMs: -1n })],
    [RangeError, () => db.kvPut({ ...address, value, expiresAtMs: 2n ** 64n })],
    [TypeError, () => db.kvPut({ ...address, value, expected: "present" })],
    [TypeError, () => db.kvPut({ ...address, value, expected: { revision: 1 } })],
    [TypeError, () => db.kvPut({ ...address, value, expected: { revision: 1n, extra: 1 } })],
    [TypeError, () => db.kvPut({ ...address, value, expected: null })],
    [RangeError, () => db.kvDelete({ ...address, expected: { revision: -1n } })],
    [TypeError, () => db.kvList({ ...address, limit: 1n })],
    [TypeError, () => db.kvList({ ...address, limit: 1.5 })],
    [RangeError, () => db.kvList({ ...address, limit: -1 })],
    [TypeError, () => db.kvList({ ...address, prefix: null })],
    [TypeError, () => db.kvList({ ...address, after: 1 })],
  ];
  for (const [type, action] of rejected) await assert.rejects(Promise.resolve().then(action), type);
  await assert.rejects(db.kvGet(address), code("kv_not_found"));
  await db.close();
}));

test("bounds and missing collections come from the native layer", async () => fixture(async native => {
  const db = await native.createInMemory(1);
  const address = { collection: "default", namespace: "n" };
  await assert.rejects(db.kvGet({ collection: "default", namespace: "", key: "a" }), code("invalid_input"));
  await assert.rejects(db.kvList({ ...address, limit: 0 }), code("invalid_input"));
  await assert.rejects(db.kvPut({ ...address, key: "a", value: new Uint8Array(1047291) }), code("invalid_input"));
  assert.equal((await db.kvPut({ ...address, key: "a", value: new Uint8Array(1047290) })).expires_at_ms, null);
  assert.equal((await db.kvGet({ ...address, key: "a" })).value.length, 1047290);
  await assert.rejects(db.kvPut({ collection: "missing", namespace: "n", key: "a", value: new Uint8Array() }), code("collection_not_found"));
  await assert.rejects(db.kvList({ collection: "missing", namespace: "n" }), code("collection_not_found"));
  assert.deepEqual((await db.listCollections()).map(collection => collection.name), ["default"]);
  await db.createCollection("wide", 2);
  await assert.rejects(db.kvGet({ collection: "wide", namespace: "n", key: "a" }), code("not_kv_collection"));
  await db.close();
}));

test("a closed database rejects", async () => fixture(async native => {
  const db = await native.createInMemory(1);
  await db.close();
  await assert.rejects(db.kvGet({ collection: "default", namespace: "n", key: "a" }), code("closed"));
}));

test("values survive reopen and a read-only handle refuses writes", async () => fixture(async (native, directory) => {
  const path = join(directory, "kv.vlite");
  const db = await native.create(path, 1);
  const address = { collection: "default", namespace: "n", key: "a" };
  const { version } = await db.kvPut({ ...address, value: new Uint8Array([7]), expiresAtMs: U64_MAX });
  await db.close();
  const reader = await native.open(path, { readOnly: true });
  assert.deepEqual(await reader.kvGet(address), { key: "a", value: new Uint8Array([7]), version, expires_at_ms: U64_MAX });
  await assert.rejects(reader.kvPut({ ...address, value: new Uint8Array(), expected: "absent" }), code("read_only"));
  await assert.rejects(reader.kvDelete(address), code("read_only"));
  assert.equal((await reader.kvList({ collection: "default", namespace: "n" })).items[0].version, version);
  await reader.close();
}));

test("a format 4 file stores values but expiry needs an upgrade", async () => fixture(async (native, directory) => {
  const db = await native.create(join(directory, "v4.vlite"), 1, { format: 4 });
  const address = { collection: "default", namespace: "n", key: "a" };
  assert.equal((await db.kvPut({ ...address, value: new Uint8Array([1]) })).expires_at_ms, null);
  await assert.rejects(db.kvPut({ ...address, value: new Uint8Array([1]), expiresAtMs: 1n }), code("upgrade_required"));
  assert.deepEqual((await db.kvGet(address)).value, new Uint8Array([1]));
  await db.close();
}));

function failingLibrary(error) {
  return { _request: () => Promise.reject(error) };
}

test("libraries predating the kv op, or one operation of it, report unsupported", async () => {
  const old = new EmbeddedError("invalid_input", "unknown variant `kv`, expected one of `features`, `graph`, `create`");
  const db = new Database(failingLibrary(old), "1");
  const address = { collection: "default", namespace: "n", key: "a" };
  const calls = [() => db.kvGet(address), () => db.kvPut({ ...address, value: new Uint8Array() }),
    () => db.kvDelete(address), () => db.kvList(address)];
  for (const call of calls) await assert.rejects(call(), code("unsupported"));
  // Another error that merely mentions the operation passes through.
  const other = new EmbeddedError("invalid_request", "unknown variant `batch`, expected `any` or `absent`");
  await assert.rejects(new Database(failingLibrary(other), "1").kvGet(address), error => error === other);
  const field = new EmbeddedError("invalid_input", "unknown field `x`");
  await assert.rejects(new Database(failingLibrary(field), "1").kvGet(address), error => error === field);
});

test("the real library words unknown operations as the wrappers detect them", async () => fixture(async native => {
  await assert.rejects(native._request("0", { op: "zzz" }),
    error => error.code === "invalid_input" && error.message.includes("unknown variant `zzz`"));
  const { handle } = await native._request("0", { op: "features", request: { op: "create", path: null, dimensions: 1 } });
  await assert.rejects(native._request(handle, { op: "kv", request: { op: "zzz" } }),
    error => error.code === "invalid_request" && error.message.includes("unknown variant `zzz`"));
  await native._request(handle, { op: "close" });
}));
