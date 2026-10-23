# VLite embedded Node.js / TypeScript

Server-free VLite for Node.js 22.9+, using an N-API addon and the versioned VLite
C ABI. Native operations run in a dedicated worker thread rather than blocking
the JavaScript event loop. The ESM package includes TypeScript declarations.
This is distinct from the REST client at `clients/javascript`.

## Build and install

Build on the target OS/architecture with Rust 1.89+, Python, a C++ compiler,
Node.js 22.9+ and npm. Windows needs Visual Studio C++ build tools; Linux needs
make and a C++ toolchain; macOS needs Xcode command-line tools.

```text
npm --prefix bindings/node ci --ignore-scripts
npm --prefix bindings/node run build
npm --prefix bindings/node test
npm --prefix bindings/node run typecheck
```

Run `node bindings/node/pack.mjs <new-output-directory>` from the root to produce
a checksummed tarball containing only the host-native addon and Rust library,
with OS/architecture restrictions in its package metadata.
Verify it using `node tests/node_package.mjs <tarball-path>`.
Install that tarball in your application:
`npm install <path-to-tarball>`. Runtime users need Node and its normal OS
runtime, not Rust, a compiler, Docker, an HTTP service or credentials.
CI builds and verifies a tarball for each native runner. No npm publication,
automatic binary download or universal native package is implied.
Packages must match the OS/architecture and native runtime where they run.
Linux builds are host-specific, not manylinux-certified Node addons.
Windows x64 and Linux x64/Debian 12 packages have been built and installed
locally. CI also builds on the current macOS runner; other architectures and
libc/runtime combinations require their own builds and verification.
For the clean Linux build/install check:

```text
docker build --file bindings/node/Dockerfile --target artifacts --output type=local,dest=node-linux-packages .
```

The final verification stage installs and runs the package without network
access, Rust or a compiler. Release workflows include native tarballs and
checksums; adding that workflow does not itself publish a new release.

```javascript
import { NativeLibrary, EmbeddedError } from "@vlite/embedded";

const native = new NativeLibrary();
try {
  const db = await native.create("documents.vlite", 3);
  try {
    await db.put("18446744073709551615", [1, 0, 0], {
      payload: new Uint8Array([0, 255]),
      metadata: { category: { type: "string", value: "example" } },
    });
    console.log(await db.search([1, 0, 0], 5));
    console.log(await db.getMany(["0", "18446744073709551615"]));
    await db.buildIndex();
    console.log(await db.searchApproximateReady([1, 0, 0], 5));
  } finally {
    await db.close();
  }
} catch (error) {
  if (error instanceof EmbeddedError) console.error(error.code);
  throw error;
} finally {
  await native.close();
}
```

Use `native.open(path)` for an existing file; create never overwrites, and open
never silently creates or repairs. Records return padded `payload_base64`;
`put` accepts `Uint8Array`/Buffer for binary input. Batch operations use the
existing JSON `payload_base64` shape, not Buffers. Full-width IDs and i64
metadata remain strings. `getMany` retains order/duplicates and returns null
for missing IDs, with 100-ID/8-MiB bounds. Put replaces all record components.
Exact search accepts typed filters; ANN is ready-only with explicit builds.
Exact search also accepts `{ metric: "cosine" }` or
`{ metric: "inner_product" }`, defaulting to `"squared_l2"`.
Cosine returns 1 minus similarity, rejects zero queries and excludes zero-norm
records. Inner product returns negative dot product. Lower is always better.
ANN supports L2 (default) and cosine with an explicit matching build:
`await db.buildIndex({ metric: "cosine" })` then
`await db.searchApproximateReady(query, k, { metric: "cosine" })`.
One graph is retained; a different metric returns `index_not_ready`.
Inner-product ANN is rejected. Cosine ANN requires a v0.1.2+ wrapper
and native library; v0.1.1 has L2 ANN only.
See [ABI contracts and limitations](../README.md).

## Read-only multi-process access

Use `await native.open(path, { readOnly: true })` for an existing database with a
shared OS lock. Multiple readers may coexist across handles and processes.
`get` and `search` work normally; mutations reject with `EmbeddedError.code ===
"read_only"` without invalidating the handle. Add `key` or `passphrase` to the
options for encrypted files.

The default `native.open(path)` is an exclusive writer: it rejects with `"locked"`
while any reader exists, and readers likewise reject while a writer holds the
file. Close every reader before opening a writer. A reader uses a snapshot at
open; close and reopen to see new commits rather than expecting live refresh.
Native C ABI calls are serialized within one process, including across workers.
For parallel reads use separate processes, each opening its own read-only handle.

## Format 4: collections, revisions, planned search and imports

`native.create(path, dims)` still creates format 3. Pass `{ format: 4 }` for
named collections, collection settings and resumable imports; on format 3 those
calls reject with `requires_format_v4`.

