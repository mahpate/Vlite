// Node smoke test: node test/smoke.mjs [path/to/vlite_wasm.js]  (wasm-bindgen --target nodejs output)
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const pkg = require(path.resolve(process.argv[2] ?? 'pkg/vlite_wasm.js'));
pkg.setClock(Date.now());

const db = pkg.VLiteDatabase.create(2);
db.put(1, new Float32Array([1, 0]), new TextEncoder().encode('one'));
db.put(2, new Float32Array([0, 1]), new TextEncoder().encode('two'));
const hits = db.search(new Float32Array([0.9, 0.1]), 2);
assert.equal(hits[0], 1);
assert.equal(new TextDecoder().decode(db.get(2)), 'two');
db.delete(1);
assert.equal(db.len(), 1);

const image = db.toBytes();
// Every new database is graph-enabled V5: header version 5, FLAG_GRAPH (4).
assert.equal(image[8], 5);
assert.equal(image[26] & 4, 4);
const reopened = pkg.VLiteDatabase.fromBytes(image);
assert.equal(reopened.len(), 1);
assert.equal(reopened.search(new Float32Array([0, 1]), 1)[0], 2);

// V5 (the default) enables expiry, metadata-only writes, grouping and the durable change feed.
pkg.setClock(100);
const advanced = pkg.VLiteDatabase.create(2);
advanced.putWithExpiry(10, new Float32Array([1, 0]), new Uint8Array(), 500, JSON.stringify({
  category: { type: 'string', value: 'a' }, score: { type: 'integer', value: '7' },
}));
advanced.putWithExpiry(11, new Float32Array([0, 1]), new Uint8Array(), undefined, JSON.stringify({
  category: { type: 'string', value: 'b' },
}));
const cursor = advanced.feedCursor();
advanced.updateMetadata(10, JSON.stringify({ category: { type: 'string', value: 'b' } }));
const grouped = JSON.parse(advanced.groupedSearch(new Float32Array([1, 0]), 2, 2, 'category'));
assert.equal(grouped.length, 1);
assert.equal(grouped[0].value.value, 'b');
const changes = JSON.parse(advanced.changesAfter(cursor, 20));
assert.equal(changes.changes.length, 1);
assert.equal(changes.changes[0].kind, 'metadata_update');
assert.equal(changes.changes[0].id, '10');
assert.equal(changes.has_more, false);
pkg.setClock(500);
assert.equal(advanced.get(10), undefined);

const key = new Uint8Array(32).fill(7);
const secret = pkg.VLiteDatabase.create(2, key);
secret.put(5, new Float32Array([1, 1]), new TextEncoder().encode('classified'));
const sealed = secret.toBytes();
assert.ok(!Buffer.from(sealed).includes('classified'));
assert.throws(() => pkg.VLiteDatabase.fromBytes(sealed));
assert.throws(() => pkg.VLiteDatabase.fromBytes(sealed, new Uint8Array(32).fill(8)));
assert.equal(pkg.VLiteDatabase.fromBytes(sealed, key).len(), 1);
assert.throws(() => pkg.VLiteDatabase.fromBytes(image.slice(0, image.length - 3)));

// Key-value. The shared dialect goldens (tests/fixtures/kv/kv_dialect_goldens.json, replayed by every surface) run
// through the typed methods on a fresh one-dimension database per sequence. This surface has no raw JSON channel,
// no read-only handle and no named collections, so those steps are not replayed here.
const U64_MAX = 2n ** 64n - 1n;
const goldens = JSON.parse(readFileSync(
  fileURLToPath(new URL('../../../tests/fixtures/kv/kv_dialect_goldens.json', import.meta.url)), 'utf8'));
assert.equal(goldens.format, 1, 'the replay reads format 1');
const skipped = (node) => (node.skip ?? []).includes('wasm') || node.typed === false;
const codeOf = (error) => { assert.ok(error instanceof Error, `${error}`); return error.code; };

// `$ref:NAME` becomes the bound version and { $repeat: [text, count], $suffix: text } a long string.
function expand(template, bound) {
  if (typeof template === 'string') return template.startsWith('$ref:') ? bound[template.slice(5)] : template;
  if (Array.isArray(template)) return template.map((item) => expand(item, bound));
  if (template !== null && typeof template === 'object') {
    if ('$repeat' in template) return template.$repeat[0].repeat(template.$repeat[1]) + (template.$suffix ?? '');
    return Object.fromEntries(Object.entries(template).map(([name, item]) => [name, expand(item, bound)]));
  }
  return template;
}

