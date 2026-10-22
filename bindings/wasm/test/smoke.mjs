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
const reopened = pkg.VLiteDatabase.fromBytes(image);
assert.equal(reopened.len(), 1);
assert.equal(reopened.search(new Float32Array([0, 1]), 1)[0], 2);

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