```js
const db = await native.create("docs.vlite", 2, { format: 4 });
await db.createCollection("docs", 2, { textFields: ["title"], metadataIndexes: [{ field: "tag", kind: "eq" }] });
const revision = await db.put("1", [1, 0], { collection: "docs", expectedRevision: "absent" });
await db.put("1", [0, 1], { collection: "docs", expectedRevision: revision }); // precondition_failed if changed
const { hits, plan } = await db.searchAuto([1, 0], 5, { collection: "docs", explain: true });
const hybrid = await db.searchHybrid("apple", 5, { collection: "docs", vector: [1, 0] });
const job = await db.nextImportJobId();
await db.importChunk(job, "1", [{ op: "put", id: "2", vector: [0, 1] }], { collection: "docs" });
await db.importFinish(job, { collection: "docs" });
```

- Most methods accept `{ collection }`; omitting it addresses `default`.
- Records include a `revision` string. `put` resolves to the new revision only
  when `expectedRevision` (`"absent"` or a revision string) is given; otherwise it
  still resolves to `undefined`. `delete` accepts `expectedRevision` as well.
- `search(..., { explain: true })` returns `{ hits, stats }`. `searchAuto` returns
  `{ hits }` plus `plan` and `stats` with `explain`. `searchHybrid(text, k, ...)`
  fuses BM25 over the collection's `textFields` with an optional vector leg.
- `buildIndex`, `searchApproximateReady`, `searchAuto`, `annQuality` and
  `annMemory` accept `quantization` (`"none"`/`"scalar_u8"`) and
  `rerankMultiplier`. Omitted metric, quantization and rerank multiplier come from
  the collection's settings; `default` keeps squared L2 without quantization.
- `listCollections`, `createCollection`, `dropCollection` and
  `setCollectionSettings` (full replacement) manage collections. Setting keys are
  camelCase in JavaScript and are returned in the native snake_case shape.
- `importChunk(job, chunk, rows, { policy: "skip_invalid" })`, `importStatus`,
  `importFinish` and `importAbort` give exactly-once chunks that survive reopen.
  Job and chunk numbers are decimal strings.

Automatic maintenance, scheduled backups, metrics and slow-query logs, and
server authentication credentials and audit logs are server-only; this package has no API for them.

## Lifetime and failure behavior

Each `NativeLibrary` owns one worker. Always await `native.close()` in `finally`,
including after failed database creation. It drains accepted requests, closes
remaining database handles and stops the worker. Do not forcibly terminate a
worker during a write; a disconnect/crash may leave the mutation outcome unknown.
Database close rejects new database calls and drains previously admitted ones.
Explicit close is required; there is no finalizer that hides failures.
On Linux, the native library remains mapped until process exit so Rust's
thread-local destructors stay valid after worker cleanup. Database handles and
file locks are still released by close; this does not keep workers alive.

The queue admits at most 64 pending operations by default (`maxPending`: 1..1024);
`busy` means rejection before admission. Library shutdown bypasses that cap so
cleanup can always be requested. A database close rejected as `busy` retains its
handle; await admitted work before explicitly retrying close. No writes are
automatically retried. Requests are limited to 8 MiB. JSON encoding on the main
thread is still synchronous and bounded; native work runs in the worker.
This is asynchronous scheduling, not a query latency guarantee.
The internal worker does not inherit the application's `execArgv` flags/loaders.

`EmbeddedError.code` preserves engine codes, including `commit_indeterminate`,
`poisoned`, `locked`, `not_found` and `index_not_ready`. `worker_failed` and
`native_failure` mean a native outcome may be unknown; do not blindly retry a
mutation. Native process crashes cannot be contained by a worker thread.
The native ABI serializes all handles through one mutex; multiple workers do
not make the engine parallel. Writers retain exclusive file ownership across
processes/REST/native bindings; only read-only handles share file locks.

For advanced deployments, pass absolute `libraryPath` and `addonPath` to the
constructor. There is no PATH search or automatic native download. N-API v8
avoids binding to one Node major version, but the native platform must match.
This package is Node-only, not a browser or edge-runtime binding.


## V5 embedded feature API

New methods use additive JSON operations through the existing ABI-1 library and
worker; there are no new native symbols and no network calls. Use a matching
new bundled library. Default creation remains format 3. Pass `{format:5}` for
TTL/metadata-only mutations and named/sparse/model-versioned vectors.

