# VLite for WebAssembly

An in-memory VLite database for `wasm32-unknown-unknown`. The host persists the
database by saving `toBytes()` and reloading it with `fromBytes()`; there are no
files, OS locks or threads. Encryption at rest (32-byte raw key) is supported.

```text
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.129 --locked
cd bindings/wasm
cargo build --release --target wasm32-unknown-unknown
wasm-bindgen target/wasm32-unknown-unknown/release/vlite_wasm.wasm --target nodejs --out-dir pkg
node test/smoke.mjs pkg/vlite_wasm.js
```

Use `--target web` or `bundler` for browsers. `create` makes a graph-enabled V5
image like every new VLite file, so V5 mutations and the change feed always work. Metadata
arguments are JSON strings in the engine typed-metadata format. `feedCursor()`
and `changesAfter()` return opaque cursor tokens and JSON strings respectively.
Call `setClock(Date.now())` before operations that depend on time (TTL, commit
timestamps): WebAssembly has no system clock.

API: `VLiteDatabase.create(dimensions, key?)`, `VLiteDatabase.fromBytes(bytes, key?)`,
`put(id, Float32Array, Uint8Array)`, `putWithExpiry(id, vector, payload, expiresAtMs?, metadataJson?)`,
`updateMetadata(id, metadataJson)`, `groupedSearch(query, groups, hitsPerGroup, field)`,
`feedCursor()`, `changesAfter(cursorToken, maxEvents)`, `get(id)`, `delete(id)`, `search(query, k)`
(flat `[id, distance, ...]`), `len()`, `isEmpty()`, `toBytes()`. IDs are JS safe
integers. Limits: file/path APIs, background HNSW rebuilds (no threads), sidecars
and GPU search are unavailable; exact search and synchronous `build_hnsw` work.

Key-value: `kvGet(collection, namespace, key)`, `kvPut(collection, namespace, key, value, expected?, expiresAtMs?)`,
`kvDelete(collection, namespace, key, expected?)` and `kvList(collection, namespace, prefix?, after?, limit?)` store
byte values under namespaced text keys in a collection that already exists and has one dimension: `create(1)` has the
`default` collection, an image from `fromBytes` may hold others, and a missing collection is never created. Values are
`Uint8Array` (a string, an `ArrayBuffer`, a `DataView`, another typed array, an array, a detached array, or a proxy or other
object that only resembles one is refused with `invalid_request`, never copied; a `Buffer` or a subclass is a `Uint8Array`, and
what is stored is what the array holds whatever `length`, `slice` or species it claims or defines: the binding reads it through
the intrinsics of `%TypedArray%.prototype`, so no code of the value's own runs);
collections, namespaces, keys, prefixes and `after` are well-formed strings (an argument of the wrong type throws an `Error`
whose `code` is `invalid_request`, where the Node and Python wrappers throw `TypeError` or `ValueError` first, and one with an
unpaired surrogate is `invalid_input`, as in the other bindings). `limit` is a `number` that is a safe integer, never a coerced
value, and a collection name over 63 bytes is `invalid_input`. Versions and deadlines are
`bigint` (a `Number` is rejected), `expected` is `"any"` (the default),
`"absent"` or `{ revision: bigint }`, and `expiresAtMs` is an absolute Unix-millisecond deadline judged on the
`setClock` clock: a due entry reads as absent, and a put without a deadline clears it. `kvGet` returns
`{ key, value, version, expires_at_ms }`, or `undefined` for an absent or due key; `kvPut` returns
`{ version, expires_at_ms }`; `kvDelete` returns a boolean; `kvList` returns `{ items, next_after }`, one page of at most
`limit` (1 to 10,000, default 100) entries and 16 MiB of JSON, so follow `next_after` (pass it as `after`) until it is
`null`. Namespaces are 1 to 255 and keys 1 to 1,024 UTF-8 bytes and a value is 0 to 1,047,290 bytes. A failure throws an
`Error` whose `code` property is one of the key-value codes of the other bindings (`precondition_failed`,
`invalid_input`, `invalid_request`, `not_kv_collection`, `kv_slots_exhausted`, `upgrade_required`, ...). There are no
lease operations. `test/smoke.mjs` replays the goldens in `tests/fixtures/kv/kv_dialect_goldens.json`, which the other
bindings and the shell and MCP tools replay too.
