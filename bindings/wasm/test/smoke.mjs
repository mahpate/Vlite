// Node smoke test: node test/smoke.mjs [path/to/vlite_wasm.js]  (wasm-bindgen --target nodejs output)
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import path from 'node:path';

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
console.log('wasm smoke test passed');
