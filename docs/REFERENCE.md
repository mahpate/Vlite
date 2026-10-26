# VLite technical reference

Contracts, limits and durability rules for the engine and the REST service.
Commands run from the repository root. For installation, start with the
[README](../README.md).

VLite is an experimental embedded vector and graph database in Rust with its
own single-file storage engine. Legacy files hold one fixed-dimension vector
space; opt-in v4 files add named collections, and v5 adds record expiry and
GeoPoint metadata. Graph-enabled v5 files also store
[property graphs](GRAPH.md). Every format stores opaque payloads, commits single and
atomic batch writes durably and supports exact search. Optional in-memory HNSW
adds approximate search without changing the file, and an
[ANN snapshot](ANN-SNAPSHOTS.md) can keep the graph across restarts. Optional
REST and [gRPC](GRPC.md) services expose the same engine. There is no
distributed operation and no interactive transaction support.

Related references:

- [Collections and advanced retrieval](CAPABILITIES.md): v4/v5 files,
  revisions, expiry, metadata indexes, hybrid search, quantized ANN and
  resumable ingestion.
- [Storage features](STORAGE-FEATURES.md): encryption, point-in-time restore,
  change feed, replicas, disk ANN, GPU and WebAssembly.
- [Vector extensions](VECTORS.md): named and sparse vectors, model versions,
  providers and rerankers.
- [MCP server](MCP.md) and [agent skills](AGENT-SKILLS.md) for AI agents.
- [Upgrade notes](RELEASE-0.5.1.md) for the current release.

Sections below describe legacy and default files unless marked otherwise.

## Build and use

Requires Rust 1.89 or newer.

```powershell
cargo run --example basic -- demo.vlite
```

The example creates a **new** database and refuses to overwrite an existing
file. To use a local checkout from another crate, add a path dependency:

```toml
[dependencies]
vlite = { path = "../VLite" }
```

```rust
use vlite::Database;

fn main() -> vlite::Result<()> {
    let mut db = Database::create("vectors.vlite", 3)?;
    db.put(10, &[1.0, 0.0, 0.0], b"document A")?;
    db.put(20, &[0.0, 1.0, 0.0], b"document B")?;
    let hits = db.search(&[0.9, 0.1, 0.0], 5)?;
    assert_eq!(hits[0].id, 10);
    assert_eq!(db.get(10)?.unwrap().payload, b"document A");
    assert!(db.delete(20)?);
    db.close()?;

    let db = Database::open("vectors.vlite")?;
    assert_eq!(db.len(), 1);
    db.close()
}
```

IDs are `u64`, including zero. `put` replaces both the vector and the payload
of an existing ID. `get` returns `None` for a missing ID, and `delete` returns
`false` without writing. Dimensions are fixed at creation, from 1 through
65,536. Vectors hold finite `f32` values; NaN and infinities are rejected.
Payloads are uninterpreted bytes, up to 1,048,576 bytes per record (empty is
allowed).

`search` uses **squared Euclidean distance**, accumulated in `f64`. It scans
every live vector, keeps the best k in a bounded max-heap and orders hits by
distance, then by ascending ID. `k=0` returns no hits, and a `k` above the live
count returns every record. Query validation still applies when `k=0` or the
database is empty. No embedding-generation API is included.

### Exact distance metrics (v0.1.1)

Choose a metric per query with `Database::search_with_metric(query, k, metric)`
or `search_filtered_with_metric(query, k, conditions, metric)`, using
`DistanceMetric::{SquaredL2, Cosine, InnerProduct}`. REST and ABI requests take
`"metric":"squared_l2"`, `"cosine"` or `"inner_product"`. Omitting it means
squared L2, which is also what `search` and `search_filtered` use.

Results use the `distance` field and sort ascending, then by ascending ID.
Cosine distance is `1 - clamp(dot/(norm(a)*norm(b)), -1, 1)`, in [0,2].
Inner-product distance is the negative dot product and can be negative, so do
not treat it as a Euclidean distance. Accumulation is f64 over the finite f32
components, with no quantization, stored-vector normalization or file-format
change. An inner-product zero is canonical positive zero.

Cosine rejects a zero-norm query with `ZeroNormQuery` (`invalid_input` in the
ABI and REST, where it is a 422), even for k=0 or an empty database. Zero-norm
stored records are excluded, so fewer than min(k, eligible count) hits can be
returned. Nonzero subnormals are valid. Dimension, finite-value and filter
validation run first.