```js
const db = await native.createInMemory(2, { format: 5 });
try {
  const cursor = await db.feedCursor();
  await db.putWithExpiry("1", [1, 0], {
    expiresAtMs: "2000000000000", metadata: { tag: { type: "string", value: "example" } },
  });
  await db.updateMetadata("1", { tag: { type: "string", value: "updated" } });
  console.log(await db.searchGrouped([1, 0], 5, 2, "tag"));
  await db.configureVector("text", 2, { activeVersion: "model-v1", history: 1 });
  await db.putVectors("1", {
    dense: { text: [{ version: "model-v1", vector: [1, 0] }] }, sparse: { terms: [[7, 0.8]] },
  });
  console.log(await db.searchNamed("text", [1, 0], 5));
  console.log(await db.searchDenseSparse("text", [1, 0], "terms", [[7, 1]], 5));
  console.log(await db.query("SEARCH default NEAR $q K 5", { q: [1, 0] }));
  console.log(await db.changesAfter(cursor));
  const image = await db.toBytes(); // save these bytes in host-owned storage
  const restored = await native.openFromBytes(image);
  await restored.close();
} finally { await db.close(); }
```

Also exposed, with explicit TypeScript declarations and `{collection}` support:

- `put(...,{expiresAtMs})` for unconditional TTL writes, or `putWithExpiry` for
  a returned revision and optional null expiry. Expiry is absolute Unix-ms, not a
  duration. `updateMetadata` returns a revision, replacing metadata but retaining
  vector/payload/expiry; ordinary puts clear additional vectors. TTL writes
  reject revision preconditions rather than ignoring them.
- `recommend(k,{positiveIds,negativeIds,positiveVectors,negativeVectors})`,
  `facets(field)`, `numericStats(field)`, and
  `searchGeo(vector,k,field,latitude,longitude,radiusMeters,{sort:"geo_distance"})`.
  These retrieval methods support optional filters/metrics. Geo metadata uses
  `{type:"geopoint",value:{latitude:0,longitude:0}}`.
- `vectorSpecs`, `recordVectors`, `putVectorVersion`, `pruneVectorHistory`,
  `searchSparse`; `searchNamed(...,{version})` chooses retained history.
  `putVectors` replaces all additional vectors, not a partial merge. Sparse
  pairs must be sorted by unique u32 index; distance is negative dot product.
- `rerank(vector,candidates,limit,{method:"exact",metric})` or
  `{method:"mmr",lambda:0.5}`. MMR always uses cosine; omit `metric`. Candidate
  IDs resolve current live records. No cross-encoder callback crosses the worker.
- `feedCursor()` starts at head. `changesAfter(cursor,maxEvents)` returns
  `{changes,next,has_more,head}`; omit the cursor to start at the beginning.
  Persist `next`; committed batches are never split. TTL expiry is not an event.
  `query` executes only the bounded, read-only grammar in `docs/VECTORS.md`.
- `native.create/open(...,{key:Uint8Array32})` or `{passphrase:"..."}`;
  credentials also work for in-memory create and byte-image open. Never pass
  both. Encryption implies V5 unless an incompatible explicit format was given.
  `isEncrypted()`, `encryptionKeyId()` and `encryptionInfo()` expose status only.
- `native.restoreTo(closedLog,newPath,{sequence:"42"})` or `{timestampMs:...}`,
  optionally `baseBackup`/credentials. Timestamp restore needs database creation
  with `{commitTimestamps:true}`. Sources must be closed/copied; destination must
  not exist. Restore never edits a source and assigns a fresh lineage.
- `buildDiskAnn(path,{metric})`, `searchDiskAnn(path,vector,k,{efSearch,cacheBytes})`.
  Each search opens/closes its own sidecar and cache; any database commit makes
  the index stale. Disk ANN does not filter metadata/expiry and encrypted
  databases refuse plaintext sidecars. Builds use ordinary HNSW build memory.

IDs/revisions remain decimal strings. Input expiry/restore timestamps and
sequences accept decimal strings or non-negative safe JS integers; unsafe numbers
are rejected. Feed heads/timestamps, lineage, key IDs and restored revisions are
strings. New retrieval methods return arrays directly; `query` returns an object
with `hits`, `groups`, and `facets`. Returned option/status keys retain native
snake_case. Byte images are Uint8Array-compatible Buffers. JSON requests/responses
have an 8-MiB limit including base64 overhead, limiting the image sizes supported.

Encryption protects frame contents, not frame prefixes/access patterns or live
process memory. Worker/JSON credential copies cannot be reliably wiped: do not
log requests. In-memory data is durable only after the host saves `toBytes()`.
Read replicas, GPU, named ANN and re-embedding workers are not exposed. See
`bindings/c/FEATURES.md` for the complete result/error and ownership contract.
`npm test` runs both legacy tests and real-engine feature scenarios, including
cross-process read-only locking. Set `VLITE_TEST_LIBRARY` when running
`node --test bindings/node/features.test.mjs` to select an alternate native library;
the default uses the package's platform prebuild.

`npm run typecheck` also checks compile-time examples of every new method.