// Exact equality, except that `$bind:NAME[>OTHER]` binds a commit version (a canonical decimal u64 above zero).
function matches(expected, actual, bound) {
  if (typeof expected === 'string' && expected.startsWith('$bind:')) {
    const [name, floor] = expected.slice(6).split('>');
    if (typeof actual !== 'string' || !/^[1-9][0-9]{0,19}$/.test(actual) || BigInt(actual) > U64_MAX) return false;
    if (floor !== undefined && BigInt(actual) <= BigInt(bound[floor])) return false;
    bound[name] = actual;
    return true;
  }
  if (Array.isArray(expected)) {
    return Array.isArray(actual) && expected.length === actual.length
      && expected.every((item, index) => matches(item, actual[index], bound));
  }
  if (expected !== null && typeof expected === 'object') {
    if (actual === null || typeof actual !== 'object' || Array.isArray(actual)) return false;
    const names = Object.keys(expected);
    return names.length === Object.keys(actual).length
      && names.every((name) => name in actual && matches(expected[name], actual[name], bound));
  }
  return expected === actual;
}

function dialectEntry(entry) {
  assert.ok(entry.value instanceof Uint8Array, 'value must be a Uint8Array');
  assert.equal(typeof entry.version, 'bigint');
  assert.ok(entry.expires_at_ms === null || typeof entry.expires_at_ms === 'bigint');
  return { key: entry.key, value_base64: Buffer.from(entry.value).toString('base64'), version: entry.version.toString(),
    expires_at_ms: entry.expires_at_ms === null ? null : entry.expires_at_ms.toString() };
}

const expectedOf = (expected) => (typeof expected === 'object' ? { revision: BigInt(expected.revision) } : expected);

// Runs a golden request through the typed methods; the result comes back in the dialect's JSON shape.
function call(db, request) {
  const { op, collection, namespace, ...rest } = request;
  if (op === 'get') {
    const entry = db.kvGet(collection, namespace, rest.key);
    // An absent or expired key is `undefined` here, like get(id).
    if (entry === undefined) throw Object.assign(new Error('kv_not_found'), { code: 'kv_not_found' });
    return dialectEntry(entry);
  }
  if (op === 'put') {
    const deadline = rest.expires_at_ms === undefined || rest.expires_at_ms === null ? null : BigInt(rest.expires_at_ms);
    const result = db.kvPut(collection, namespace, rest.key, new Uint8Array(Buffer.from(rest.value_base64, 'base64')),
      expectedOf(rest.expected), deadline);
    assert.equal(typeof result.version, 'bigint');
    return { version: result.version.toString(), expires_at_ms: result.expires_at_ms === null ? null : result.expires_at_ms.toString() };
  }
  if (op === 'delete') {
    const deleted = db.kvDelete(collection, namespace, rest.key, expectedOf(rest.expected));
    assert.equal(typeof deleted, 'boolean');
    return { deleted };
  }
  const page = db.kvList(collection, namespace, rest.prefix, rest.after, rest.limit);
  return { items: page.items.map(dialectEntry), next_after: page.next_after };
}

pkg.setClock(Date.now());
let replayed = 0;
for (const sequence of goldens.sequences) {
  if (skipped(sequence)) continue;
  const kv = pkg.VLiteDatabase.create(1);
  const bound = {};
  for (const step of sequence.steps) {
    if (skipped(step)) continue;
    const request = expand(step.request, bound);
    const context = `${sequence.name}/${step.name}`;
    replayed++;
    let result;
    try { result = call(kv, request); } catch (error) {
      assert.equal(codeOf(error), step.error, `${context}: ${error.message.slice(0, 300)}`);
      continue;
    }
    assert.ok(step.result !== undefined, `${context}: succeeded unexpectedly with ${JSON.stringify(result).slice(0, 300)}`);
    assert.ok(matches(expand(step.result, bound), result, bound), `${context}: ${JSON.stringify(result).slice(0, 300)}`);
  }
}
assert.ok(replayed > 100, `only ${replayed} golden steps replayed`);