HNSW supports squared-L2 and cosine ANN. Inner-product ANN is rejected (REST
422), an index is never reused under a different metric, and there is no
implicit fallback. See [filtered approximate search](#filtered-approximate-search)
for filter-aware ANN. Upgrade SDKs together with the native library or server
they call.

## REST service (optional)

The default Cargo library is embedded-only. Build the HTTP service with the
`server` feature; it does not change the storage format:

```powershell
$env:VLITE_AUTH_TOKEN = python -c "import secrets; print(secrets.token_urlsafe(32))"
cargo run --release --features server --bin vlite-server -- vectors.vlite --create-if-missing 3
```

For real deployments, generate the token once and keep it in a secret
manager. Never print it, put it in a URL, commit it or pass it as a
command-line argument. For a guided container setup, see
[Docker operations](DOCKER-OPERATIONS.md).

### Authentication and binding

The server needs **exactly one** authentication mode before it opens the
database: `VLITE_AUTH_TOKEN`, `--token-file PATH` / `VLITE_AUTH_TOKEN_FILE`, or
explicit **`--insecure-local`**. Conflicting sources fail startup. Tokens are
32..256 base64url characters; use at least 32 random bytes. A token file may
end in one LF or CRLF, and no other whitespace is trimmed. Restrict the file
and its directory to the service account (UID 10001 in Docker). Empty,
malformed, unreadable or missing secrets fail closed. Tokens are read once, so
rotate by replacing the secret and restarting.

Data, metrics and OpenAPI routes require `Authorization: Bearer TOKEN`.
Missing, wrong or duplicate headers return 401 with
`WWW-Authenticate: Bearer`. The admin token grants full access. Read-only
and scoped credentials are described in
[server administration](CONSOLE-ADMIN.md#configuration-and-roles); dashboard
accounts, native TLS, request rates, quotas and tenants are in
[security](SECURITY.md). The API does not support CORS. The library's `router`
function is unauthenticated, for in-process use; `authenticated_router` with a
validated `BearerToken` is the network-safe option.

The server opens one exclusively locked file. `--create-if-missing N` creates
it only when the path is missing and checks that an existing file has
dimension N; without the option, a missing file fails startup. Corruption,
incomplete tails, lock conflicts and dimension mismatches also fail startup,
with no repair, replacement or retry. Parent directories must already exist.

The server listens on `127.0.0.1:8080` by default. Change it with
`--bind 127.0.0.1:9000` (numeric IP and port; IPv6 as `[::1]:9000`). A
non-loopback bind needs `--allow-remote` **and** authentication, and
`--insecure-local` never allows one. Use HTTPS, natively or through a trusted
proxy, for any non-loopback traffic.

### Routes and requests

`GET /openapi.json` serves the OpenAPI 3.1 contract, and the
[API guide](API.md) covers the full surface, including collections,
administration and sign-in. The core routes are:

- `GET /health`: public process liveness, independent of database health.
- `GET /ready`: 200 when the owner can read; 503 if busy, closed or poisoned.
- `GET /metrics`: authenticated operational snapshot, in JSON or Prometheus
  text.
- `GET /v1/database`: dimensions, format version and live record count.
- `PUT /v1/vectors/{id}`: insert or replace; 204 only after the durable commit.
- `GET /v1/vectors/{id}`: ID, vector, base64 payload, typed metadata and an
  `ETag` header; 404 when missing.
- `DELETE /v1/vectors/{id}`: `{"deleted":true|false}`; a missing ID is a no-op.
- `GET /v1/vectors`: paged browsing, described below.
- `POST /v1/batches`: `{"operations":[...]}` as one atomic commit; 204 on
  success.
- `POST /v1/search`: `{"vector":[...],"k":N,"mode":"exact"}` returns
  `{"hits":[{"id":"...","distance":0.0}]}`.

IDs are **canonical decimal strings** such as `"0"` and
`"18446744073709551615"`, not JSON numbers, so JavaScript keeps full precision.
Path IDs follow the same rule, with no plus signs or leading zeros. Payloads use
canonical padded standard base64; an omitted payload means empty bytes,
including on replacement. Unknown fields are rejected. Vectors must be finite
f32 values with the database's exact dimensions.

Search keeps the embedded metric ordering and tie rules, with k from 0 to
10,000. `mode` defaults to `exact`. `"mode":"approximate"` uses the rebuildable
HNSW index: `ef_search` defaults to `max(100,min(k,records))`, must be
1..100,000 and at least `min(k,records)`, and is rejected in exact mode. REST
builds use m=16, ef_construction=100 and seed=42, which are not configurable
per request. Recall and a full k are not guaranteed. A stale index is rebuilt
under the exclusive write guard; searches on a ready index share the read
guard.

`GET /v1/vectors?limit=25&after=123` pages records in ascending numeric ID
order with typed metadata only. `limit` is 1..100. `after` is an exclusive
canonical u64 bound; omit it for the first page, which includes ID 0.
Responses hold `items` and `next_after` (a string cursor or null). Follow
`next_after`, not the page length, because a 1 MiB serialized cap can shorten
a page. Fetch a record to see its vector and payload. Pages are not a
snapshot: inserts at or below the cursor are missed. There is no offset, total
count or listing filter.

PowerShell examples, for a native server or the Docker service:

```powershell
$base = 'http://127.0.0.1:8080'  # Docker: 'http://127.0.0.1:18084'
$auth = @{ Authorization = "Bearer $env:VLITE_AUTH_TOKEN" }
# Docker: $auth = @{ Authorization = "Bearer $((Get-Content .secrets\api-token -Raw).TrimEnd())" }
Invoke-RestMethod "$base/ready" -Headers $auth
Invoke-RestMethod "$base/v1/vectors/10" -Headers $auth -Method Put -ContentType 'application/json' -Body '{"vector":[1,0,0],"payload_base64":"ZG9jdW1lbnQgQQ=="}'
Invoke-RestMethod "$base/v1/vectors/10" -Headers $auth
Invoke-RestMethod "$base/v1/search" -Headers $auth -Method Post -ContentType 'application/json' -Body '{"vector":[0.9,0.1,0],"k":5}'
Invoke-RestMethod "$base/v1/batches" -Headers $auth -Method Post -ContentType 'application/json' -Body '{"operations":[{"op":"put","id":"20","vector":[0,1,0]},{"op":"delete","id":"10"}]}'
Invoke-RestMethod "$base/v1/vectors/20" -Headers $auth -Method Delete
```

### Limits, errors and shutdown

Transport limits are **8 MiB of JSON per request**, **16 in-flight requests**
and **16 queued or running database workers**, on top of the engine's batch
and payload limits: a valid 16 MiB engine batch can be too large for the API.
Reads run concurrently. Simple exact searches scan a bounded snapshot outside
the read guard; ANN, reranking, indexed filters and oversized searches hold
it. Writes take the exclusive guard. Opt-in
[durable group commit](CONSOLE-ADMIN.md#opt-in-durable-group-commit), off by
default, coalesces supported record writes without acknowledging any before
the shared sync. Slow requests can occupy slots, and there is no per-client
fairness, streaming or execution deadline. Use a proxy for connection, header
and body timeouts and network-level limits.

Errors use `{"error":{"code":"...","message":"..."}}`:

| Status | Meaning |
|---|---|
| 400 | Malformed JSON or ID |
| 401 | Authentication failed |
| 404 | Missing record or route |
| 405 | Unsupported method |
| 409 | Write needs a newer file format (batches v2+, metadata v3), or a ready-only ANN query has no matching index |
| 413 | Body too large |
| 415 | Wrong content type |
| 422 | Schema or engine validation |
| 503 | Busy, unavailable, poisoned or indeterminate commit |
| 500 | Internal failure; details go to stderr |

Credential, conditional-write and rate-limit statuses (403, 412, 429) are
described with those features. The HTTP stack can answer protocol errors that
are rejected before routing.

**A lost response or client disconnect does not mean a write failed.**
Accepted work can finish after the client disconnects. `commit_indeterminate`
means the write may already be committed: the owner is poisoned and readiness
fails. There is no automatic reopen, repair, retry or idempotency key. Stop
the server, inspect the file and resolve the outcome before retrying.

Ctrl+C, and SIGTERM on Unix or in Docker, stop admission, drain active
requests, wait for database workers and close the owner. Docker allows 30
seconds before a forced kill, and long requests can exceed that. Acknowledged
commits keep the engine's sync and hardware limits; they are not physical
power-loss guarantees.

### Dashboard

The REST binary serves the browser dashboard at `/dashboard`. Its HTML,
script and stylesheet are public static assets with no credentials or data.
People sign in with a username and password; the admin token stays available
through **Use an access token instead**. See the [dashboard guide](CONSOLE.md),
[sign-in and accounts](SECURITY.md#dashboard-sign-in-and-accounts) and the
[auth routes](API.md#dashboard-sign-in-and-accounts). Records render as text,
never HTML, under a restrictive same-origin CSP. Writes need confirmation and
are never retried automatically. Index builds take the exclusive guard and can
block reads.

The Compose `dashboard` profile serves the same assets from an unprivileged
Nginx container on loopback port 8081. It proxies same-origin `/api/*`
requests to the `api` service and forwards `Authorization` only there, so the
API needs no CORS.

```text
docker compose --profile dashboard up --build -d dashboard
# UI:  http://localhost:8081
# API: http://localhost:18084
docker compose --profile dashboard stop dashboard api
```

The UI container publishes to loopback only, with a read-only root filesystem
and no Linux capabilities. Nginx accepts the API's 8 MiB body limit and allows
index builds up to its one-hour proxy timeout. Set `VLITE_DASHBOARD_PORT` and
`VLITE_PORT` to avoid host port conflicts, and start the API with the same
profile; the dashboard connects to no other API origin.

### Docker deployment

The `api` profile publishes only to the host's loopback address and uses its
own named volume, separate from smoke-test data:

```text
python -c "import pathlib,secrets; p=pathlib.Path('.secrets'); p.mkdir(mode=0o700,exist_ok=True); (p/'api-token').write_text(secrets.token_urlsafe(32), encoding='ascii')"
docker compose --profile api up --build -d api
docker compose logs api
docker compose stop api
docker compose start api
```

Compose mounts `.secrets/api-token` read-only at `/run/secrets/api_token`,
never as an image layer or environment value. `.secrets` is Git-ignored and
excluded from the build context. Set `VLITE_TOKEN_FILE` to use another secret
file that UID 10001 can read, and keep its directory private: Compose
bind-mounted secrets do not enforce host permissions.

Set `VLITE_DIMENSIONS` before the first start (default 3) and `VLITE_PORT` to
change the host port (default 18084). `VLITE_DATABASE_PATH` selects the file
inside the data volume (default `/data/vectors.vlite`), such as a validated
restored copy. Changing dimensions never migrates a file; startup fails
instead. Inside the private Docker network the container listens on `0.0.0.0`
with `--allow-remote`, and other containers still need the token. The runtime
is non-root, with a read-only root, dropped capabilities and no-new-privileges.
`docker compose down` keeps the volume. Stop the service before offline
maintenance, and never mount one database into two servers.

### Conditional updates and deletes (optimistic concurrency)

`PUT` and `DELETE` on `/v1/vectors/{id}` accept an optional `If-Match` header
holding exactly one strong tag, copied unchanged from a record `GET`'s `ETag`.
The service compares the tag and writes while holding the owner lock, so for a
given tag at most one competing write succeeds.

| Request | Result |
| --- | --- |
| No `If-Match` | Unchanged: PUT 204, DELETE `{"deleted":...}` |
| Tag matches current record | Normal success response; one durable commit |
| Tag stale, from another file/record identity, or record missing | 412 `precondition_failed`; nothing written; PUT never creates |
| `*`, `W/"..."`, comma list, repeated header, unquoted/invalid bytes | 400 `invalid_if_match`; nothing written |

Tags are opaque; do not parse them. Servers that report
`features.per_record_etags` bind each tag to the file identity, collection ID,
record ID, the record's last-put revision and a full SHA-256 content digest, so
unrelated commits do not invalidate it. Records in one batch can share a
revision number but never a tag, and deleting and recreating an ID advances
its revision.

V4 tags survive a restart. Legacy-file tags include a random nonce chosen at
each service start, so they fail after a restart. Compaction, upgrade and
restore assign a new v4 lineage, so reacquire tags afterwards. If OS
randomness fails, the server does not start.

After a 412, `GET` again, reapply your change and decide whether to resend;
neither the service nor clients should retry automatically. Authentication and
read-only checks run first (401/403). A 503 `commit_indeterminate` still means
the write may have committed: inspect rather than retry. A successful PUT
returns the new `ETag`. `POST /v1/batches` has no conditional form.

Check `"conditional_writes":true` under `features` in `GET /v1/capabilities`
before sending a conditional write: older servers ignore `If-Match` and write
unconditionally, and servers without `per_record_etags` use process-local tags
that any commit invalidates.

## HTTPS, observability and releases

### HTTPS with Caddy

`compose.tls.yaml` adds a Caddy HTTPS reverse proxy with automatic
certificates. Point a DNS name at the Docker host, set `VLITE_DOMAIN` to it,
open TCP 80 and 443, create the token secret and run:

```text
docker compose -f compose.yaml -f compose.tls.yaml --profile api up --build -d api caddy
```

Clients call `https://YOUR_DOMAIN` with the same authorization header. Caddy
forwards the token over the private Docker network, so keep untrusted
containers off it. Persist Caddy's certificate and config volumes, keep the
API port on loopback, restrict ingress and pin the Caddy image by digest. The
example enables HSTS, so use a hostname that will stay HTTPS-only. HTTPS does
not encrypt database files, backups or container traffic, and no access log
records authorization headers or bodies. For TLS without a proxy, see
[native TLS](SECURITY.md#native-rest-tls-and-grpc-mutual-tls).

### Logs and metrics

Each routed request writes one JSON line to stderr with `event`, a
server-generated `request_id` (also returned as `X-Request-ID`), method, route
**template**, status and duration. Client-supplied request IDs are not
trusted, and request IDs are unique only within one process lifetime.
Headers, raw paths, query strings, IDs, payloads, vectors, metadata and tokens
are never logged. Startup prints the listening address on stdout; startup,
shutdown and indeterminate-commit failures go to stderr as JSON.

`/metrics` requires authentication and returns JSON, or Prometheus text with
`?format=prometheus`. Its process-local counters reset on restart: requests,
4xx/5xx failures, unauthorized requests, indeterminate commits, HNSW rebuilds,
total request seconds and total rebuild-triggering-search seconds (which
**include the triggering query**). Gauges are `database_bytes`,
`live_records`, `hnsw_buffered_ids` and `hnsw_buffer_vector_bytes` (original
plus current buffered vectors, not payloads or RSS; zero without a graph).
There are no histograms, and canceled requests may not be logged. Snapshots
are not transactional. Database gauges wait for writes and return 503 when the
owner is busy or closed; a poisoned owner reports `database_healthy: false`
with null gauges, never stale data. Give metrics access only to trusted
monitoring.

`GET /v1/admin/logs` reads the structured log ring, with `since` (a sequence
cursor), `level`, `q` and `limit` (1..1000, default 200), and returns
`entries`, `next_seq`, `dropped` and `capacity`. The ring lives in memory;
`--log-buffer` sets its size (default 2000, range 1..100000).
`GET /v1/admin/metrics/summary` returns uptime, request and search counters,
per-route latency percentiles over the last 1,024 samples, and database and
ring state. The [dashboard](CONSOLE.md) shows both as live diagnostics, not as
a monitoring backend.

### Releases

Each release publishes the three images for Linux amd64 and arm64 (see
[Docker operations](DOCKER-OPERATIONS.md)) and checksummed archives, and its
GitHub release pins the image digests. Native builds are tested on Windows,
Linux (x86_64 and arm64) and macOS, and the images are checked with REST,
HTTPS and browser tests before and after publishing. There is no `latest` tag
and no automatic rollout.

Every archive holds the native tools, the OpenAPI and protobuf contracts and
the docs. The Windows, Linux x86_64 and macOS archives add the C library,
deployment files, examples, benchmarks, and client and binding sources.
Archives are named `vlite-<rust-target>.zip`, each with a `.sha256` file.
Verify the checksum before running a downloaded binary; it proves integrity,
not authorship. Deploy a pinned digest without local builds:

```text
VLITE_API_IMAGE=ghcr.io/mahpate/vlite-api@sha256:YOUR_RELEASE_DIGEST docker compose --profile api up --no-build -d api
```

On Windows, set the variable with your shell's syntax. If the packages are
private, log in to `ghcr.io` with read access first.

### Upgrade and rollback

Follow the Docker [upgrade](DOCKER-OPERATIONS.md#upgrade) and
[rollback](DOCKER-OPERATIONS.md#rollback) steps. The rules:

- Stop the owner and confirm nothing else uses the volume. Run
  `vlite-maintain inspect`, then `backup` to a **new** path, and copy the
  backup off-host. Keep the previous image digest and token configuration.
- Migrate a format by compacting to a **new** file, inspecting it and
  switching the database path. Never edit a header or overwrite the only copy.
- After upgrading, check authenticated readiness, the record count and sample
  reads and searches before you retire the original file.
- To roll back, restore the backup to another new path and start the old
  version against it; an old binary may not read a newer format. Writes made
  after the backup are not merged back, so stop writers and decide how to
  handle them before restoring.
- Never run `docker compose down -v` as part of an upgrade.

## Atomic batch writes

New databases use format v3. A batch is one bounded, checksummed commit frame:

```rust
use vlite::{Database, Mutation};

fn main() -> vlite::Result<()> {
    let mut db = Database::create("batch.vlite", 2)?;
    db.write_batch(&[
        Mutation::Put {
            id: 10,
            vector: vec![1.0, 2.0],
            payload: b"document A".to_vec(),
        },
        Mutation::Put {
            id: 20,
            vector: vec![3.0, 4.0],
            payload: Vec::new(),
        },
        Mutation::Delete { id: 10 },
    ])?;
    assert!(db.get(10)?.is_none());
    assert!(db.get(20)?.is_some());
    db.close()
}
```

Every operation is validated before anything is written, so invalid input
never commits a valid prefix. Operations apply in order, and the last mutation
of a repeated ID wins. Deleting a missing ID is a no-op, and an empty batch
writes nothing. Limits are **4,096 operations** and **16 MiB of encoded body
bytes** (16 bytes per operation plus put vectors and payloads); larger batches
return `BatchTooLarge`.

The batch is one frame: under `Durability::Full` one `sync_all()` acknowledges
the whole batch before memory changes (periodic mode syncs it later). Reopen
validates the whole frame before replaying it, so a batch is never partly
visible: an interrupted frame makes open fail closed, and explicit tail repair
discards the whole frame (see [durability](#commits-recovery-and-durability)).
A failed write or sync poisons the handle and leaves the **whole batch**
indeterminate: after reopen it is either absent or fully present. There are no
interactive transactions or retry tokens.

**v1 files** keep reads and single-record writes in their original encoding,
but nonempty batches return `BatchRequiresUpgrade`. The library never rewrites
a header in place: close the database, compact it to a **new** path (which
writes v3), verify the copy and switch paths. Backup and restore keep the input
format. Older binaries reject newer formats. `Database::format_version()` and
`vlite-maintain inspect` report the version.

## Typed metadata and exact filtered search

Format v3 stores a typed map with each record. `Record::metadata` is a
`BTreeMap<String, MetadataValue>` of strings, i64 integers, finite f64 floats
or booleans. Arrays, nested maps and nulls are not supported.

```rust
use vlite::{Condition, Database, Metadata, MetadataValue};

fn main() -> vlite::Result<()> {
    let mut db = Database::create("metadata.vlite", 3)?;
    let metadata = Metadata::from([
        ("category".into(), MetadataValue::String("docs".into())),
        ("year".into(), MetadataValue::Integer(2026)),
    ]);
    db.put_with_metadata(10, &[1.0, 0.0, 0.0], b"document A", &metadata)?;
    let filter = [
        Condition::Eq {
            field: "category".into(),
            value: MetadataValue::String("docs".into()),
        },
        Condition::Range {
            field: "year".into(),
            min: Some(MetadataValue::Integer(2025)),
            max: None,
        },
    ];
    let hits = db.search_filtered(&[0.9, 0.1, 0.0], 5, &filter)?;
    assert_eq!(hits[0].id, 10);
    db.close()
}
```

`put_with_metadata` and `Mutation::PutWithMetadata` replace the **entire**
vector, payload and map. Plain `put` and `Mutation::Put` clear existing
metadata. `get` returns the stored map.

Each map allows **64 fields**, names of **1..256 UTF-8 bytes**, strings up to
**4,096 UTF-8 bytes** and **65,536 encoded bytes** in total, which also count
toward the batch limit. Names and strings are case-sensitive and not
normalized. Non-finite floats are rejected, and -0.0 equals +0.0 in
comparisons. Integers and floats never coerce, so i64 values keep full
precision.

A filter is an AND of up to **64** conditions: equality on any type, or an
inclusive range of one numeric type with `min`, `max` or both (min <= max).
Missing fields and mismatched types never match, and an empty filter matches
everything. Filters are validated even for k=0 or an empty database.
`search_filtered` is exact: it scans every record (there are no field indexes)
and filters **before** top-k selection instead of post-filtering the nearest
hits. For ANN, see [filtered approximate search](#filtered-approximate-search).

In REST, `metadata` is optional on PUT and batch puts (omitted means `{}`):

```json
{
  "vector": [1, 0, 0],
  "payload_base64": "",
  "metadata": {
    "category": {"type": "string", "value": "docs"},
    "year": {"type": "integer", "value": "2026"},
    "score": {"type": "float", "value": 1.5},
    "active": {"type": "boolean", "value": true}
  }
}
```

Integers are **canonical decimal i64 strings**, so JavaScript clients keep all
64 bits; floats are JSON numbers. Reads return the same form. Search takes a
`filter`:

```json
{
  "vector": [0.9, 0.1, 0],
  "k": 5,
  "mode": "exact",
  "filter": [
    {"op": "eq", "field": "category", "value": {"type": "string", "value": "docs"}},
    {"op": "range", "field": "year", "min": {"type": "integer", "value": "2025"}}
  ]
}
```

`filter` works in the `exact`, `approximate` and `approximate_ready` modes;
`null`, omission and `[]` mean unfiltered. Inner product is exact-only.
Malformed filters return 422 `invalid_input` before any readiness, zero-k,
empty-database or build handling.

**Migration:** v1 and v2 records open with empty metadata, and plain writes
(including REST puts with empty or omitted metadata) keep the legacy encoding.
Nonempty metadata, and any `Mutation::PutWithMetadata`, needs v3; otherwise
the write returns `MetadataRequiresUpgrade` (HTTP 409) without changing the
file. Upgrade with `vlite-maintain compact old.vlite upgraded.vlite`, verify
the copy and switch paths. The source is never rewritten, and compaction keeps
metadata. Older binaries reject v3 files even without metadata.

## Optional HNSW search

`Database::search` is always exact. `search_approximate` opts into a
deterministic HNSW graph (bounded degree, greedy upper-layer descent and beam
search at the base layer) that scores with the selected f64 distance and
breaks ties by ID. It can miss neighbors or return fewer than
`min(k, records)` hits. There is no recall guarantee and no automatic fallback
to exact search, which remains the correctness baseline.

Traversal and construction compare candidates with a fixed-order 16-lane f32
kernel (scalar and AVX2 give bit-identical results) that falls back to f64 on
f32 overflow or underflow; returned distances are re-scored with the exact f64
distance. After the first 1,024 insertions, construction inserts deterministic
batches of `min(inserted/8, 256)` nodes planned in parallel on native targets,
so the graph never depends on thread count; wasm32 runs the same batches
sequentially.

```rust
use vlite::{Database, HnswConfig};

fn main() -> vlite::Result<()> {
    let mut db = Database::open("vectors.vlite")?;
    let config = HnswConfig::default(); // m=16, ef_construction=100, seed=42
    db.build_hnsw(config)?; // optional: otherwise the first query builds it
    let hits = db.search_approximate(&[0.9, 0.1, 0.0], 5, 100, config)?;
    println!("{hits:?}");
    db.close()
}
```

Construction takes `m` (2..64), `ef_construction` (m..4096) and a u64 seed.
`ef_search` must be 1..100,000 and at least `min(k, live records)`, counting
zero-norm records. Larger values usually trade build time, memory or query work
for recall, though not on every dataset. Approximate methods validate their
inputs even for k=0 or an empty database, which never need or build a graph.

### Metric identity

`build_hnsw`, `search_approximate` and `search_approximate_ready` use squared
L2. Their `_with_metric` variants take a final `DistanceMetric`, as in
`build_hnsw_with_metric(config, metric)`. Only `SquaredL2` and `Cosine` are
supported; `InnerProduct` returns `UnsupportedAnnMetric`.

Each owner keeps one graph, identified by its construction parameters and
metric. Lazy queries rebuild a missing or mismatched graph synchronously;
ready-only queries return `IndexNotReady` instead. Reopening starts without a
graph unless the server loads a valid [ANN snapshot](ANN-SNAPSHOTS.md).

Cosine uses `1 - clamp(dot / (norm_a * norm_b), -1, 1)` in f64 and never
normalizes stored vectors. Zero queries are rejected, and zero-norm records
are excluded from the graph and from buffered results.

REST accepts `{"metric":"cosine"}` on the build route and `/v1/search`. Index
status reports `ready` and the active `metric` (null without a graph); both
must match your query. The dashboard builds for the selected metric and offers
inner product only for exact search.

### Explicit builds and ready-only queries

`search_approximate_ready(&query, k, ef_search, config)` never builds a graph.
It takes a shared reference and returns `Error::IndexNotReady` when a nonempty
query needs a missing or differently configured index; otherwise it behaves
exactly like lazy search.

Build explicitly in a maintenance window with `build_hnsw(config)`. The REST
build uses the default parameters:

```powershell
Invoke-RestMethod "$base/v1/index/hnsw" -Headers $auth -Method Post -ContentType 'application/json' -Body '{}'
Invoke-RestMethod "$base/v1/search" -Headers $auth -Method Post -ContentType 'application/json' -Body '{"vector":[0.9,0.1,0],"k":5,"mode":"approximate_ready","ef_search":100}'
```

The build route accepts `{}` (L2) or `{"metric":"cosine"}` and returns 204 when
done. Ready mode returns 409 `index_not_ready` for a missing index or a metric
mismatch: build first or choose another mode instead of retrying. Later writes
can invalidate the index again. Building never changes database bytes.

`GET /v1/index/hnsw` reports a point-in-time `ready` flag for the default
configuration, buffer counts and bytes, capacity limits and the configuration.
It never builds or counts as a rebuild, and a poisoned or closed owner returns
503. Readiness is a snapshot, not a reservation.

Explicit builds and lazy rebuilds increment `hnsw_rebuilds_total`, and
`hnsw_rebuild_and_search_seconds_total` adds explicit build time plus lazy
build-and-query time; ready queries never count. Explicit builds run on the
serialized worker, so requests queue behind them, and an admitted build
finishes even if the client disconnects. To build off the request path, use
[background rebuilds](#background-hnsw-rebuilds-rust).

### Mutation buffer

The graph stays fixed between rebuilds, and writes record changed IDs in a
bounded buffer. Searches skip those IDs in graph results (their old vectors
still serve as traversal bridges), scan the current vectors exactly and merge
the two. New writes are searchable at once, and deleted IDs never return. Only
original vectors are copied, not payloads or metadata.

The buffer holds at most **256 distinct IDs** and **8 MiB of vector bytes**
(retained originals plus current vectors). Repeated updates to one ID keep only
its first graph vector. Crossing either limit drops the graph, and the next
approximate query rebuilds it synchronously; a batch that crosses the limit
still commits. These are not process-memory limits. Invalid writes, empty
batches and deletes of missing IDs leave the buffer unchanged, and
`build_hnsw` always clears it. Candidate width grows by up to 256 buffered IDs
to offset stale graph candidates, which still does not guarantee recall or a
full k.

### Filtered approximate search

```rust
db.search_approximate_filtered_with_metric(&query, k, ef_search, config, &conditions, metric)?;
db.search_approximate_ready_filtered_with_metric(&query, k, ef_search, config, &conditions, metric)?;
```

Conditions come before the metric, as in
`search_filtered_with_metric(query, k, conditions, metric)`, and use the same
typed predicates and validation as exact search. Only L2 and cosine are
supported, and empty conditions take the unfiltered path. The lazy method
rebuilds like `search_approximate_with_metric`; the ready method never builds.

Filters apply during base-layer traversal, not after an unfiltered top-k. Any
reachable node can bridge the traversal, but only nodes whose **current**
metadata matches enter the results; matching buffered records are scored
separately and merged. `ef_search` is the number of eligible candidates kept,
**not** a visit or time cap, so a selective or no-match filter can visit every
reachable node. Up to k results return with exact distances, ordered by
distance, then ID. Matches unreachable from the entry point are missed, and
there is no hidden exact fallback, so use exact mode when completeness matters.
Servers that support this report `features.filtered_ann: true`; older servers
reject filters in approximate modes with 422.

### Index limits

The HNSW graph lives in memory; [disk-based ANN](STORAGE-FEATURES.md#disk-based-ann)
is a separate opt-in index. The first lazy query after reopen or overflow
blocks other operations while it rebuilds; background rebuilds, server
maintenance and [ANN snapshots](ANN-SNAPSHOTS.md) avoid that. Small datasets
can be faster with exact search. Building and searching allocate RAM without
recoverable out-of-memory handling, and `ef_search` does not cap visited nodes
or memory.

Fixed-workload tests require recall@10 >= 0.95 on 1,000 seeded 16-dimensional
vectors, including after 50 deletes, 50 updates and 100 inserts. This is not a
promise for arbitrary data.

## Proactive HNSW maintenance (Rust)

`maintain_hnsw(config, metric, policy)` is an explicit owner-thread tick. By
default, `HnswMaintenancePolicy` starts a replacement at **128 buffered IDs or
4 MiB of buffered vector bytes**, whichever comes first. Thresholds must be
positive and at most the 256-ID / 8-MiB buffer limits. The policy's
`HnswRebuildLimits` match [background rebuilds](#background-hnsw-rebuilds-rust).
Invalid values return `InvalidHnswMaintenancePolicy` or
`InvalidHnswRebuildLimits`.

```rust
use vlite::{Database, DistanceMetric, HnswConfig, HnswMaintenancePolicy};

fn tick(db: &mut Database) -> vlite::Result<()> {
    let report = db.maintain_hnsw(
        HnswConfig::default(), DistanceMetric::Cosine,
        HnswMaintenancePolicy::default(),
    )?;
    // The caller decides admission/throttling using ready, pressure and outcome.
    println!("{report:?}");
    Ok(())
}
```

Call the tick between mutation or query batches, not from a concurrent timer.
Each tick polls at most one matching job and may publish it. If no job
remains, it starts a snapshot when the graph is missing or a threshold is
reached, which can happen right after a publish. The report holds the poll
`outcome`, `started` and `status`. A terminal `Cancelled` or `Superseded` blocks
a restart for that tick only. A pending job for another configuration or metric
returns `HnswRebuildBusy` and is left alone. A tick can fail after publishing,
for example when the next snapshot exceeds its budget; the publication stands.

`hnsw_maintenance_status(config, metric, policy)` only observes. `ready` means
a matching graph exists. `active_buffer` reports used and remaining IDs and
bytes. A pending worker reports its identity, snapshot and processed record
counts, `worker_finished`, terminal reason and catch-up capacity. Counts
measure records, **not time or an ETA**, and a finished worker may have
failed; poll to find out.

| Pressure | Meaning (first matching condition wins) |
|---|---|
| `CatchUpOverflow` | Pending worker has overflowed and is being discarded. |
| `Unavailable` | No active matching graph; ready-only search may reject. |
| `CapacityExhausted` | Active or pending catch-up buffer has zero remaining IDs or vector bytes. The active graph may still be ready at the exact limit. |
| `RebuildRecommended` | Active graph reaches either configured high-water threshold. |
| `Healthy` | None of the above; not a future-readiness promise. |

Capacity is **not a count of writes left**: repeated IDs, vector sizes and
deletes all affect it, and one large batch can use it up. Thresholds start
work early but reserve nothing. Check `ready`, pressure and the outcome
together, then throttle writes, reject ANN requests or use exact search.
Durable writes are always accepted. The server's automatic maintenance is
separate and off by default.

## Background HNSW rebuilds (Rust)

The owner has an **opt-in** lifecycle: `start_hnsw_rebuild(config, metric,
limits)`, `poll_hnsw_rebuild()` and `cancel_hnsw_rebuild()`. REST, the C ABI,
the Python and Node wrappers and the dashboard expose it too, and server
scheduling is off by default. Try the
[`background_hnsw` example](../examples/background_hnsw.rs):

```powershell
cargo run --locked --example background_hnsw
```

`start_hnsw_rebuild` validates health, configuration, metric (L2 or cosine)
and limits, then builds the graph on one named thread without holding the file
or database lock. Only one job may be pending, so a second start returns
`HnswRebuildBusy`. If the thread cannot be created, start returns `Io` and the
current index stays. Owned [read snapshots](read-snapshots.md) provide separate
concurrent reads.

`HnswRebuildLimits` defaults to **100,000 records and 128 MiB of vector
bytes**, with maximums of 1,000,000 records and 256 MiB; a larger snapshot
returns `HnswRebuildTooLarge` before capture. Unquantized capture also refuses
more than 256 MiB of record bodies, including payloads and metadata. These
limits do not cap RSS: map overhead, graph links, scratch space and the old
graph use memory too, and the old and new graphs coexist.

While a job runs, the owner tracks each changed ID against its snapshot-era
vector, within the **256-ID / 8-MiB** buffer limits. Overflow cancels the job
with `CatchUpOverflow`; writes stay committed and **no stale graph is
published**. A job whose catch-up does not cover the collection's current
version is superseded. Nothing falls back to exact search or a synchronous
build.

Call `poll_hnsw_rebuild` on the owner between requests. It returns immediately
while the worker runs. Once the worker ends, one poll joins it, applies the
catch-up buffer and swaps the index in a single serialized step, so deleted IDs
cannot reappear. Swapping and freeing the old graph take owner time.

| Poll outcome | Meaning |
|---|---|
| `Idle` | No pending job. |
| `Running` / `Cancelling` | Worker still owns resources; poll again later. |
| `Published` | New graph and catch-up buffer are active. |
| `Cancelled` | Caller cancellation discarded the replacement. |
| `Superseded` | An explicit synchronous/lazy build selected another graph; worker cannot overwrite it. |
| `CatchUpOverflow` | Bounded mutation catch-up was exceeded; snapshot discarded. Retry explicitly with a new snapshot or throttle mutations. |
| `Err(HnswRebuildFailed)` | Worker panicked/failed; active index is unchanged and storage remains usable. |

A terminal poll consumes the job. A newly published index may already need
replacing; publication promises neither future readiness nor recall. A
poisoned database rejects start, poll and search and never publishes.

Latency-sensitive callers should use `search_approximate_ready_with_metric`,
which returns `IndexNotReady` while no valid graph is published. **Do not**
retry with lazy search: it builds synchronously and supersedes the pending
job. Poll often and limit write rates to meet your availability needs; no
sustained write rate is guaranteed.

Cancellation is cooperative and checked between node insertions; it does not
kill the thread. `cancel_hnsw_rebuild` returns false when no job exists and
never replaces the active graph. `close()` cancels and joins a pending worker
and reports its failure; `Drop` does the same without reporting. Reopening
recovers committed data, not index state. Background rebuilds write no sidecar
and do not change the file format.

## Reproducible benchmark

`vlite-bench` generates deterministic SplitMix64 vectors, ingests them in
synced atomic batches, reopens the database and times exact squared-L2
queries. An independent brute-force oracle checks every result **outside** the
timed section, and a wrong result fails the run. The
[benchmark guide](../benchmarks/README.md) covers campaigns and comparisons.

```text
cargo run --release --bin vlite-bench -- 1000 32 100 100 10 42
docker compose run --build --rm --entrypoint vlite-bench vlite 1000 32 100 100 10 42
```

Arguments are `records dimensions queries batch_size k seed`, with the
defaults shown; `batch_size=1` measures one sync per record. The run uses a
private temporary database in the current directory (`/data` in Docker) or
`VLITE_BENCH_DIR`, never overwrites existing files and removes it afterwards;
an abrupt exit can leave it behind. Bounds: at most 1,000,000 records, 10,000
queries, 256 MiB of input vectors and 500 million query distance components,
plus the library's dimension and batch limits; k must be 1..records. Bounds do
not cap RAM or run time.

Output is one JSON object with the engine and format versions, platform, build
profile, inputs, dataset checksum, ingestion rate, warm-cache reopen time,
p50/p95/p99 query latency, raw samples, file sizes and recall@k. Exact recall
must be 1.0; this is a correctness check, **not** an ANN claim.
`process_peak_rss_bytes` is `null` unless you use the [memory wrapper](#memory).
Record hardware, filesystem and build profile with your results, and do not
compare this synthetic workload directly with other projects' benchmarks.

### ANN comparison

```text
cargo run --release --bin vlite-bench -- --ann 16 100 100 1000 32 100 100 10 42
docker compose run --build --rm --entrypoint vlite-bench vlite --ann 16 100 100 1000 32 100 100 10 42
```

The `--ann m ef_construction ef_search` prefix adds an `hnsw` object:
parameters, build milliseconds, warm p50/p95 and ID-set recall@k against the
exact top-k (ties by ID). The construction seed is fixed at 42. Each
candidate's distance, uniqueness and order are verified, and timing excludes
the build, warmup and checks. No recall target is forced.

### Memory

```powershell
python tests\measure_benchmark.py target\release\vlite-bench.exe --ann 16 100 100 1000 32 100 100 10 42
```

On Linux or macOS, run `python3 tests/measure_benchmark.py
target/release/vlite-bench --ann 16 100 100 1000 32 100 100 10 42`. The
wrapper fills in the child's peak resident memory (`PeakWorkingSetSize` on
Windows, `getrusage` elsewhere). It covers the whole process (dataset,
database, oracle and graph), **not marginal index memory**, and OS definitions
differ. Use a fresh wrapper per workload.

### Filtered search

`--filter-every N` benchmarks exact filtering on IDs divisible by N (about 1/N
of records), checked against the oracle. Output adds `filter_every`,
`eligible_records` and `returned_per_query`; k can exceed the eligible count.
It cannot be combined with `--ann`.

```text
cargo run --release --bin vlite-bench -- --filter-every 10 10000 32 100 100 10 42
cargo run --release --bin vlite-bench -- --filter-every 100 10000 32 100 100 1000 42
```

Exact search keeps at most `min(k, live count)` candidates in a bounded heap.
One Windows x86_64 release-mode comparison (32 dimensions, 100 queries, seed
42, sequential runs on the same host) measured p50 1.8302 ms for the previous
full-sort binary versus 0.7373 ms for the bounded heap at 10,000 records and
k=10. At 1,000 records and k=1,000 the heap was slower: 0.1339 ms versus
0.1143 ms. All results matched the oracle. These are workload-specific
observations; repeat them on your hardware.

### Real vectors, mixed writes and background rebuilds

`--vectors <file> --query-vectors <file>` loads finite f32 text rows.
`--mixed-writes N --ann ...` commits N replacements before every approximate
query and verifies against the current data. `--mixed-deletes N` adds deletes,
and external updates need `--update-vectors <file>` with exactly
`queries * mixed-writes` rows. Put these flags before the filtering and ANN
flags. See the
[native campaign guide](../benchmarks/README.md#native-profiling-real-vectors-and-mixed-updates)
and the [real-data campaign](../benchmarks/README.md#real-glove-embeddings-and-mixed-deletions).

`--rebuild-mode background`, optionally with `--query-interval-ms` (0..1000),
polls before each operation, starts a new worker after each terminal poll and
uses ready-only queries, recording rejections rather than hiding them.
`--rebuild-mode maintenance` uses `maintain_hnsw` and records
`background.policy.capacity_samples`, and `lazy` is the default synchronous
path. Paced and background runs are limited to 200 queries, 100,000 records and
128 MiB of snapshot vectors. See the
[paired campaign guide](../benchmarks/README.md#background-rebuild-latency-and-availability).

## Storage maintenance

`vlite-maintain` inspects, backs up, restores and compacts closed databases.
Stop every owner first: each operation takes the source's exclusive lock and
returns `Locked` instead of waiting. Sources open read-only, and inspection
validates the whole file.

```text
cargo run --bin vlite-maintain -- inspect vectors.vlite
cargo run --bin vlite-maintain -- backup vectors.vlite backup.vlite
cargo run --bin vlite-maintain -- restore backup.vlite restored.vlite
cargo run --bin vlite-maintain -- compact vectors.vlite compact.vlite
```

The library equivalents are `vlite::maintenance::{inspect, backup, restore,
compact}`; `upgrade`, `verify` and `drill` serve opt-in v4 workflows (see
[collections and advanced retrieval](CAPABILITIES.md)).

- **Inspect** reports the format version, dimensions, live records, commit
  frames (a batch counts once), file bytes, valid-prefix bytes and
  incomplete-tail bytes. It never changes data. Corruption is an error, and an
  incomplete tail makes the CLI exit nonzero.
- **Backup** and **restore** produce byte-identical, validated copies and
  reject corruption and incomplete tails. Restore **never overwrites an
  existing database**.
- **Compact** writes only live records to a new v3 file, restarting sequences
  at one. IDs, dimensions, payloads and exact vector bits (including negative
  zero) are kept, so search results do not change. An empty live set gives a
  32-byte file that accepts new writes.

A v4 source compacts to v4 with its collections and settings. Restore and
compaction assign a new file identity, so reacquire conditional-update tags
afterwards.

Output is staged in a private temporary file beside the destination,
validated, synced and published without replacing any existing file,
directory or alias of the source. An abrupt exit can leave an orphaned staging
file that nothing removes; only the final name marks a published output. On
Unix, `PublicationIndeterminate` means the file is complete but its directory
entry may not be durable, so inspect it before retrying. On Windows, the
directory entry is not guaranteed to survive power loss. Filesystems without
no-clobber publication fail explicitly.

Compaction is **copy-out, never in place**. To reclaim space, stop all owners,
compact to a new path, inspect and verify the copy, point the application at
it, and keep the original until the copy and its backups are verified. Never
rename over an open database. Compaction needs disk space for both files and
RAM for the live records.

The Docker runtime image includes the maintenance tool. After the
[smoke test](#smoke-test) creates its fixture, run maintenance on the same
volume:

```text
docker compose run --build --rm vlite
docker compose run --rm --entrypoint vlite-maintain vlite inspect /data/smoke.vlite
docker compose run --rm --entrypoint vlite-maintain vlite backup /data/smoke.vlite /data/backup.vlite
docker compose run --rm --entrypoint vlite-maintain vlite restore /data/backup.vlite /data/restored.vlite
docker compose run --rm --entrypoint vlite-maintain vlite compact /data/restored.vlite /data/compact.vlite
docker compose run --rm vlite verify /data/compact.vlite
```

Rerunning with the same destinations fails on purpose; use new names rather
than deleting real backups. Backups hold the same sensitive data as the source
and are encrypted only when the source is an encrypted v5 database.

## One file and ownership

Each database is one self-contained file with **no WAL, journal or lock
files**. The append log is the canonical store, and the live map is rebuilt in
RAM on every open. Opt-in derived files, such as
[ANN snapshots](ANN-SNAPSHOTS.md) and
[disk ANN](STORAGE-FEATURES.md#disk-based-ann) indexes, can be rebuilt and are
never needed to move a closed database.

`Database::open` and `open_encrypted` hold an exclusive, nonblocking OS lock
for the handle's lifetime. `open_read_only` and
`open_read_only_encrypted(path, &key)` take a shared lock: many readers can
coexist across processes, but never alongside a writer. Conflicting opens and
repairs return `Error::Locked` at once. A read-only handle replays committed
state when it opens, so reopen it to see later commits. It rejects incomplete
tails, returns `Error::ReadOnly` for mutations without poisoning the handle
and can build an in-memory HNSW index without writing the file.

In Rust, `&self` reads can run concurrently and mutations need exclusive
access. The server shares one database behind a read-write lock: reads share
it, while writes, stale-index rebuilds and background index publication take
it exclusively. Python `open(path, read_only=True)`, Node
`open(path, {readOnly: true})` and the C ABI's `"read_only": true` open shared
readers. The C ABI serializes calls within a process, so use separate
processes for parallel embedded requests.

Large native exact scans (at least 4,096 candidate rows and rows × dimensions
≥ 524,288) run in parallel when more than one worker is available and keep
distance-then-ID order; wasm32 scans stay serial. `close()` consumes the
handle, and dropping it releases the lock without committing anything.

Use local filesystems that honor OS locks and flushes. Avoid NFS, SMB,
cloud-synced folders, hard-link or symlink aliases and external editors.
Never rename, replace, copy, truncate or modify an open database. Locks are
cooperative on some platforms and do not stop hostile software.

## Commits, recovery, and durability

Each `put`, successful `delete` or nonempty `write_batch` appends one
checksummed frame with a commit footer and calls `File::sync_all()`. Memory
changes, and the call succeeds, only after the sync succeeds. That is
`Durability::Full`; since v0.5.1 file handles default to `Periodic` (sync every
60 s), so the sync moves to a background flusher; see
[periodic durability](#periodic-lazy-durability). Open validates every header,
length, sequence number, checksum and footer before exposing any vector.

| Situation | Behavior |
|---|---|
| Complete valid frames | Replay in sequence; newest record or tombstone wins. |
| Interrupted final append | `open` returns `IncompleteTail`, without modifying the file. |
| Full frame with invalid checksum/footer, invalid lengths, bad sequence, or invalid header | `Corrupt`; neither open nor tail repair discards it. |
| Unsupported format version with a valid header checksum | `UnsupportedVersion`; no automatic migration. |
| Commit write or sync fails | `CommitIndeterminate`; reads/search/writes on that handle then return `Poisoned`. Close and inspect/reopen. |
| Repair truncation or sync fails | `RepairIndeterminate`; inspect/reopen before retrying. |
| Creation fails after creating the path | Error; a partial file may remain and is never silently overwritten. |

A failed operation is **not** proof that the mutation is absent: a complete
frame may have reached storage before the error. Reconcile by ID after
reopening. Batch outcomes are indeterminate too, and there are no retry
tokens.

An incomplete tail is never dropped silently. After backing up the **closed**
file, an application can explicitly allow destructive tail repair:

```rust
let report = vlite::Database::discard_incomplete_tail("vectors.vlite")?;
println!("discarded {} bytes", report.discarded_bytes);
let db = vlite::Database::open("vectors.vlite")?;
# Ok::<(), vlite::Error>(())
```

Repair validates the complete prefix and truncates only a structurally
incomplete final frame, reporting the discarded bytes and the retained commit
count. A clean file is a no-op, and a complete bad frame is never skipped.

**Limits:** truncated acknowledged data looks exactly like an interrupted
unacknowledged write, so repair accepts losing that last commit. Removing whole
frames from the end of a file cannot be detected without an external trusted
history. Checksums detect accidental damage, not tampering. Nothing silently
falls back to an older state.

Durability depends on the OS, filesystem, device and controller honoring sync;
VLite cannot protect against hardware that ignores flushes, media failure or
external modification. `create` syncs the file but not its parent directory,
so keeping a new file name across power loss needs platform-specific handling
by the caller. Tests simulate interrupted writes and process exits, not
physical power loss.

## Periodic (lazy) durability

`Durability::Periodic { interval }` (1 ms–60 s) is the default since v0.5.1
(`Durability::default()`, interval `DEFAULT_FLUSH_INTERVAL` = 60 s): a
per-handle trade of crash durability for write throughput. Every commit, vector
or graph, is still written in full to the OS before success, but `sync_all()`
moves to a background flusher that runs at most once per interval.
`Durability::Full` syncs every commit, as before v0.5.1.
`Durability::None` is the mode of in-memory handles (`create_in_memory`,
`open_from_bytes`): nothing is written to disk and the data is gone when the
handle is dropped. `set_durability` rejects `None` on file handles and
`Full`/`Periodic` on in-memory handles with `InvalidDurability`.

```rust
use vlite::{Database, Durability};
let mut db = Database::open("vectors.vlite")? // periodic, 60 s
    .with_durability(Durability::periodic_ms(100)?)?; // or Durability::Full
db.put(1, &[0.0; 3], b"payload")?;
db.sync()?; // explicit durable point
db.close()?; // final sync, reports failure
# Ok::<(), vlite::Error>(())
```

- A process crash loses nothing acknowledged. An **OS crash or power loss can
  lose acknowledged commits from about the last interval** (up to ~60 s by
  default); only a suffix is lost, never the synced prefix. Choose `Full` when
  every acknowledged write must survive an OS crash.
- `sync()` forces durability and `unsynced_commits()` counts pending commits.
  `close()` syncs and reports errors; a failed sync poisons the handle.
- The mode is not persisted: each open starts with the default. Read-only
  handles start no flusher, and replica followers stay `Full`. Change-feed
  reads, segment export and ANN sidecar saves sync first.
- Server: `--durability periodic|full|none` (default `periodic`) and
  `--flush-interval-ms N` (periodic only, default 60000). `none` serves a fresh
  in-memory database: it never creates, reads, locks or writes the path, needs
  `--create-if-missing <dimensions>`, logs a startup warning, loses every write
  on restart, and rejects `--ann-snapshot`, encryption keys and
  `VLITE_ADMIN_DIR` (backups, audit and accounts need a file).
- C ABI, Python and Node: `durability` (`"periodic"`, `"full"`, or `"none"` for
  in-memory only) and `flush_interval_ms` on open/create, plus `sync`.

After an OS crash, missing whole frames open normally; a partial final frame
returns `IncompleteTail` (explicit tail repair); a zero-filled suffix returns
`Corrupt { offset, .. }` at the first unsynced frame, so back up and truncate
to `offset` or restore. Repair never does this automatically.

## Opt-in file format v4

Format v4 is only created explicitly; opening an older file never upgrades it.
Older binaries refuse v4, and v1–v3 handling is unchanged.

The 32-byte header keeps the magic, version, dimensions and CRC-32. In v4,
bytes 16..24 hold a nonzero random file lineage, bytes 24..26 the collection
allocation floor, and bytes 26..28 stay zero. Collection IDs are never reused;
copy-out maintenance keeps the floor and assigns a new lineage.

V4 record and batch frames keep the checksummed 36-byte prefix and 16-byte
footer and store a little-endian collection ID in prefix bytes 5..7, with
byte 7 reserved as zero. Operations 5, 6 and 7 create, drop and update
collection settings. Each collection has its own dimensions and ID space.
Settings persist; derived metadata, text and HNSW indexes do not.

A v4 atomic batch can end with an import checkpoint entry (op 8). Ordinary
checkpoints have a 52-byte body: chunk number, per-chunk row counts, flags and
a 32-byte SHA-256 identity. Open-job snapshots written by compaction use a
60-byte body with high count bits, so cumulative counts never truncate. A
marker's length must match its flags. Invalid histories, collection reuse,
counter overflow and malformed fields are corruption even with valid
checksums. `tests/format_v4.rs` pins these rules against independent
encodings and a fixed v4 fixture.

## File formats v1, v2 and v3

All integers and IEEE-754 `f32` components are little-endian, never native
struct layouts. Checksums are IEEE CRC-32 (reflected polynomial `0xEDB88320`,
initial and final XOR `0xFFFFFFFF`, check value `0xCBF43926` for
`123456789`).

The 32-byte database header:

| Offset | Size | Value |
|---|---|---|
| 0 | 8 | `VLITEDB` followed by NUL |
| 8 | 4 | Format version, 1, 2 or 3; new files use 3 |
| 12 | 4 | Dimensions |
| 16 | 12 | Reserved, zero |
| 28 | 4 | CRC-32 of bytes 0 through 27 |

Each single-record mutation is a 36-byte record header, a body and a 16-byte
commit footer:

| Offset | Size | Value |
|---|---|---|
| 0 | 4 | `VREC` |
| 4 | 1 | Operation: 1=put, 2=delete |
| 5 | 3 | Reserved, zero |
| 8 | 8 | Sequence, starting at 1 and increasing by exactly 1 |
| 16 | 8 | ID |
| 24 | 4 | Vector component count |
| 28 | 4 | Payload byte count |
| 32 | 4 | CRC-32 of record header bytes 0 through 31 |
| 36 | variable | Vector `f32` bytes followed by payload bytes |
| after body | 4 | `VCMT` |
| after body + 4 | 4 | CRC-32 of the complete record header and body |
| after body + 8 | 8 | Repeated sequence |

Put component counts must equal the database dimensions; deletes have zero
components and zero payload bytes. Lengths and header CRCs are checked before
bodies are allocated, and unknown operations or nonzero reserved fields are
rejected. A tombstone removes its ID on replay, sequence exhaustion rejects
the next write, and the header is never rewritten.

Versions 2 and 3 add operation **3=batch** in the same outer frame: the ID at
offset 16 must be zero, offset 24 holds the operation count and offset 28 the
**total encoded body length**. Checksums, footer and sequence work as above,
and one batch advances the sequence by one. The body is exactly the declared
number of entries:

| Entry offset | Size | Value |
|---|---|---|
| 0 | 1 | Operation: 1=put, 2=delete; nested batches forbidden |
| 1 | 3 | Reserved, zero |
| 4 | 8 | ID |
| 12 | 4 | Payload length; zero for delete |
| 16 | variable | Put: database-dimension count of f32 values then payload; delete: empty |

Limits are checked before allocation. Entries must fit exactly, hold finite
vectors and respect payload limits; trailing bytes are rejected, and every
entry is checked before any is applied. A batch frame in a v1 file is
corruption.

Version 3 adds **4=put with metadata**, standalone or inside a batch. A
standalone op4 has the ID at prefix offset 16, dimensions at 24 and the
**total extended-body length** at 28; in a batch entry, offset 12 holds that
total. The extended body is the f32 components, a u32 payload length, the
payload and the metadata encoding, with no spare bytes. Empty-metadata
standalone puts may use op1; `Mutation::PutWithMetadata` always uses op4.

Metadata is a u32 field count followed by fields in strictly ascending UTF-8
name-byte order. Each field is a u16 name length, the UTF-8 name, a u8 tag and
the value: 1 string (u32 byte length plus UTF-8), 2 integer (i64 LE), 3 float
(f64 LE), 4 boolean (one byte, 0 or 1). Duplicate or unsorted names, invalid
UTF-8, non-finite floats, unknown tags, trailing bytes and limit violations
are corruption even with valid checksums, as is op4 in a v1 or v2 file.

The fixtures `tests/fixtures/v1.hex`, `v2.hex` and `v3.hex` were encoded
independently with Python `struct` and `zlib.crc32`. They pin complete file
bytes, replay, batch encoding and copy-out upgrade, and are format contracts:
never regenerate them to hide a writer change. A new frame meaning needs a new
format version and an explicit migration; reserved bytes and operation numbers
are never repurposed.

## Capacity and platform status

Files grow with **every historical mutation**; deletes reclaim space only when
you switch to a compacted copy. Open time is linear in log size, and memory is
proportional to live vectors, payloads and metadata. Exact search evaluates n
metadata maps and computes distances for e eligible records in
O(e*d + e*log(1+L) + L*log(1+L)) time, where L = min(k, n), with O(L)
candidate memory. There is no file or RAM quota, and running out of memory is
not recoverable, so plan capacity. HNSW adds O(n*m) adjacency plus build and
search scratch space.

CI runs the full test suite natively on Windows, Linux (x86_64 and arm64) and
macOS with Rust 1.89.0 and stable, plus Linux Docker checks for REST
persistence, HTTPS, the dashboard and SIGTERM shutdown. The arm64 images
themselves are not run in tests, Docker on macOS is untested, and container
tests do not prove power-loss durability. VLite uses its own storage format,
with no on-disk compatibility with other engines.

## Development and testing

[Tests](TOOLS.md#tests) lists the commands that validate a change. Docker
checks need **Linux containers** and Compose v2. They run Linux code even on
Windows and macOS hosts, so they do not replace native filesystem tests. With
WSL-only Docker, run them inside WSL, and prefer Docker-managed volumes over
bind mounts, network shares and synced folders.

### Smoke test

```text
docker compose build vlite
docker compose run --rm vlite
docker compose run --rm vlite
```

The first run creates `/data/smoke.vlite`, commits a batch of inserts,
updates and deletes, then verifies every record and search result from a
second process. The second run verifies the **same persisted file** without
changing it. Success prints `PASS` and exits 0; any error prints `FAIL` and
exits nonzero. Unexpected or corrupt files are never reset. For an
exit-code-driven run:

```text
docker compose up --build --abort-on-container-exit --exit-code-from vlite vlite
```

The container runs as UID/GID 10001 with a read-only root, dropped
capabilities and the `vlite-data` volume, which survives
`docker compose down`. For a fresh run that keeps existing data, use another
project: `docker compose -p vlite-fresh run --build --rm vlite`. The smoke
binary is a test fixture, not a general CLI, and also runs natively:

```text
cargo run --bin vlite-smoke -- run smoke.vlite
cargo run --bin vlite-smoke -- verify smoke.vlite
```

`docker compose --profile test run --build --rm tests` runs the whole Rust
suite in the development image, and runtime image builds run it by default.
Builds use the checked-in lockfile and Rust 1.89, but base images can change,
so images are not bit-for-bit reproducible.

### End-to-end and reliability tests

- `python3 tests/rest_e2e.py` starts an isolated Compose project on a random
  loopback port and checks authentication failures, CRUD, batches, exact and
  ANN search, validation, metrics, log redaction, restart persistence and
  copy-out maintenance.
- `python3 tests/tls_e2e.py` runs the real Caddy proxy with a generated local
  CA and checks the certificate chain, hostname, authentication and HTTPS
  CRUD. It does not prove public DNS or certificate renewal.
- `python3 tests/dashboard_e2e.py` drives the embedded and standalone
  dashboards in Chromium against isolated containers.
- `cargo test --locked --test reliability` replays four seeded CRUD and batch
  histories against an independent model across reopen, backup and
  compaction, killing owners around acknowledged and unacknowledged writes.
  Kill timing cannot hit every write boundary, and this does not certify
  power-loss safety.
- `cargo test --locked --test ann_reliability` runs three seeded mixed ANN
  histories across the 256-ID buffer boundary, overflow, rebuilds,
  configuration mismatch and reopen. Its 95% recall threshold applies only to
  that small dataset at `ef_search` 1000.

The end-to-end scripts keep their data volumes and clean up everything else.

### Release criteria

Update the website and package READMEs with each feature. Install examples
must match the latest release, experimental features stay gated and labeled,
and historical benchmarks and release notes keep their provenance. Keep
product prose free of capture dates and unneeded version labels.
`python tests/release_docs.py` catches stale versions, and
`python tests/content_budget.py` keeps pages within their size budgets.

VLite is pre-1.0, so formats and APIs can still change. A 1.0 release needs:

1. Frozen, documented contracts: Rust API, REST/OpenAPI, file format,
   migration rules, support matrix and versioning policy.
2. Release gates passing on the exact candidate: native minimum and stable
   Rust checks on Windows, Linux and macOS, Docker REST, HTTPS and browser
   acceptance, and every advertised image architecture verified.
3. Recovery and upgrade checks against retained fixtures: restarts,
   interrupted writes, corruption, backup, restore, migration and rollback.
4. Measured limits from sustained representative workloads: latency tails,
   recall, memory, replay time, file growth, buffer overflow and rebuilds.
5. License, dependency, authentication and deployment review, plus documented
   supported versions and vulnerability reporting.
6. Matching versions in `Cargo.toml` and `Cargo.lock`, release notes with
   known limits, a `v<version>` tag on the approved commit and images pinned
   by digest.
