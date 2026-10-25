# VLite REST clients

First-party clients for Python 3.11+, TypeScript/JavaScript (Node.js 22+),
[Go 1.22+](#go) and [.NET 8+](#net).
They call the existing REST service; they are not embedded database bindings.
For server-free access, use the separate [Python/native bindings](../bindings/README.md)
or [embedded Node.js/TypeScript addon](../bindings/node/README.md).
Both use the MIT License. The Python client is published as
[vlite-client on PyPI](https://pypi.org/project/vlite-client/):
install it with `python -m pip install vlite-client`. The Python embedded
binding is [vlite-embedded on PyPI](https://pypi.org/project/vlite-embedded/);
see the [embedded deployment guide](../bindings/README.md). npm packages are
not published. The GitHub release assets and checkout installation below are
available for alternatives and contributor development.

## Python

Install the published package for application use:

```text
python -m pip install vlite-client
```

For contributor development, install the checkout from the repository root:

```text
python -m pip install ./clients/python
```

Use the URL and token supplied by your deployment:

```python
import os
from vlite_client import Client, VLiteError

client = Client(os.environ["VLITE_URL"], os.environ["VLITE_AUTH_TOKEN"])
client.put("10", {
    "vector": [1, 0, 0],
    "payload_base64": "AP8=",
    "metadata": {"category": {"type": "string", "value": "example"}},
})
print(client.get("10"))
print(client.search({"vector": [1, 0, 0], "k": 5}))
page = client.list_vectors(limit=25)
while True:
    print(page["items"])
    if page["next_after"] is None:
        break
    page = client.list_vectors(limit=25, after=page["next_after"])
```

Python is synchronous and uses the standard library with a 30-second socket
timeout by default (`timeout=60` overrides it). Environment HTTP proxies are
disabled; use a directly reachable service or HTTPS reverse proxy.

## TypeScript / JavaScript

Install the local package from your application's directory, adjusting the path:

```text
npm install /path/to/VLite/clients/javascript
```

```javascript
import { Client, VLiteError } from "@vlite/client";

const client = new Client(process.env.VLITE_URL, process.env.VLITE_AUTH_TOKEN);
await client.put("10", {
  vector: [1, 0, 0],
  metadata: { category: { type: "string", value: "example" } },
});
console.log(await client.get("10"));
console.log(await client.search({ vector: [1, 0, 0], k: 5 }));
let page = await client.listVectors({ limit: 25 });
for (;;) {
  console.log(page.items);
  if (page.next_after === null) break;
  page = await client.listVectors({ after: page.next_after, limit: 25 });
}
```

The ESM package includes TypeScript declarations and uses native `fetch`;
there are no runtime dependencies. `timeoutMs` defaults to 30000 and bounds the
request including response consumption. See [the typed example](javascript/example.ts).
Browser use requires a same-origin deployment: VLite does not enable CORS.
Keep the full-access token out of shipped frontend code and public sites.
Node.js is the tested runtime; browser clients are not a separate supported SDK.

## Supported operations and wire contracts

Exact search accepts `metric: "squared_l2" | "cosine" | "inner_product"` in the
request (Python uses a dictionary). Omission preserves squared L2.
Cosine distance is 1 minus similarity; zero queries are rejected and zero-norm
records excluded. Inner-product distances are negative dot products.
Results always sort by increasing distance then numeric ID.
The server supports these exact metrics and cosine ANN.
Build it with `client.build_index(metric="cosine")` (Python) or
`await client.buildIndex({ metric: "cosine" })` (JavaScript), then search with
`metric: "cosine"` and `mode: "approximate_ready"`. Index status includes
`metric` (null when absent; older servers omit the field). Only one graph is
retained. A metric mismatch returns 409 in ready-only mode, or synchronously
rebuilds in lazy mode. Inner-product ANN returns 422, with or without a filter.
There is no automatic downgrade or retry against older servers.

Python uses `list_vectors`, `index_status`, `build_index`; JavaScript uses
`listVectors`, `indexStatus`, `buildIndex`. Both expose `health`, `ready`,
`database`, `metrics`, `get`, `put`, `delete`, `batch` and `search`, plus
`get_versioned` (Python) / `getVersioned` (JavaScript) for conditional writes.
`delete` returns a boolean; `search` returns the hit array. Mutations returning
HTTP 204 complete without a result. Collections, new search modes, imports and
administration are described below. See [OpenAPI](../src/openapi.json) for
limits, metadata/filter shapes and server error codes.

- IDs are **canonical decimal strings**, including `18446744073709551615`.
  Integer metadata values are signed-i64 strings, not JavaScript numbers.
- Payloads remain canonical padded base64 strings; encode/decode binary data
  explicitly with your language's base64 utilities.
- Put replaces the entire record; omitted metadata/payload clears them.
  Batch accepts the REST operation objects and commits them atomically.
- Search accepts `exact`, `approximate` and `approximate_ready`, with the same
  `filter` field in every mode (see [Filtered search](#filtered-search)).
  Ready-only search never builds; explicitly call the build method.
- Listing returns IDs/metadata only, capped at 100 records and 1 MiB per page.
  Follow the returned cursor until null, even on short pages. Pages are live,
  not snapshots; index readiness is also only a point-in-time observation.
- Constructors require an HTTP(S) origin, without credentials, subpath, query
  or fragment, and a 32..256-character base64url bearer token. Use HTTPS beyond
  loopback. Tokens are private client state, never included in request URLs.
  Server-side validation remains authoritative; types do not replace validation.
- No automatic retries, redirects, index builds, repair, or fallback search.

## Filtered search

`filter` is an AND array of typed conditions on record metadata: `eq` (any
metadata type, strict type match) and `range` (integer or float bounds, either
optional). Omitted, `null` or `[]` keeps every record. The request object and
client signatures are unchanged; the same filter works in every mode:

```python
flt = [{"op": "eq", "field": "category", "value": {"type": "string", "value": "example"}},
       {"op": "range", "field": "rank", "min": {"type": "integer", "value": "10"}}]
exact = client.search({"vector": [1, 0, 0], "k": 5, "filter": flt})
ann = client.search({"vector": [1, 0, 0], "k": 5, "filter": flt,
                     "mode": "approximate_ready", "metric": "cosine"})
```

```javascript
const filter = [{ op: "eq", field: "category", value: { type: "string", value: "example" } }];
const hits = await client.search({ vector: [1, 0, 0], k: 5, filter, mode: "approximate" });
```

- **Exact** mode returns the true top k among matching records.
- **Approximate** modes (`approximate`, `approximate_ready`) with a filter require
  a server whose `GET /v1/capabilities` reports `features.filtered_ann: true`, and
  support `squared_l2` and `cosine` only; inner product stays exact-only. The
  filter is applied during graph traversal (non-matching records are still
  traversed but never returned), not to an unfiltered top k afterwards. Every hit
  matches the filter and reflects current (including buffered) writes and
  deletes; distances are exact for returned records.
- Approximate results contain **at most k and possibly fewer** eligible records,
  and recall is not guaranteed. A selective or non-matching filter may visit
  most or all of the graph, so it can be slower than an unfiltered query.
  `ef_search` widens how many eligible candidates are retained; it is not a cap
  on visited nodes or time. Use exact mode when you need complete results.
- There is **no hidden exact fallback**. `approximate_ready` still returns 409
  `index_not_ready` when the graph is missing or built for another metric;
  malformed filters return 422 before readiness is checked.
- Older servers reject a non-null filter in either approximate mode with 422
  `invalid_input`. The clients do not probe capabilities for searches (unlike
  conditional writes, the rejection is explicit and nothing is mutated); they
  propagate that `VLiteError` without retrying or downgrading to exact search.

## Conditional writes (optimistic concurrency)

Requires a server whose authenticated `GET /v1/capabilities` reports
`api_version: 1` and `features.conditional_writes: true`. Signatures:

```text
Python:     get_versioned(id) -> {"record": Record, "etag": str}
            put(id, record, *, if_match: str | None = None) -> None
            delete(id, *, if_match: str | None = None) -> bool
JavaScript: getVersioned(id): Promise<{ record: VectorRecord; etag: string }>
            put(id, record, options?: { ifMatch?: string }): Promise<void>
            delete(id, options?: { ifMatch?: string }): Promise<boolean>
```

```python
current = client.get_versioned("10")
try:
    client.put("10", {"vector": [0, 1, 0]}, if_match=current["etag"])
except VLiteError as error:
    if error.status != 412:
        raise
    # Changed since the read: refetch, re-decide, and write again only if still intended.
```

```javascript
const { record, etag } = await client.getVersioned("10");
await client.delete("10", { ifMatch: etag }); // VLiteError 412 if stale
```

- The `etag` is the HTTP `ETag` of the record GET: an opaque, quoted, strong
  tag. Pass it back unchanged; do not parse, construct or persist it.
- Tag scope depends on the server. When `GET /v1/capabilities` reports
  `features.per_record_etags: true`, tags are **per record**: they change only
  when that record is written or deleted (including through a batch or import),
  and unrelated writes do not invalidate them. On format v4 files they survive a
  service restart; on v1-v3 files a restart invalidates every tag. GET responses
  also carry `revision` (a decimal string), which is informational only: pass
  the `etag`, not the revision, to conditional writes. Older servers use
  **database-wide, service-lifetime** tags: any committed write to *any* record
  or a restart invalidates every outstanding tag, so 412 does not prove this
  record changed.
- A stale tag, or a record deleted since the read, fails with `VLiteError`
  status 412 and code `precondition_failed`; nothing is written.
  Neither client retries; refetch with `get_versioned`/`getVersioned` and decide.
- Without `if_match`/`ifMatch`, `put` and `delete` are unconditional and behave
  exactly as before. Python `if_match=None` means unconditional. In JavaScript,
  the options object may contain only `ifMatch`; an explicitly present but
  `undefined`/`null`/malformed `ifMatch` is rejected, never downgraded.
- Older servers ignore `If-Match`, which would silently turn a conditional
  write into an unconditional one. So before **every** conditional `put` or
  `delete`, the client makes a fresh `GET /v1/capabilities` (a positive answer
  is never cached, because the server may restart as a different version). If
  the route is missing (404), the response is not an object, `api_version` is
  not exactly `1`, or `features.conditional_writes` is not exactly `true`, the
  client raises `UnsupportedError` and sends no mutation. Other capability
  failures propagate unchanged with no mutation sent: `VLiteError` (for example
  401) or `TransportError` (invalid response). Unconditional writes skip this
  check. This is a compatible-deployment guard, **not** protection against a
  mixed-version, load-balanced endpoint whose backend can change between the
  capability check and the write. Point each client at one server origin.
- Clients reject, before sending, any tag that is not exactly one strong tag
  of visible ASCII (`W/` weak tags, `*`, lists, whitespace and control
  characters). Python raises `ValueError`; JavaScript rejects with `TypeError`.
- `get_versioned`/`getVersioned` raise `TransportError` when the response lacks
  exactly one valid strong `ETag` (for example, an older server). Batch
  operations are not conditional. Transport errors on a conditional write leave
  the outcome uncertain, as described below.

## Collections (format v4)

Servers whose capabilities report `features.collections: true` (format v4
databases) host named collections. Each collection has its own dimensions, ID
space and settings. Names match `^[a-z][a-z0-9_-]{0,62}$`; the clients reject any
other name before sending. Legacy `/v1/vectors`, `/v1/batches`, `/v1/search` and
`/v1/index/hnsw` calls on `Client` continue to act on the `default` collection
with their historical defaults (squared L2, default HNSW config, no quantization).

```python
client.create_collection("docs", 768, {"metric": "cosine", "text_fields": ["title"],
                                       "metadata_indexes": [{"field": "category", "kind": "eq"}]})
docs = client.collection("docs")   # same record/search/index methods as Client
docs.put("1", {"vector": [0.0] * 768})
client.set_collection_settings("docs", {"quantization": "scalar_u8"})  # {"collection", "persisted"}
```

```javascript
const docs = client.collection("docs");
await docs.batch([{ op: "delete", id: "1" }]);
console.log((await client.listCollections()).items, await docs.info());
```

| Python | JavaScript | HTTP |
|---|---|---|
| `list_collections()` | `listCollections()` | `GET /v1/collections` |
| `create_collection(name, dims, settings=None)` | `createCollection(name, dims, settings?)` | `POST /v1/collections` |
| `get_collection(name)` / `coll.info()` | `getCollection(name)` / `coll.info()` | `GET /v1/collections/{c}` |
| `drop_collection(name)` -> bool | `dropCollection(name)` | `DELETE /v1/collections/{c}` |
| `set_collection_settings(name, partial)` | `setCollectionSettings(name, partial)` | `PUT /v1/collections/{c}/settings` |
| `coll.get/get_versioned/put/delete/list_vectors/batch` | `coll.get/getVersioned/put/delete/listVectors/batch` | `/v1/collections/{c}/vectors[/{id}]`, `/batches` |
| `coll.search(req)` / `coll.search_detailed(req)` | `coll.search(req)` / `coll.searchDetailed(req)` | `POST /v1/collections/{c}/search` |
| `coll.index_status()` / `coll.build_index(...)` | `coll.indexStatus()` / `coll.buildIndex({...})` | `GET`/`POST /v1/collections/{c}/index/hnsw` |
| `rebuild_status/start_rebuild/poll_rebuild/cancel_rebuild` | `rebuildStatus/startRebuild/pollRebuild/cancelRebuild` | `.../index/hnsw/rebuild`, `/rebuild/poll`, `/rebuild/cancel` |
| `coll.ann_quality(options=None)` | `coll.annQuality(options?)` | `POST /v1/collections/{c}/index/hnsw/quality` |
| `coll.maintenance_status()` / `coll.maintenance_tick()` | `coll.maintenanceStatus()` / `coll.maintenanceTick()` | `GET .../index/hnsw/maintenance`, `POST .../maintenance/tick` |

- Omitted `metric`, `config`, `quantization` and `rerank_multiplier` on
  collection routes use **that collection's** settings. An unknown collection
  is 404 `collection_not_found`; nothing falls back to `default`.
- Settings updates are partial: omitted fields keep their values. On a v1-v3
  file, creating collections and imports return 409 `requires_format_v4`;
  `default` settings changes apply in memory only (`persisted: false`).
- Dropping requires an empty collection (409 `collection_not_empty`); `default`
  cannot be dropped (409 `default_collection_immutable`).
- Collection index status is a superset of the legacy shape: `ready`, `config`,
  `metric`, `quantization`, `rerank_multiplier`, `graph_nodes`, `stale_nodes`,
  buffer counters, `settings_ready` and `memory` (graph, quantized code,
  codebook, buffer and resident vector bytes; null without a graph).
  `ann_quality`/`annQuality` measures recall@k against exact search on
  the **ready** index only (409 `index_not_ready` otherwise; it never builds) and
  may return `recall_at_k: null` with `skipped_reason` `"budget"` or `"empty"`.

## Search modes, explain and quantization

In addition to `exact`, `approximate` and `approximate_ready`, servers reporting
the matching capability flags accept:

- `mode: "auto"` (`features.search_auto`): the server's planner chooses an exact
  scan, an indexed exact plan, filtered ANN or ANN, using the collection's
  planner policy and metadata indexes. It never builds an index.
- `mode: "hybrid"` (`features.search_hybrid`): reciprocal-rank fusion of a
  vector leg (`vector_leg: "exact" | "approximate_ready"`) and a BM25 text leg
  over the collection's `text_fields`. `vector` is optional only in this mode;
  `text` and `fusion: {rrf_k, vector_weight, text_weight, candidates}` apply only
  here. Hybrid hits are `{id, score, distance, bm25, vector_rank, text_rank}`
  (nullable per leg) sorted by score descending then ID; every other mode sorts
  by distance ascending then ID.
- `explain: true` (`features.search_explain`): `search_detailed` /
  `searchDetailed` return the whole response including `stats` (records scanned,
  distance evaluations, quantized evaluations, graph nodes visited, buffered
  records merged, reranked candidates, index candidates) and `plan` (non-null for
  `auto` only). `search` still returns only the hit array.
- `quantization: "none" | "scalar_u8"` and `rerank_multiplier` (1-16)
  (`features.quantization`) select a scalar-quantized index identity, reranked
  with exact distances. Build or rebuild it explicitly
  (`build_index(quantization="scalar_u8", rerank_multiplier=4)` /
  `buildIndex({ quantization: "scalar_u8", rerankMultiplier: 4 })`); a missing
  or different identity in ready-only mode is 409, never a silent exact search.

Read-only and scoped `search` credentials may use `exact`, `approximate_ready`,
`auto` and `hybrid`; lazy `approximate` (which may build) needs the admin token
or the scoped `search_build` operation; otherwise it is 403 (`read_only` or
`forbidden_scope`). Errors are
propagated once; the clients never retry or downgrade a search to exact mode.

## Resumable imports (format v4)

| Python | JavaScript | HTTP |
|---|---|---|
| `begin_import(collection)` | `beginImport(collection)` | `POST /v1/collections/{c}/imports` (no body) |
| `import_chunk(job, chunk, ops, *, policy=None)` | `importChunk(job, chunk, ops, { policy })` | `PUT /v1/imports/{job}/chunks/{n}` |
| `import_status(job)` | `importStatus(job)` | `GET /v1/imports/{job}` |
| `finish_import(job)` / `abort_import(job)` | `finishImport(job)` / `abortImport(job)` | `POST /v1/imports/{job}/finish`, `/abort` |
| `import_ndjson(path, collection, *, chunk_size=1024, policy="reject_chunk", job=None)` | `importNdjson(path, collection, { chunkSize, policy, job })` (Node only) | all of the above |

Jobs and chunk numbers are decimal strings; chunks start at `"1"` and must be
contiguous. Each chunk (up to 4096 batch operations) commits atomically with
its checkpoint. Outcomes are explicit:

- `committed`: the chunk was written.
- `already_committed`: an identical resubmission of the last committed chunk;
  nothing was written. Use it to confirm a chunk whose response was lost.
- **Rejected**: with `policy: "reject_chunk"` (default) any invalid row raises
  `ImportRejectedError` (a `VLiteError`, HTTP 422 `import_rows_invalid`) whose
  `report` lists `invalid: [{row, code, message}]` with `outcome: "rejected"`;
  nothing was written and the job did not advance. `"skip_invalid"` commits the
  valid rows and reports the skipped ones.
- 409 `import_conflict` (for example a different payload for a committed chunk,
  a gap, a dropped collection, a superseded reservation, or a first chunk sent
  while an earlier reservation on a live collection has not committed; that
  message names the earlier job and nothing is written), 409 `import_limit`
  and 404 `import_not_found` are `VLiteError`s. Finishing a job with no chunk is
  409; aborting an unused reservation drops it.

`import_ndjson`/`importNdjson` reads one batch operation object per line (blank
lines ignored). The whole file is read and validated **before** a job is
reserved or resumed: a missing, unreadable, empty or malformed file (a
`ValueError`/`TypeError` naming the line) sends nothing and leaves no orphaned
job. Any failure after that rethrows the original error with the job ID
attached (`error.import_job` in Python, `error.importJob` in JavaScript). The
job is never aborted automatically, so committed chunks stay resumable or can be
aborted explicitly.
With `job`, it resumes: it reads the job status, skips chunks before
`last_chunk`, resubmits `last_chunk` and requires `already_committed` (proving the
same file, chunk size and policy), sends the rest, then finishes. A finished job
returns its status without sending anything. Any other outcome, a transport
failure or an error stops the import; nothing is retried automatically. A job
without a durable chunk (`durable: false`) does not survive a restart.

## Administration (admin token)

| Python | JavaScript | HTTP |
|---|---|---|
| `capabilities()` | `capabilities()` | `GET /v1/capabilities` |
| `prometheus_metrics()` -> str | `prometheusMetrics()` | `GET /metrics?format=prometheus` (text/plain) |
| `auto_maintenance()` / `set_auto_maintenance(policy)` | `autoMaintenance()` / `setAutoMaintenance(policy)` | `GET`/`PUT /v1/admin/maintenance/auto` |
| `tick_auto_maintenance()` | `tickAutoMaintenance()` | `POST /v1/admin/maintenance/auto/tick` |
| `backup_schedule()` / `set_backup_schedule(schedule)` | `backupSchedule()` / `setBackupSchedule(schedule)` | `GET`/`PUT /v1/admin/backups/schedule` |
| `run_backup_schedule()` | `runBackupSchedule()` | `POST /v1/admin/backups/schedule/run` |
| `list_backups()` / `create_backup()` | `listBackups()` / `createBackup()` | `GET`/`POST /v1/admin/backups` |
| `verify_backup(name)` / `drill_backup(name)` | `verifyBackup(name)` / `drillBackup(name)` | `POST /v1/admin/backups/{name}/verify`, `/drill` |
| `slow_queries()` | `slowQueries()` | `GET /v1/admin/diagnostics/slow-queries` |
| `list_credentials()` | `listCredentials()` | `GET /v1/admin/credentials` |
| `create_credential(request)` | `createCredential(request)` | `POST /v1/admin/credentials` |
| `rotate_credential(id, *, grace_seconds=None)` | `rotateCredential(id, { graceSeconds })` | `POST /v1/admin/credentials/{id}/rotate` |
| `revoke_credential(id)` | `revokeCredential(id)` | `POST /v1/admin/credentials/{id}/revoke` |
| `persistent_audit(*, limit=100)` | `persistentAudit({ limit })` | `GET /v1/admin/audit/persistent?limit=n` |

- Auto-maintenance and backup-schedule responses are `{config, state}`; PUT
  merges the given fields (auto-maintenance takes `interval_secs` and reports
  `interval_ms`). Auto-maintenance policy changes are memory-only. Backup schedule, verify and
  drill need a server started with an admin directory; a failed verify or drill
  is 422 `verification_failed` / `drill_failed` (verify also fails when the
  manifest or its metadata was altered). Backup names are validated as
  generated snapshot names (no paths) before sending. Without an admin
  directory, schedule changes return 409 `admin_directory_disabled`.
- Scoped credentials (`features.scoped_credentials`) are created only with the
  bootstrap admin token and an admin directory on a v4 file (otherwise 409
  `credentials_disabled` / `requires_format_v4`): `{name (1-64 chars),
  collections: ["*"] | [names], operations, expires_unix_ms}` with operations from `read`, `search`, `search_build`,
  `write`, `import`, `index`, `collections`, `diagnostics`, `backup`, and a
  mandatory expiry within 365 days. **The `token` from create/rotate is shown
  once.** Hand it to its consumer; never log, print, persist or include it in
  error reports. Lists never return secrets; credentials report `state`
  (`active`, `expired`, `revoked`) and their bound `lineage`. Rotating an
  inactive credential is 409 `credential_inactive`; 503
  `credentials_unavailable` / `audit_unavailable` mean nothing was changed.
  **503 `credentials_commit_indeterminate`** on create, rotate or revoke means
  the change *is* in effect but may not survive a crash, and no token is
  returned: re-list credentials and revoke or re-issue as needed; never treat it
  as "no change".
  Persistent audit returns `{items, enabled}`.
- A scoped token is used like any token (`Client(url, token)`); `capabilities()`
  then reports `role: "scoped"` and its `scope`. Calls outside the scope fail
  with 403 `forbidden_scope`; expired tokens with 401 `credential_expired`,
  revoked or unknown ones with 401 `unauthorized`. Scopes are
  bound to the collection identity and database lineage: a dropped and
  recreated collection, or a restored different database, fails closed.

## Errors and uncertain writes

`VLiteError` exposes HTTP `status`, server `code`, and `request_id` (Python) or
`requestId` (JavaScript). Missing records raise 404 rather than returning null.
Nonstandard HTTP errors use `http_error`; Python surfaces redirects as an HTTP
error, while fetch rejects them as `TransportError`.

`TransportError` means connection, timeout, response parsing or transport failure.
A write **may already have committed**, including after a timeout/disconnect.
Likewise, `commit_indeterminate` does not establish failure. Do not automatically
retry a mutation; inspect/reconcile current state and follow the recovery
contract. Clients do not expose credentials in their own error messages, but
application logs must still avoid logging tokens or sensitive returned data.

## Client development

Run from the repository root:

```text
python -m unittest discover -s clients/python -p test_client.py
npm --prefix clients/javascript ci
npm --prefix clients/javascript test
npm --prefix clients/javascript run typecheck
cargo build --locked --features server --bin vlite-server
python tests/clients_e2e.py target/debug/vlite-server
```

On Windows, pass `target\debug\vlite-server.exe` to the final command.
Integration tests start isolated servers on ephemeral loopback ports, create
temporary databases and tokens, exercise both languages and clean up.


## Go

The standard-library-only module is `github.com/mahpate/Vlite/clients/go`:

```sh
go get github.com/mahpate/Vlite/clients/go
```

Until a module version is tagged, use a local checkout with
`go mod edit -replace github.com/mahpate/Vlite/clients/go=/path/to/VLite/clients/go`.
No registry package publication is required for Git-backed Go modules.

```go
import (
    "context"
    "os"
    "time"
    vlite "github.com/mahpate/Vlite/clients/go"
)

client, err := vlite.New(os.Getenv("VLITE_URL"), os.Getenv("VLITE_AUTH_TOKEN"), 30*time.Second)
if err != nil { panic(err) }
defer client.Close()
ctx := context.Background()
if err := client.Put(ctx, "10", vlite.Put{Vector: []float32{1, 0, 0}}); err != nil { panic(err) }
hits, err := client.Search(ctx, vlite.Object{"vector": []float32{1, 0, 0}, "k": 5, "mode": "exact"})
_ = hits
_ = err
```

`Collection(name)` returns scoped `Records` (and an error for invalid names).
`ListCollections`, `CreateCollection`, `Info`, `Drop`, `SetSettings`, `Get`,
`Put`, `Delete`, `Batch`, `Search`, `SearchDetailed`, `Browse`, `IndexStatus`,
`BuildIndex`, `Health`, `Ready` and `Database` mirror REST. Each call accepts a
`context.Context`. `Browse(ctx, 25, after)` returns `Page.NextAfter`; follow it
until nil. Search options and collection settings use `Object`, preserving REST
field names, modes, typed filters and decimal-string integers. Structured record,
metadata, page and hit types are included. Dynamic JSON numbers use `json.Number`.

Use `errors.As(err, &apiError)` with `*vlite.APIError` to inspect `Status`, `Code`
and `RequestID`. `*vlite.TransportError` has code `transport_error` and unwraps
the underlying timeout/cancellation/network/JSON error. A mutation may have
committed when a transport error occurs: do not blindly retry. Redirects and
implicit proxies are disabled. Timeout zero selects 30 seconds; negative values
are rejected. Server-side validation remains authoritative.

```sh
cd clients/go
go test ./...
go vet ./...
```

## .NET

The `net8.0` library uses only `System.Net.Http` and `System.Text.Json`; there
are no NuGet runtime or test dependencies. From an application in this checkout:

```sh
dotnet add reference /path/to/VLite/clients/dotnet/VLite.Client/VLite.Client.csproj
```

```csharp
using VLite;

using var client = new Client(Environment.GetEnvironmentVariable("VLITE_URL")!,
                              Environment.GetEnvironmentVariable("VLITE_AUTH_TOKEN")!);
await client.PutAsync("10", new PutRequest([1, 0, 0]));
var hits = await client.SearchAsync(new { vector = new[] { 1, 0, 0 }, k = 5, mode = "exact" });
var docs = client.Collection("docs"); // Collection must already exist.
var page = await docs.BrowseAsync(limit: 25);
```

The methods correspond to Go with an `Async` suffix, except `Collection` and
`Dispose`. `SearchAsync` returns typed hits; `SearchDetailedAsync` and
administrative/collection info return `JsonElement`. Pass anonymous objects or
dictionaries with REST field names for search/settings/batch operations. Use
`MetadataValue("integer", "9223372036854775807")` for i64 metadata, never a JSON
number. All asynchronous calls accept a `CancellationToken`. The constructor's
optional `timeout` defaults to 30 seconds. `VLiteException` exposes `Status`,
`Code`, `RequestId`; `TransportException.Code` is `transport_error` and warns
about unknown mutation outcomes. Redirects/proxies are disabled by default;
custom `HttpMessageHandler` injection is for tests or explicitly controlled
transports (the caller is responsible for equivalent transport safety).

The package-free test executable uses a fake HTTP handler and exits nonzero on
failure. Run it rather than `dotnet test` (no test-runner package is needed):

```sh
dotnet build clients/dotnet/VLite.Client.Tests
dotnet run --project clients/dotnet/VLite.Client.Tests --no-build
```

Go and .NET cover the core REST surface above, including exact/ANN and typed
filters. The broader Python/JS convenience APIs for imports, backups, diagnostics,
credentials, conditional writes and maintenance are not yet wrapped in these two
clients. No client silently retries or falls back between search modes.

### Optional live Go/.NET e2e

Both suites run live checks only when `VLITE_URL` is set, using
`VLITE_AUTH_TOKEN`. Use a **disposable v4, 3-dimensional database**: they create/drop collections, write/delete
high record IDs and build an ANN index. To own a disposable v4 server automatically:

```sh
cargo build --locked --features server --bin vlite-server
python clients/go/live_test.py go
python clients/go/live_test.py dotnet
```

The helper waits for `/health`, runs the suite, stops its own child process and
removes its project-local database directory. It never reuses a user database.

## LangChain and LlamaIndex integrations

Optional extras do not affect the standard-library Python client:

```sh
python -m pip install 'vlite-client[langchain]'
python -m pip install 'vlite-client[llamaindex]'
# Or install both:
python -m pip install 'vlite-client[integrations]'
```

For contributor development from a checkout, extras can instead be installed
from the source directory:

```sh
python -m pip install './clients/python[langchain]'
python -m pip install './clients/python[llamaindex]'
python -m pip install './clients/python[integrations]'
```

Importing `vlite_client.integrations` or either adapter module is dependency-free.
Requesting an adapter class lazily imports its framework; missing dependencies
raise an actionable `ImportError` naming the extra.

```python
from vlite_client import Client
from vlite_client.integrations import VLiteVectorStore

client = Client(url, token)
# embeddings implements LangChain's Embeddings protocol.
store = VLiteVectorStore(client, embeddings, collection="docs", mode="exact")
ids = store.add_texts(["hello world"], metadatas=[{"category": "greeting"}])
docs = store.similarity_search("hello", k=3)
retriever = store.as_retriever()
store.delete(ids)
```

`similarity_search_with_score` returns raw VLite **distance** (lower is better).
The relevance-score hook uses `1/(1+distance)` for squared L2, `1-distance` for
cosine, or negative distance for inner product; these values are not calibrated
probabilities and inner-product/cosine relevance may be outside `[0,1]`.
Use REST condition arrays via `filter=`. `mode="approximate_ready"` never builds
or silently falls back; build the index explicitly using the underlying client.

```python
from vlite_client.integrations import VLiteLlamaIndexVectorStore
from llama_index.core import StorageContext, VectorStoreIndex

vector_store = VLiteLlamaIndexVectorStore(client, collection="docs", metric="cosine")
storage = StorageContext.from_defaults(vector_store=vector_store)
# embed_model must be explicitly configured for your environment.
index = VectorStoreIndex.from_documents(documents, storage_context=storage, embed_model=embed_model)
```

LlamaIndex supports dense/default queries, AND equality filters and inclusive
numeric `>=`/`<=` filters. Unsupported OR/operators, doc/node-ID query restrictions
and non-dense modes raise errors instead of silently losing constraints.
`delete_nodes` requires explicit node IDs; `delete(ref_doc_id)` browses all pages
then deletes the matching nodes. Deletion/search plus record fetches are live,
not snapshot transactions. Concurrent mutation may affect them.

Both adapters support dense distance modes, not VLite hybrid score mode.
Both adapters store text/full node JSON in base64 payloads and flat scalar
metadata in typed filter fields. Nested/null/list metadata and out-of-i64 integers
are rejected. LlamaIndex reserves `_vlite_ref_doc_id` for document deletion.
Collections must already exist with matching embedding dimensions; v3+ metadata
and explicit v4 collection creation/upgrades are the caller's responsibility.
Canonical u64 string IDs pass through; other framework IDs map deterministically
via 64-bit BLAKE2b. Original IDs are preserved in payloads; hashing has a theoretical
collision risk (including collisions with numeric IDs), so do not mix adapter and
unrelated raw records in one collection. Chunked adds/deletes use batches of at
most 4096; each chunk commits independently, and HTTP byte limits still apply.

Test without framework packages (fakes + explicit missing-dependency checks), or
install extras to additionally run real-framework smoke tests:

```sh
python -m unittest discover -s clients/python/vlite_client/integrations -p test_integrations.py -v
```

When running from a checkout without installing the client, set
`PYTHONPATH=clients/python`. CI exercises both dependency-free and real-package
matrices. Tests use deterministic fake embeddings and never call an embedding API.