// Values are Uint8Array, versions and deadlines bigint; results are plain objects.
const kvOne = pkg.VLiteDatabase.create(1);
const written = kvOne.kvPut('default', 'n', 'a', new Uint8Array([7, 8]), 'absent', 2n ** 53n + 1n);
assert.equal(Object.getPrototypeOf(written), Object.prototype);
assert.equal(written.expires_at_ms, 2n ** 53n + 1n);
const entry = kvOne.kvGet('default', 'n', 'a');
assert.deepEqual(entry, { key: 'a', value: new Uint8Array([7, 8]), version: written.version, expires_at_ms: 2n ** 53n + 1n });
const next = kvOne.kvPut('default', 'n', 'a', new Uint8Array(), { revision: written.version });
assert.ok(next.version > written.version);
assert.equal(next.expires_at_ms, null);
assert.throws(() => kvOne.kvDelete('default', 'n', 'a', { revision: written.version }), (error) => codeOf(error) === 'precondition_failed');
assert.equal(kvOne.kvDelete('default', 'n', 'a', { revision: next.version }), true);
assert.equal(kvOne.kvDelete('default', 'n', 'a'), false);
assert.equal(kvOne.kvGet('default', 'n', 'a'), undefined);

// A byte image keeps entries, versions and deadlines.
const kept = pkg.VLiteDatabase.create(1);
const keptPut = kept.kvPut('default', 'n', 'a', new Uint8Array([7, 8]), 'absent', U64_MAX);
assert.deepEqual(pkg.VLiteDatabase.fromBytes(kept.toBytes()).kvGet('default', 'n', 'a'),
  { key: 'a', value: new Uint8Array([7, 8]), version: keptPut.version, expires_at_ms: U64_MAX });

// Expiry follows setClock: an entry whose deadline is the clock is due.
pkg.setClock(1_000_000);
const timed = pkg.VLiteDatabase.create(1);
timed.kvPut('default', 'n', 'k', new Uint8Array([1]), undefined, 1_001_000n);
pkg.setClock(1_000_999);
assert.equal(timed.kvGet('default', 'n', 'k').expires_at_ms, 1_001_000n);
pkg.setClock(1_001_000);
assert.equal(timed.kvGet('default', 'n', 'k'), undefined);
assert.deepEqual(timed.kvList('default', 'n').items, []);
pkg.setClock(Date.now());

// Arguments that cannot be honoured throw: a Number deadline or revision is never coerced to a bigint.
const strict = pkg.VLiteDatabase.create(1);
const value = new Uint8Array([1]);
for (const action of [
  () => strict.kvPut('default', 'n', 'k', value, undefined, 5),
  () => strict.kvPut('default', 'n', 'k', value, undefined, '5'),
  () => strict.kvPut('default', 'n', 'k', value, undefined, -1n),
  () => strict.kvPut('default', 'n', 'k', value, undefined, 2n ** 64n),
  () => strict.kvPut('default', 'n', 'k', value, 'present'),
  () => strict.kvPut('default', 'n', 'k', value, null),
  () => strict.kvPut('default', 'n', 'k', value, { revision: 1 }),
  () => strict.kvPut('default', 'n', 'k', value, { revision: -1n }),
  () => strict.kvPut('default', 'n', 'k', value, { revision: 1n, extra: 1 }),
  () => strict.kvPut('default', 'n', 'k', value, {}),
  () => strict.kvDelete('default', 'n', 'k', { revision: 2n ** 64n }),
  () => strict.kvList('default', 'n', undefined, undefined, -1),
  () => strict.kvList('default', 'n', undefined, undefined, 1.5),
  () => strict.kvList('default', 'n', undefined, undefined, NaN),
  () => strict.kvList('default', 'n', undefined, undefined, 2 ** 60),
]) assert.throws(action, (error) => codeOf(error) === 'invalid_request');
assert.throws(() => strict.kvList('default', 'n', undefined, undefined, 0), (error) => codeOf(error) === 'invalid_input');
assert.throws(() => strict.kvPut('default', 'n', 'k', new Uint8Array(1047291)), (error) => codeOf(error) === 'invalid_input');
// Nothing was written by the rejected calls.
assert.equal(strict.kvGet('default', 'n', 'k'), undefined);

// A value is a Uint8Array and nothing else: a typed parameter would copy a string as zero bytes, an ArrayBuffer or a
// DataView as nothing and the numbers of an array modulo 256, and report success.
const typed = pkg.VLiteDatabase.create(1);
for (const bad of ['text', new ArrayBuffer(4), new DataView(new ArrayBuffer(2)), [1, 300], undefined, null, 7, {}]) {
  assert.throws(() => typed.kvPut('default', 'n', 'k', bad), (error) => codeOf(error) === 'invalid_request', String(bad));
}
assert.equal(typed.kvGet('default', 'n', 'k'), undefined);
// A Node Buffer is a Uint8Array.
typed.kvPut('default', 'n', 'k', Buffer.from([1, 2]));
assert.deepEqual(typed.kvGet('default', 'n', 'k').value, new Uint8Array([1, 2]));

// Every text argument is a string, and a well-formed one: an unpaired surrogate would be copied as U+FFFD, and two
// different keys would name one entry. The other bindings refuse it too.
for (const bad of [1, null, undefined, {}, ['a'], new String('a')]) {
  for (const action of [
    () => typed.kvGet(bad, 'n', 'k'), () => typed.kvGet('default', bad, 'k'), () => typed.kvGet('default', 'n', bad),
    () => typed.kvPut('default', 'n', bad, new Uint8Array()), () => typed.kvDelete('default', 'n', bad),
    () => typed.kvList(bad, 'n'), () => typed.kvList('default', bad),
  ]) assert.throws(action, (error) => codeOf(error) === 'invalid_request', String(bad));
}
assert.deepEqual(typed.kvList('default', 'n', null, null, null).items.map((item) => item.key), ['k']);
for (const bad of ['\ud800', 'a\udc00', '\udc00\ud800', 'x\ud83d']) {
  for (const action of [
    () => typed.kvGet('default', 'n', bad), () => typed.kvGet('default', bad, 'k'), () => typed.kvGet(bad, 'n', 'k'),
    () => typed.kvPut('default', 'n', bad, new Uint8Array()), () => typed.kvDelete('default', 'n', bad),
    () => typed.kvList('default', 'n', bad), () => typed.kvList('default', 'n', undefined, bad),
  ]) assert.throws(action, (error) => codeOf(error) === 'invalid_input', JSON.stringify(bad));
}
// A valid surrogate pair is an ordinary character.
const smile = '\u{1F600}/é';
typed.kvPut('default', smile, smile, new Uint8Array([3]));
assert.equal(typed.kvGet('default', smile, smile).key, smile);
assert.deepEqual(typed.kvList('default', smile, '\u{1F600}').items.map((item) => item.key), [smile]);

// An `expected` that runs the caller's code and throws is a malformed value, and leaves the database usable: an
// exception that unwound through the native frame would leave it borrowed for good.
const hostile = pkg.VLiteDatabase.create(1);
const getter = { get revision() { throw new Error('the getter ran'); } };
const trap = new Proxy({}, { ownKeys() { throw new Error('the trap ran'); } });
for (const expected of [getter, trap]) {
  assert.throws(() => hostile.kvPut('default', 'n', 'k', new Uint8Array(), expected), (error) => codeOf(error) === 'invalid_request');
  assert.throws(() => hostile.kvDelete('default', 'n', 'k', expected), (error) => codeOf(error) === 'invalid_request');
}
hostile.kvPut('default', 'n', 'k', new Uint8Array([9]));
assert.deepEqual(hostile.kvGet('default', 'n', 'k').value, new Uint8Array([9]));
assert.ok(hostile.toBytes().length > 0);
// A collection with other than one dimension is not a key-value collection.
const wide = pkg.VLiteDatabase.create(2);
assert.throws(() => wide.kvGet('default', 'n', 'k'), (error) => codeOf(error) === 'not_kv_collection');
assert.throws(() => wide.kvPut('default', 'n', 'k', value), (error) => codeOf(error) === 'not_kv_collection');

// A list page holds at most 16 MiB of JSON: 12 of 13 largest values.
const paged = pkg.VLiteDatabase.create(1);
const largest = new Uint8Array(1047290);
for (let index = 0; index < 13; index++) paged.kvPut('default', 'n', `k${String(index).padStart(2, '0')}`, largest);
const first = paged.kvList('default', 'n', undefined, undefined, 10000);
assert.equal(first.items.length, 12);
assert.equal(first.next_after, 'k11');
const second = paged.kvList('default', 'n', undefined, first.next_after, 10000);
assert.deepEqual(second.items.map((item) => item.key), ['k12']);
assert.equal(second.next_after, null);
assert.equal(second.items[0].value.length, 1047290);
console.log('wasm smoke test passed');
