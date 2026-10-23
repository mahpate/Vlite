# VLite technical reference

Commands in this reference run from the repository root. For installation,
project status and common workflows, start with the [README](../README.md).

VLite is an experimental embedded vector database written in Rust. It owns its
storage engine: it is not a SQLite wrapper. Legacy files expose one
fixed-dimension vector space. Opt-in v4 files support named collections with
independent dimensions; opt-in v5 adds record expiry and GeoPoint metadata. Both support opaque payloads, durable single-record
and atomic batch mutations, and exact nearest-neighbor search.

Optional REST and gRPC services expose the same engine. Optional in-memory HNSW
search supports approximate retrieval without changing the database file;
an explicit snapshot sidecar can cache derived graph state across restarts.
Distributed operation and interactive transactions are deferred; shared-lock
read-only handles and concurrent server reads are available in current source; see [storage features](STORAGE-FEATURES.md) for opt-in encryption at
rest, point-in-time restore, change feed, file-shipped replicas, disk ANN, GPU
and WebAssembly.
This is a correctness-first MVP, not a production-hardened storage engine.

See [collections and advanced retrieval](CAPABILITIES.md) for opt-in v4/v5,
per-record revisions, expiry and metadata-only updates, metadata indexes and query planning, hybrid search,
quantized ANN, quality diagnostics and resumable ingestion. Later sections
describe legacy/default workflows unless explicitly marked otherwise.
See [gRPC](GRPC.md) and [ANN snapshots](ANN-SNAPSHOTS.md) for their opt-in
contracts and [0.3.0 upgrade guidance](RELEASE-0.3.0.md) before deployment.

See [embedded vector extensions](VECTORS.md) for v5 named/sparse vectors, model
version history, resumable re-embedding, provider/reranker traits and parsed queries.

## Build and use

Requires Rust 1.89 or newer. Maintenance uses `tempfile` for private staging and
no-clobber publication; the vector storage format remains VLite's own engine.

```powershell
cargo test --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo run --example basic -- demo.vlite
```

The example creates a **new** database and refuses to overwrite an existing file.
When consuming this source locally, add a Cargo path dependency:

```toml
[dependencies]
vlite = { path = 'Q:\VLite' }
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

## Docker end-to-end testing

Requires Docker Engine with **Linux containers** and Docker Compose v2 or newer.
These commands work from the project root on Linux, macOS with a Linux Docker
VM, or Windows with Docker Desktop/WSL. Docker runs Linux code on all these
hosts; it does not replace testing the native Windows or macOS filesystem APIs.
The default smoke job has no network API. The opt-in `api` service below runs
a persistent HTTP server.

Build and run the persistent smoke test:

```text
docker compose build vlite
docker compose run --rm vlite
docker compose run --rm vlite
```

On the first invocation, `vlite-smoke` launches a separate process that creates
`/data/smoke.vlite`, atomically inserts vectors and payloads, updates a record, and
deletes a record in one batch. A second process reopens the file and verifies every expected record and
exact-search result. The second container invocation verifies the **same
persisted file** without changing it. Successful runs print `PASS` and exit 0;
any error or incorrect result prints `FAIL` and exits nonzero. Existing corrupt
or unexpected databases are never reset or repaired automatically.

The runtime uses a non-root UID/GID of 10001, a read-only root filesystem,
dropped capabilities, and a Docker-managed `vlite-data` named volume for writable
storage. No host-directory permissions need to be configured. The container is
intentionally a finite test job: exiting successfully is expected, not a crash.
For an exit-code-driven orchestration run:

```text
docker compose up --build --abort-on-container-exit --exit-code-from vlite vlite
```

Run the entire Rust test suite, including corruption, interrupted-write,
subprocess recovery, locking, and CLI tests, in the development/test image:

```text
docker compose --profile test run --build --rm tests
```

Every runtime image build also runs the complete suite before building the
release smoke executable. Builds use the checked-in Cargo lockfile and Rust 1.89;
the base-image tags can receive upstream updates, so bit-for-bit image
reproducibility is not claimed. The runtime image contains no Cargo or compiler.

Without Compose, equivalent persistent smoke commands are:

```text
docker build --target runtime -t vlite-smoke:local .
docker volume create vlite-smoke-data
docker run --rm --read-only --cap-drop ALL --security-opt no-new-privileges --mount type=volume,src=vlite-smoke-data,dst=/data vlite-smoke:local
docker run --rm --read-only --cap-drop ALL --security-opt no-new-privileges --mount type=volume,src=vlite-smoke-data,dst=/data vlite-smoke:local verify /data/smoke.vlite
```

The volume survives container removal and `docker compose down`. To start an
independent test without deleting existing data, use a different Compose project,
for example `docker compose -p vlite-fresh run --build --rm vlite`. Volume removal
is destructive; do not remove volumes containing databases you want to retain.
The smoke file is a reserved fixture, not a generic database CLI.

For a WSL-only Docker installation, enter the WSL distribution and change into
the mounted project directory before running these commands. On this development
machine that is `cd /mnt/q/VLite`. Prefer Docker-managed local volumes over host
bind mounts, network shares, and cloud-synced folders for locking and sync tests.
The default images build for the engine's architecture; Linux amd64 and arm64 are
intended targets where the upstream Rust/Debian images are available.

The same process-separated smoke test is available natively:

```text
cargo run --bin vlite-smoke -- run smoke.vlite
cargo run --bin vlite-smoke -- verify smoke.vlite
```

IDs are `u64`, including zero. `put` replaces both the vector and payload for an
existing ID. `get` returns `None` for missing IDs; `delete` returns `false` without
writing for a missing ID. Dimensions are fixed at creation, from 1 through 65,536.
Vectors contain finite `f32` values; NaN and infinities are rejected. Payloads are
uninterpreted bytes limited to 1,048,576 bytes per record, including empty payloads.

Search uses **squared Euclidean distance**, accumulating in `f64`. It scans every
live vector, retains the best k in a bounded max-heap, then orders those hits by
distance with ties broken by ascending ID. `k=0` returns no
hits; `k` above the live count returns every record. Query validation still applies
when `k=0` or the database is empty. Existing `search` and `search_filtered`
retain squared-L2 defaults; explicit metrics are described below. No
embedding-generation API is included.

### Exact distance metrics (v0.1.1)

`DistanceMetric::{SquaredL2, Cosine, InnerProduct}` is selected per query using
`Database::search_with_metric(query, k, metric)` or
`search_filtered_with_metric(query, k, conditions, metric)`.
REST and ABI search requests accept `"metric":"squared_l2"`, `"cosine"` or
`"inner_product"`; omission retains squared L2.

All results use the existing `distance` field and sort ascending, then ascending
numeric ID on ties. Cosine distance is `1 - clamp(dot/(norm(a)*norm(b)), -1, 1)`
in [0,2]. Inner-product distance is negative dot product and may be negative;
do not treat it as a nonnegative Euclidean distance. Accumulation is f64 over
finite f32 components, without quantization, stored-vector normalization or a
file-format change. Inner-product zero is canonical positive zero.

Cosine rejects a zero-norm query with `ZeroNormQuery` (REST 422 `invalid_input`,
ABI `invalid_input`), even for k=0 or an empty database. Zero-norm stored records
are excluded, including after metadata filtering, so the result count can be
less than min(k, live/filtered count). Nonzero subnormals remain valid. Dimension,
nonfinite-vector and filter validation still applies before returning results.

HNSW in v0.1.1 remains squared-L2 only. Version v0.1.2+ also supports
cosine ANN as described below; inner-product ANN remains rejected.
REST rejects inner-product ANN with 422. There is no
implicit fallback or index reuse under a different metric. Filter-aware ANN is
described under "Filtered approximate search" below.
The default L2 cutoff optimization is not used for cosine/inner product.
Older native binaries/servers do not understand explicit new metric fields;
deploy the v0.1.1 SDK and native library/service together for metric queries.

## REST service (optional)

The default Cargo library remains embedded-only. Build the transport with the
`server` feature; it uses Axum/Tokio without changing the storage format:

```powershell
$env:VLITE_AUTH_TOKEN = python -c "import secrets; print(secrets.token_urlsafe(32))"
cargo run --release --features server --bin vlite-server -- vectors.vlite --create-if-missing 3
```

Generate the token once and store it in a secret manager for real deployments;
the example creates a new token in the current shell. Do not print it, put it
in a URL, check it into Git, or pass it as a command-line argument.
The binary requires **exactly one** authentication mode before opening/creating
the database: `VLITE_AUTH_TOKEN`, `--token-file PATH` /
`VLITE_AUTH_TOKEN_FILE`, or explicit **`--insecure-local`**. Conflicting sources
fail startup. Tokens must be 32..256 base64url characters; use at least 32 random
bytes. Files may end in one LF or CRLF, but arbitrary whitespace is not trimmed.
Restrict file/directory permissions to the service account (and Docker's
non-root UID 10001 where applicable). Empty, malformed, unreadable or missing
secrets fail closed. Tokens are read once; rotate by replacing the secret and
restarting the server, updating clients accordingly.

The server opens one exclusively locked file. `--create-if-missing N` creates
only if the path is missing and checks that an existing file has dimension N.
Without that option, missing files fail startup. Corruption, incomplete tails,
locking conflicts, and dimension mismatches fail startup without repair,
replacement, or retry. Parent directories must already exist. File creation
retains the embedded engine's parent-directory sync limitations.

Native listening defaults to `127.0.0.1:8080`. Override with
`--bind 127.0.0.1:9000` (numeric IP/port; IPv6 uses `[::1]:9000`).
Non-loopback binding requires explicit `--allow-remote` **and authentication**.
`--insecure-local` never allows a non-loopback bind, even with `--allow-remote`.
Data, metrics and OpenAPI routes require `Authorization: Bearer TOKEN`; missing,
wrong or duplicate authorization headers return 401 with `WWW-Authenticate:
Bearer`. One token grants full read/write/metrics access; there are no roles,
per-client quotas, cross-origin/CORS support, or built-in TLS. Use HTTPS through a
trusted proxy for any non-loopback traffic. The embedding library's `router`
function is explicitly unauthenticated for local/in-process use;
`authenticated_router` with a validated `BearerToken` is the network-safe option.

The browser dashboard is bundled into the REST binary at `/dashboard`; its HTML,
`/dashboard/app.js` and `/dashboard/style.css` are public static assets with no
credentials or database contents. All data requests still authenticate using
the existing token entered into the page. The dashboard keeps that token in a
closure in page memory, clears the entry field after connecting, and never
writes tokens or database contents to browser storage/cookies or puts credentials
in URLs/inspector output. Theme/density alone can be saved by explicitly enabling
the appearance preference.
Reload/Disconnect clears it. Browser extensions/password managers and host
security remain outside this guarantee; never use an untrusted browser.

An optional standalone frontend image is available from the Dockerfile's
`dashboard` target and Compose's `dashboard` profile. It serves the same static
assets from an unprivileged Nginx container at loopback port 8081 and proxies
same-origin `/api/*` requests to the Compose `api` service, stripping the prefix.
This keeps the browser on one origin without adding CORS to the REST API. The
API service remains separately available on loopback port 8080 and continues to
require its bearer token. The proxy forwards `Authorization` only to the API;
the UI keeps it in page memory and it is not part of the URL or Nginx access log.
Both containers use the same named API data volume. The UI container is not a
security boundary for its bearer token: treat the token as full database access.

```text
docker compose --profile dashboard up --build -d dashboard
# UI:  http://localhost:8081
# API: http://localhost:18084
docker compose --profile dashboard stop dashboard api
```

The UI publishes to loopback only, uses a read-only root filesystem and runs
without Linux capabilities. Its `/tmp` is an ephemeral 16-MiB tmpfs. Nginx
allows JSON bodies up to the REST service's 8-MiB request limit and permits
long-running explicit index builds up to its one-hour proxy timeout. Set
`VLITE_DASHBOARD_PORT` and `VLITE_PORT` to avoid host port conflicts. The API
must be started with the dashboard Compose profile so the proxy can reach it;
the dashboard does not connect directly to arbitrary external API origins.

The UI supports known-ID CRUD, base64 payloads, typed metadata, exact filters,
exact/lazy-ANN/ready-only search, HNSW maintenance, overview metrics and a latest
request inspector. Put/delete/build require a confirmation dialog. Client
validation supplements, not replaces, authoritative server validation.
Sidebar anchors navigate the console without losing form state. Overview cards
are snapshots, not live counters; writes through the UI invalidate count/size
and index snapshots until refreshed. The example helper fills a nonzero unit
vector matching the connected database dimensions (up to 4096) plus demo
metadata, without issuing a write. Copying an editor vector into the search
query is also local-only. Browse and search result IDs load the full record into
the editor. The collection picker isolates v4 namespaces; embedding generation
is not included.
Untrusted record contents are rendered with text nodes, not HTML. Assets are
same-origin with a restrictive CSP, no third-party CDN or frontend runtime.
Browse pages use `GET /v1/vectors?limit=25&after=123`, with ascending numeric IDs,
typed metadata only and a strict 1..100 page-size range. `after` is an exclusive
canonical u64 bound; omit it for the first page (including ID 0). Responses
contain `items` and `next_after` (string cursor or null). Follow `next_after`,
not page length: a 1 MiB serialized JSON cap may shorten a page. Vectors and
payloads are omitted until click-to-inspect loads the existing record endpoint.
No snapshot isolation spans requests: inserts at/below the cursor can be missed,
and records deleted or updated between requests reflect current live state.
Refreshing the first page starts a new traversal. No offset scan, total-count
promise, listing filter, export or backup semantics are implied.

`GET /v1/index/hnsw` returns a point-in-time `ready` boolean for the default
REST configuration, buffer counts/bytes, capacity limits and configuration.
It does not build, mutate storage or increment rebuild counters. Poisoned or
closed owners fail with 503. The dashboard refreshes this on connect/build,
marks it stale after its own writes and offers manual refresh. External writes
can invalidate readiness immediately afterward; it is not a reservation.
Inspector latency includes browser/network/parsing, not isolated engine CPU.
Results may contain private data; screenshots/inspection should be handled
accordingly. Builds take an exclusive database guard and can block read requests.
Failures are shown without automatic retry; a lost mutation response may be
indeterminate. The dashboard does not provide roles, undo, backups or recovery.

For persistent Docker deployment, the opt-in service publishes only to the
host's loopback address and uses its own named volume, separate from smoke data:

```text
python -c "import pathlib,secrets; p=pathlib.Path('.secrets'); p.mkdir(mode=0o700,exist_ok=True); (p/'api-token').write_text(secrets.token_urlsafe(32), encoding='ascii')"
docker compose --profile api up --build -d api
docker compose logs api
docker compose stop api
docker compose start api
```

Compose mounts `.secrets/api-token` read-only at `/run/secrets/api_token`, not
as an image layer or environment value. `.secrets` is Git-ignored and excluded
from the Docker build context. Override `VLITE_TOKEN_FILE` to point at an
existing secret file. Ensure UID 10001 can read the file; keep its parent
directory private. Compose bind-mounted secrets do not automatically enforce
the desired host permissions. Do not use the example test tokens in production.

Set `VLITE_DIMENSIONS` before initial creation (default 3), and `VLITE_PORT` to
change the host port (default 8080). `VLITE_DATABASE_PATH` selects the path inside
the data volume (default `/data/vectors.vlite`), including a validated restored
or migrated copy. Changing dimensions never migrates a file;
it causes startup to fail. Within the isolated Docker network the container
listens on `0.0.0.0`, explicitly enabled by `--allow-remote`; other containers on
that network still require the bearer token. The runtime is non-root with
a read-only root, dropped capabilities and no-new-privileges. `docker compose
down` retains the volume. Stop the service before using offline maintenance;
never mount the same database into multiple servers.

Windows PowerShell examples (also usable against the Docker service):

```powershell
$base = 'http://127.0.0.1:8080'
$auth = @{ Authorization = "Bearer $env:VLITE_AUTH_TOKEN" }
# For Docker instead: $auth = @{ Authorization = "Bearer $((Get-Content .secrets\api-token -Raw).TrimEnd())" }
Invoke-RestMethod "$base/ready" -Headers $auth
Invoke-RestMethod "$base/v1/vectors/10" -Headers $auth -Method Put -ContentType 'application/json' -Body '{"vector":[1,0,0],"payload_base64":"ZG9jdW1lbnQgQQ=="}'
Invoke-RestMethod "$base/v1/vectors/10" -Headers $auth
Invoke-RestMethod "$base/v1/search" -Headers $auth -Method Post -ContentType 'application/json' -Body '{"vector":[0.9,0.1,0],"k":5}'
Invoke-RestMethod "$base/v1/batches" -Headers $auth -Method Post -ContentType 'application/json' -Body '{"operations":[{"op":"put","id":"20","vector":[0,1,0]},{"op":"delete","id":"10"}]}'
Invoke-RestMethod "$base/v1/vectors/20" -Headers $auth -Method Delete
```

`GET /openapi.json` serves the checked-in OpenAPI 3.1 contract from
`src/openapi.json`. Routes are:

- `GET /health`: process liveness, independent of database health.
- `GET /ready`: 200 when the owner can read, 503 if busy/closed/poisoned.
- `GET /metrics`: authenticated JSON operational snapshot (not Prometheus text).
- `GET /v1/database`: dimensions, format version, live record count.
- `PUT /v1/vectors/{id}`: insert/replace; 204 only after durable commit.
- `GET /v1/vectors/{id}`: ID, vector, base64 payload and typed metadata plus an
  `ETag` header; 404 when missing.
- `DELETE /v1/vectors/{id}`: `{"deleted":true|false}`; missing is a no-op.
- `POST /v1/batches`: `{"operations":[...]}`; one atomic commit, 204 on success.
- `POST /v1/search`: `{"vector":[...],"k":N,"mode":"exact"}`; `{"hits":[{"id":"...","distance":0.0}]}`.

IDs are **canonical decimal strings**, including `"0"` and
`"18446744073709551615"`, not JSON numbers; this avoids JavaScript integer
precision loss. Path IDs follow the same rule (no plus signs/leading zeros).
Payloads use canonical padded standard base64; omitted payload means empty
bytes, including replacement writes. Requests reject unknown fields. Vectors
are finite f32 values with the database's exact dimensions. Search preserves
the embedded metric ordering and tie rules, with k between 0 and 10,000.
`mode` defaults to `exact`. Explicit `"mode":"approximate"` enables the
rebuildable HNSW index; optional `ef_search` defaults to `max(100,min(k,records))`
and must be 1..100,000 and at least `min(k,records)`. It is rejected in exact
mode. REST construction uses m=16, ef_construction=100, seed=42 (not
per-request configurable). Approximate recall and a full k results are not
guaranteed. A stale-index approximate query retries with the exclusive write
guard to rebuild; ready-index searches share the read guard.

Transport limits are **8 MiB of JSON per request**, **16 in-flight requests**,
and **16 queued/running database workers**. These are additional to the core's
batch/payload limits: a valid 16 MiB engine batch may be too large for this API.
Read routes run concurrently on blocking workers under a shared
`Arc<RwLock<Option<Database>>>` read guard; writes take the exclusive guard.
Database work does not run on asynchronous I/O threads. Slow requests and exact searches can occupy slots; no per-client
fairness, streaming, rate limiting or execution deadline is promised. Use a
proxy for connection/header/body timeouts and network-level limits.

Errors use `{"error":{"code":"...","message":"..."}}`: 401 for authentication, 400 for malformed JSON or
IDs, 404 for missing records/routes, 405 for unsupported methods, 409 for batches
requiring a file-format upgrade (batch v2+, metadata v3), 413 for body size, 415 for content type, 422 for schema or
engine validation, 503 for busy/unavailable/poisoned or indeterminate commit,
and 500 for internal failures (details logged to stderr). HTTP protocol errors
rejected before routing may be handled directly by the HTTP stack.

**A lost response or client disconnect does not imply a failed write.** Accepted
blocking work can finish after disconnection. `commit_indeterminate` explicitly
means the write may already be committed; the owner is poisoned and readiness
fails. There is no automatic reopen, repair, retry, or idempotency-key support.
Stop the server, inspect the file and resolve the outcome before retrying.
Ctrl+C and, on Unix/Docker, SIGTERM stop admission and drain active HTTP requests,
then wait for database workers and close the owner. Docker allows 30 seconds
before a forced kill; long requests can exceed that grace period. Acknowledged
commits retain the core's sync and hardware limitations, not physical power-loss
guarantees.

The Rust tests cover in-process routing and real TCP subprocess restarts on
native platforms. With Docker and Python 3 on the host, run the complete
container lifecycle check:

```text
python3 tests/rest_e2e.py
```

It builds and starts an isolated Compose project on a dynamically assigned
loopback port, tests CRUD/batches/search/validation, gracefully stops and restarts
the container, and verifies persistence. It removes test containers/networks but
**retains and prints its database volume name**; it never deletes database data.
For WSL-only Docker, run this command within WSL from the mounted project root.

### Conditional updates and deletes (optimistic concurrency)

`PUT` and `DELETE` on `/v1/vectors/{id}` accept an optional `If-Match` header
holding exactly one strong tag copied unchanged from a record `GET`'s `ETag`.
The service compares that tag and applies the write while holding the database
owner lock, so for a tag at most one competing write succeeds.

| Request | Result |
| --- | --- |
| No `If-Match` | Unchanged: PUT 204, DELETE `{"deleted":...}` |
| Tag matches current record | Normal success response; one durable commit |
| Tag stale, from another file/record identity, or record missing | 412 `precondition_failed`; nothing written; PUT never creates |
| `*`, `W/"..."`, comma list, repeated header, unquoted/invalid bytes | 400 `invalid_if_match`; nothing written |

Tags are opaque; do not parse them. Servers advertising
`features.per_record_etags` bind each tag to file identity, collection ID, record
ID, that record's last-put revision and a full SHA-256 content digest.
Unrelated commits no longer invalidate a tag. Two records in the same batch
can share a revision number but never share the complete identity-bound tag.
Deleting and recreating the same ID advances its revision.

V4 tags survive service restart. Legacy files use a fresh OS-random service
nonce, so their tags fail after restart. Copy-out maintenance assigns a new v4
lineage; reacquire tags after compaction, upgrade or restore. Divergent raw
copies with different content cannot match solely because their sequences
coincide. OS-randomness failure prevents startup rather than weakening identity.
Older conditional-write servers without the per-record capability retain
their globally invalidated, process-local tag behavior.

After 412, `GET` again, re-apply the change and decide whether to resend.
The service never retries; do not retry automatically. Authentication and
read-only checks run first (401/403). A `commit_indeterminate` 503 keeps its
meaning: the write may have committed; inspect rather than retry.
Successful PUT returns the new ETag header; GET can also obtain the current tag.
`POST /v1/batches` has no conditional form.

Older servers ignore `If-Match` and would write unconditionally. Servers that
enforce it report `"conditional_writes":true` under `features` in
`GET /v1/capabilities` (both roles). Check that flag immediately before any
conditional write and do not send the write if it is absent or false.

## HTTPS, observability and releases

`compose.tls.yaml` adds a Caddy HTTPS reverse proxy with automatic certificate
renewal. Set `VLITE_DOMAIN` to a real DNS hostname pointing to the Docker host,
allow inbound TCP 80/443, provision the secret above, then run:

```text
docker compose -f compose.yaml -f compose.tls.yaml --profile api up --build -d api caddy
```

Use `https://YOUR_DOMAIN` with the same authorization header. Caddy forwards
the token over the private Docker network; do not attach untrusted containers
to it. HTTPS does not encrypt database files, backups, or container-network
traffic. Caddy's certificate/config volumes must persist. The API's direct host
port remains loopback-only; never publish plain HTTP publicly. Restrict ingress,
apply host/proxy connection limits, monitor certificate renewal, and pin the
Caddy image to a reviewed digest in production. The example sets HSTS, so use a
dedicated hostname that will remain HTTPS-only. No access logs containing
authorization headers or bodies are enabled. `/health` is intentionally public
and reveals no database details; readiness, metrics and OpenAPI are protected.

Each completed routed request emits one JSON line on stderr with `event`,
server-generated `request_id`, method, route **template**, status and duration.
The response returns `X-Request-ID`; client IDs are not trusted. Headers, raw
paths/query strings, IDs, payloads, vectors, metadata and tokens are not logged.
Startup prints the listening address on stdout; startup/shutdown failures and
internal/indeterminate-commit events use JSON stderr. Request IDs are unique
within a process/router lifetime, not globally across container restarts.

`/metrics` returns process-local cumulative counters for requests, failed
responses (4xx/5xx), unauthorized requests, indeterminate commits, and HNSW
rebuilds; request duration and rebuild-triggering-search duration totals use
seconds. The latter **includes the triggering query**, not isolated graph build
time. `database_bytes` and `live_records` are gauges. Counters reset on restart.
`hnsw_buffered_ids` and `hnsw_buffer_vector_bytes` expose current graph-buffer
usage (zero when no graph is retained). They count original vectors plus
current live buffered vectors, not payloads or metadata, and are not RSS.
Request timing ends when response headers/body are produced, not after streaming
to the client; canceled middleware requests may not produce a completion log.
The current metrics request is counted but not yet timed. Snapshots are
non-transactional; database gauges take a shared read guard, wait for writes and return 503
when busy or closed. A poisoned owner still exposes failure counters with
`database_healthy: false` and null database gauges, never stale live data.
Current-source observability also exposes authenticated `GET /v1/admin/logs`
(`since` sequence cursor, `level`, `q`, `limit` 1..1000, default 200), returning
`entries`, `next_seq`, `dropped` and `capacity`. The structured log ring is
process-local and non-durable; `--log-buffer` defaults to 2000 (1..100000).
Sanitized access events omit tokens, raw URLs, vectors and payloads.
`GET /v1/admin/metrics/summary` returns uptime, request/search counters,
per-route latency percentiles over recent 1024 samples, database state and ring
state. `/metrics` remains available in JSON or Prometheus format. The dashboard
adds cards/sparklines, a sortable route table, live filtered tail/JSONL export,
slow queries and audit; see [console](CONSOLE.md). These are live diagnostics,
not a persistent monitoring backend. [MCP](MCP.md) exposes embedded tools over
stdio, and [agent skills](AGENT-SKILLS.md) provide portable workflows.

Metrics share the full-access token: give access
only to trusted monitoring and never expose an unauthenticated scrape endpoint.
There are no histogram/percentile claims; aggregate durations are totals.

`tests/rest_e2e.py` checks authentication failures without mutation, metrics,
log redaction, exact/ANN behavior, persistent restarts and copy-out
backup/restore/compaction. `tests/tls_e2e.py` additionally exercises the actual
Caddy proxy with a generated local CA, verifying the certificate chain and
hostname (never disabling certificate checks), authentication and HTTPS CRUD.
Both preserve database volumes and remove only their temporary secret files.
The local-CA test does not prove public DNS/ACME renewal works on your host.

The **VLite release** workflow supports manual validation without publication.
An explicitly pushed `vX.Y.Z` tag matching `Cargo.toml` triggers tested native
ZIP bundles/checksums and a versioned `ghcr.io/mahpate/vlite:vX.Y.Z` image for
Linux amd64/arm64, then a GitHub release with the immutable image digest.
Native artifact names identify the actual Rust host target; they do not promise
every OS/architecture combination. ARM containers run the Dockerfile tests
under QEMU during publication. This is not equivalent to physical ARM testing.
Registry credentials are the workflow's scoped `GITHUB_TOKEN`; private packages
require a read-packages login on deployment hosts. There is no floating `latest`
tag, or automatic production rollout. The repository is licensed under the
MIT License; see [LICENSE](../LICENSE).

For deployment, select a published version/digest and skip local builds:

```text
VLITE_IMAGE=ghcr.io/mahpate/vlite@sha256:YOUR_RELEASE_DIGEST docker compose --profile api up --no-build -d api
```

Set `VLITE_IMAGE` using your shell's environment syntax on Windows. The image
keeps `vlite-smoke` as its default entrypoint; Compose explicitly selects
`vlite-server`. The release gate tests that exact locally built runtime image
without rebuilding it. Multi-platform publication also rebuilds/tests both
target architectures before the manifest is published. A further gate pulls the
published digest and repeats authenticated REST/HTTPS acceptance without
rebuilding before creating the GitHub release. Release archives include
native tools, OpenAPI and deployment examples; verify the adjacent SHA-256 file
before running downloaded binaries. Publication requires a maintainer to push
a version tag; adding the workflow alone does not create a release.

**Upgrade/rollback runbook:** stop the owner and confirm the volume is not in
use; run the old `vlite-maintain inspect` and `backup` to a **new** backup path,
copy the validated backup off-host, and retain the previous image digest and
token configuration. If a format migration is needed, compact to a **new** file,
inspect it, and explicitly switch the server's database path after validation.
Never modify a header or overwrite the only database/backup. Start the new
digest, check authenticated readiness, record count and representative reads/
searches, and keep the original file until acceptance. To roll back, stop the
new owner, restore the pre-upgrade backup to another **new** path and start the
old digest against it. An old binary may not read a newer format. Writes made
after the backup are not automatically rolled back or merged: stop writers and
resolve that data-loss decision explicitly before restoring. Never delete data
volumes with `down -v` as an upgrade step. Transport/auth changes here do not
alter the v3 file format.

## Atomic batch writes

New databases use format v3. A batch is one bounded checksummed commit frame,
not a loop over separately acknowledged writes:

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

All operations are validated before writing anything. They execute in slice
order; the last mutation of a repeated ID wins. A missing delete is a no-op but
still part of a nonempty batch's commit. Empty batches do not write or sync.
Limits are **4,096 operations** and **16 MiB of encoded body bytes**, including
16 bytes of metadata per operation plus put vectors/payloads. Existing dimension,
finite-component and per-record payload rules still apply. Oversized batches
return `BatchTooLarge`; invalid input never commits a valid prefix.

One `sync_all()` acknowledges the entire batch before any live-map change.
Reopen validates the whole frame and all entries before replaying the batch.
An interrupted suffix never exposes a partial batch: open fails closed, and
explicit tail repair discards the entire incomplete frame, subject to the same
potential loss of externally truncated acknowledged data described below.
A failed write or sync poisons the handle and leaves the **whole batch**
indeterminate: it can be absent or fully present after reopening, not partially
applied. There are no begin/commit sessions, cross-process snapshots, retry
tokens, or unbounded transactions. Application allocations can still abort the
process on OOM.

**Existing v1 databases:** reads and single-record writes remain supported,
with their original encoding unchanged. Nonempty batches return
`BatchRequiresUpgrade`; the library never rewrites a v1 header in place. Close
the v1 database and compact it to a **new** path to explicitly upgrade to current v3,
then verify and switch paths. Backup/restore preserve the input format exactly.
Older binaries reject newer unsupported headers; v2-only binaries reject v3.
`Database::format_version()` and maintenance inspection expose the
version.

## Typed metadata and exact filtered search

Format v3 supports a typed map alongside each vector and opaque payload.
`Record::metadata` is a `BTreeMap<String, MetadataValue>` containing string,
signed i64 integer, finite f64 float, or boolean values. No arrays, nested maps,
nulls, field indexes, or general expression language are included.

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

`put_with_metadata` replaces the **entire** vector, payload and map.
Existing `put` and `Mutation::Put` clear any previous metadata, rather than
merging or retaining it. Use `Mutation::PutWithMetadata` in atomic batches to
replace all three together. Every operation is validated before any frame is
written, and duplicate IDs still execute in order. Metadata updates participate
in the HNSW mutation buffer exactly like other commits. `get` returns the persisted map.

Limits: at most **64 fields**, names **1..256 UTF-8 bytes**, string values at most
**4,096 UTF-8 bytes**, and **65,536 encoded metadata bytes** per map (including
field count, names, tags and values). Metadata bytes count toward the existing
16 MiB batch-body limit, in addition to vector/payload and entry overhead.
Non-finite floats are rejected. Names/strings are case-sensitive with no Unicode
normalization. Signed zero float bits survive storage; equality and numeric
ranges treat -0.0 and +0.0 as equal. Integer and float types never coerce, avoiding
precision loss at i64 boundaries.

Filters are an AND conjunction of at most **64** conditions. Equality supports
all four types; ranges accept inclusive min and/or max of one numeric type,
requiring min <= max. Missing fields and mismatched stored types do not match;
an empty filter matches all records. Validation applies even to k=0 and empty
databases. Filtering is a full scan of eligible records **before** top-k
selection, not post-filtering nearest hits. `search_filtered` is always exact;
approximate filtering is a separate, explicit API described below.

For REST, `metadata` is optional on PUT and batch puts (omission means `{}`):

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

Integer values use **canonical decimal i64 strings**, not JSON numbers, so
JavaScript clients can preserve all 64 bits. Float values use JSON numbers.
Reads return the same tagged representation. Nested maps/nulls/unknown types
are rejected. Search accepts `filter`:

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

`filter` is accepted in all three modes (`exact`, `approximate`,
`approximate_ready`) with identical validation; `null`, omission and `[]` all
mean unfiltered. Inner product remains exact-only. See "Filtered approximate
search" for ANN semantics. Malformed filters return 422 `invalid_input` before
readiness, zero-k, empty-database or lazy-build handling.

**Migration:** v1/v2 records open with empty metadata. Existing reads, ordinary
puts/deletes, and v2 batches remain byte-compatible on those files. Nonempty
`put_with_metadata` requires v3; `Mutation::PutWithMetadata` always requires v3,
including an empty map. REST maps empty metadata puts to legacy-compatible
operations. Unsupported metadata writes return `MetadataRequiresUpgrade` /
HTTP 409, without changing any bytes. Stop the server/close the owner, then use
`vlite-maintain compact old.vlite upgraded.vlite` to create a validated v3 copy.
Verify it before explicitly switching the application path. The source is never
rewritten. Backup/restore preserve original versions and bytes. Compaction
preserves typed metadata and writes v3. Older binaries reject the new header
even when the file contains no metadata. `Record` gained a public metadata
field and `Mutation` gained a variant: downstream Rust struct literals and
exhaustive matches must be updated.

## Optional HNSW search

### Metric identity (cosine ANN, v0.1.2+)

The legacy `build_hnsw`, `search_approximate` and `search_approximate_ready`
methods remain squared-L2 APIs, with unchanged `HnswConfig` fields. Current
source adds corresponding `_with_metric` methods taking a final
`DistanceMetric` argument (`build_hnsw_with_metric(config, metric)`).
Only `SquaredL2` and `Cosine` are supported; `InnerProduct` returns
`UnsupportedAnnMetric` even for an empty database or k=0.
Use a v0.1.2+ engine and SDK together for cosine ANN;
v0.1.1 binaries and packages do not include this addition.

Exactly one graph is retained per owner. Its identity includes construction
parameters and metric. Lazy queries synchronously replace a missing/mismatched
graph; ready-only queries return `IndexNotReady` without replacing anything.
Empty-database/k=0 queries validate vectors, metric and ef first but need no
graph. Reopen loses all indexes. `ef_search` uses the existing live-record
count bound, including zero-norm records, rather than scanning eligibility
before validation.

Cosine traverses and prunes using `1 - clamp(dot / (norm_a * norm_b), -1, 1)`,
with f64 dot products/norms. It does not rewrite or normalize stored f32 vectors.
Graph-node norms are cached at build time and stay paired with original graph
vectors during buffered updates; query norms are calculated once per traversal.
Current buffered vectors are scored independently and merged by distance/ID.
Zero queries are rejected even for k=0; zero-norm records are excluded from
graph construction and buffer results. Zero-to-nonzero and nonzero-to-zero
replacements obey the same visibility and overflow rules as L2 updates.
No disk format, ownership or durability contract changes.

REST accepts `{"metric":"cosine"}` at both the build route and `/v1/search`.
Status reports the active `metric` (null without a graph) alongside `ready`;
both readiness and metric must match the intended query. This is a snapshot,
not a reservation. The dashboard shows the active metric and builds for the
selected metric. Inner product is only selectable for exact search.

### Explicit maintenance and no-rebuild queries

Use `Database::search_approximate_ready(&query, k, ef_search, config)` when the
caller must not initiate graph construction. It takes a shared reference and
returns `Error::IndexNotReady` if a nonempty query needs an absent or differently
configured index. It never silently falls back to exact search or changes an
existing graph. Input validation, mutation-buffer visibility, exact candidate
distances and approximate recall rules are identical to lazy search.
Empty-database and k=0 queries still validate but need no graph.

Call `build_hnsw(config)` explicitly in a maintenance window. The REST equivalent
uses the fixed default construction parameters:

```powershell
Invoke-RestMethod "$base/v1/index/hnsw" -Headers $auth -Method Post -ContentType 'application/json' -Body '{}'
Invoke-RestMethod "$base/v1/search" -Headers $auth -Method Post -ContentType 'application/json' -Body '{"vector":[0.9,0.1,0],"k":5,"mode":"approximate_ready","ef_search":100}'
```

The build route accepts `{}` for L2 or `{"metric":"cosine"}`, requires the same token,
and returns 204 when complete. Missing indexes produce HTTP 409 with code
`index_not_ready` in ready mode, as do metric mismatches; choose maintenance or an explicitly different
search mode rather than blindly retrying. Writes can invalidate an index again
after a successful build. Reopen always loses the graph. Exact search remains
available without graph construction. No storage format or database bytes change.

Both explicit builds and successful lazy rebuilds increment
`hnsw_rebuilds_total`. `hnsw_rebuild_and_search_seconds_total` accumulates explicit
build duration plus lazy build-and-triggering-query duration; it is not uniformly
isolated build time. Ready queries and readiness errors do not increment these
counters. Explicit builds use the existing serialized worker: requests may queue
behind maintenance and admitted builds finish even if the client disconnects.
This prevents query-triggered builds, **not queue latency, allocation cost or
background work**. There is still no background builder or latency guarantee.

### Existing lazy mode

`Database::search` stays exact. `search_approximate` explicitly opts into a
deterministic hierarchical navigable small-world graph with bounded
per-layer degree, greedy upper-layer descent and beam search at the base.
It uses the selected f64 distance calculation and sorts discovered candidates
by distance then numeric ID. It may miss nearest neighbors or return fewer
than `min(k,records)` hits if the reachable candidate set is smaller.
There is no universal recall guarantee or automatic fallback to exact search.

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

Construction parameters are `m` (2..64), `ef_construction` (m..4096), and a u64
seed. Search `ef_search` must be 1..100,000 and at least `min(k,live records)`.
Larger parameters usually trade more build time/memory or query work for recall,
not guaranteed monotonic quality on every dataset. Query validation applies to
empty databases and k=0 too; k=0 does not build the index.

The graph is stable between rebuilds. Successful writes track changed IDs in a
bounded buffer: graph traversal retains original vectors for updated/deleted
nodes, excludes those IDs from graph results, scans current buffered vectors
exactly, then merges/deduplicates/ranks candidates. New writes are searchable
immediately; deleted IDs and old vector distances cannot leak into results.
Only original vectors are copied, not payloads or metadata. Unchanged graph
nodes still reference authoritative records.

The buffer allows at most **256 distinct IDs** and **8 MiB of vector bytes**,
counting retained original vectors plus current buffered vectors. Current
vectors remain in the live map; the byte count includes them to bound scan
work. Repeated updates to one ID retain only its first graph vector and final
live value. Crossing either limit drops the graph and buffer; the next nonempty
approximate query rebuilds synchronously. A batch may cross the limit during
application; the whole durable batch remains authoritative. These are not
total process-memory limits.

Invalid mutations, empty batches and missing single deletes do not change the
buffer. A failed commit poisons the owner and prevents searches. Reopen starts
without an index. Changing construction parameters rebuilds it; explicit
`build_hnsw` always rebuilds and clears the buffer. Internal candidate width
may be `ef_search + buffered IDs` (at most 256 extra), to compensate for stale
graph candidates. This is not a recall/full-k guarantee: deleted nodes remain
traversal bridges, and data distribution still affects discovery. Exact search
remains the correctness baseline.

### Filtered approximate search

```rust
db.search_approximate_filtered_with_metric(&query, k, ef_search, config, &conditions, metric)?;
db.search_approximate_ready_filtered_with_metric(&query, k, ef_search, config, &conditions, metric)?;
```

Conditions come before the final metric, matching
`search_filtered_with_metric(query, k, conditions, metric)`. The conditions are
the same typed AND predicates as exact search and are validated first (after
the query vector), even for k=0, empty databases, a missing index or a lazy
build. Only squared L2 and cosine are supported. Empty conditions take the
unchanged unfiltered path; existing `search_approximate*` methods delegate with
empty conditions. The lazy method rebuilds exactly like
`search_approximate_with_metric`; the ready method never builds and returns
`IndexNotReady` under the same conditions as unfiltered ready search.

Filtering participates in base-layer traversal rather than post-filtering an
unfiltered top-k. Every reachable graph node may be expanded as a navigation
bridge, including nonmatching nodes and nodes with buffered updates/deletes
(which navigate using their pre-update graph vectors). Only unbuffered graph
nodes whose **current** metadata matches enter the result heap; eligibility is
never taken from stale state. Buffered current records that match are scored
separately against the query and merged. Traversal can stop early only once
`ef_search` eligible results are held and the nearest unexpanded node is
farther than the worst of them.

`ef_search` is the eligible-candidate retention width, **not** a visit or time
cap. Selective or no-match filters may visit every reachable node (comparable
to an exact scan of the graph). Up to k eligible records are returned with
exact distances, ordered by distance then ID. Recall and filling k are not
guaranteed (for example, matches unreachable from the entry point are missed),
and there is no hidden exact fallback: use exact mode when completeness matters.
REST capability `features.filtered_ann` is `true` on servers that accept
filters in approximate modes; older servers reject them with 422.

There is no persistent graph sidecar. A first lazy approximate query after
reopen/overflow blocks other operations while rebuilding; explicit background
rebuilds and opt-in server maintenance can prepare a ready index instead.
The buffer avoids rebuilding after each small write but is not unlimited
incremental HNSW maintenance. Rebuilding is not free and small datasets may search
faster with the exact scan. Index creation/search allocate RAM without a
recoverable OOM guarantee. The ef bound is not a total visited-node/RAM quota.
No index or sidecar is written; v1/v2/v3 files and maintenance operations remain
unchanged. Fixed-workload tests require recall@10 >=0.95 on 1,000 seeded
16-dimensional vectors and verify deterministic replay, returned distances,
ordering and mutation consistency; this is not a promise for arbitrary data.
The mixed-write workload separately requires recall@10 >=0.95 after 50 deletes,
50 updates and 100 inserts, checks exact returned distances and ID uniqueness,
and verifies immediate buffer visibility and reopen recovery.

## Proactive HNSW maintenance (Rust)

Current-source Rust callers can opt into an explicit owner-thread tick:
`maintain_hnsw(config, metric, policy)`. `HnswMaintenancePolicy` defaults to
starting a replacement at **128 buffered distinct IDs or 4 MiB of buffered
original-plus-current vector bytes**. Either threshold triggers; configure
positive thresholds up to the existing 256-ID / 8-MiB limits. The policy also
contains `HnswRebuildLimits` (same defaults/budgets as manual starts below).
Invalid thresholds return `InvalidHnswMaintenancePolicy`; invalid snapshot
limits return `InvalidHnswRebuildLimits`. No buffer limit is enlarged.

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

Call before mutation/query batches, not from a timer that concurrently touches
the owner. A tick polls at most one matching job and may publish its result.
If no job remains, it starts a snapshot when the matching graph is absent or
either active high-water threshold is reached. The result contains the poll
`outcome`, `started`, and latest `status`. A published graph can already be
at high water and cause a new job in the same tick. Overflow remains visible
in `outcome` even if the tick starts a fresh retry. A terminal `Cancelled` or
`Superseded` suppresses restarting for that tick; a subsequent explicit tick
can start again. A pending job for another config/metric returns
`HnswRebuildBusy` without polling, publishing, cancelling or replacing it.
The method can return an error after publication if starting the next snapshot
exceeds its budget or thread creation fails; completed publication is not undone.

`hnsw_maintenance_status(config, metric, policy)` validates inputs and observes
without starting, polling, joining or publishing. `ready` means a matching graph
exists, not that a worker has finished. This differs from empty-database search,
which can return empty results without a graph. `active_buffer` contains
buffered and remaining IDs/vector bytes. Each pending worker exposes identity,
snapshot/processed record counts, `worker_finished`, selected terminal reason,
and its own bounded catch-up capacity. The counters include source records
skipped for cosine zero norms. They measure processed records, **not time,
percent of CPU work or an ETA**. Counter and thread-finished reads are separate
observations; an empty snapshot has zero records. Finished does not imply
success or publication; poll/tick reports cancellation or worker failure.

| Pressure | Meaning (first matching condition wins) |
|---|---|
| `CatchUpOverflow` | Pending worker has overflowed and is being discarded. |
| `Unavailable` | No active matching graph; ready-only search may reject. |
| `CapacityExhausted` | Active or pending catch-up buffer has zero remaining IDs or vector bytes. The active graph may still be ready at the exact limit. |
| `RebuildRecommended` | Active graph reaches either configured high-water threshold. |
| `Healthy` | None of the above; not a future-readiness promise. |

Capacity is **not a number of writes left**: repeated IDs, vector sizes, deletes
and first-mutation originals affect accounting. A large batch can cross the
remaining capacity in a single commit. Thresholds are early triggers, not
reservations, predictive deadlines or automatic admission control. A running
replacement can catch up too slowly; publishing can retain a large catch-up
buffer. Inspect `ready`, pressure and terminal outcome together. The application
must choose to throttle mutations, reject ANN requests, or explicitly use exact
search. Durable writes remain accepted regardless of index overload. There is
no hidden scheduler or automatic fallback in this Rust tick API. The server's
separately configured automatic maintenance is disabled by default; existing
synchronous and ready-only searches retain their behavior.

## Background HNSW rebuilds (Rust)

Current-source addition after v0.1.2. The Rust owner has an **opt-in** lifecycle:
`start_hnsw_rebuild(config, metric, limits)`, `poll_hnsw_rebuild()` and
`cancel_hnsw_rebuild()`. REST, C ABI, Python/Node wrappers and console controls
also expose the lifecycle. Optional server scheduling is disabled by default;
indexes remain derived and memory-only. Existing synchronous build, lazy
search and ready-only search behavior remains unchanged.
See the runnable [`background_hnsw` example](../examples/background_hnsw.rs):

```powershell
cargo run --locked --example background_hnsw
```

`start_hnsw_rebuild` validates health, HNSW configuration, L2/cosine metric and
limits before creating the worker. It copies **only vectors and IDs** from the
committed live map (no payloads/metadata). Copying is synchronous on the owner;
graph construction runs on one named owned thread, without the file or database
lock. Caller operations on `Database` remain serialized; this does not add
concurrent reads/writes to the handle. Exactly one job may be pending, including
a finished-but-unpolled or cancelling job. A second start returns
`HnswRebuildBusy`. Thread creation failure returns `Io` and leaves the current
index intact.

`HnswRebuildLimits` defaults to **100,000 snapshot records / 128 MiB snapshot
vector bytes**. Caller limits must be positive, at most 1,000,000 records /
256 MiB vector bytes; exceeding the configured budget returns
`HnswRebuildTooLarge` before cloning. These bound vector bytes and graph
cardinality, **not total RSS or recoverable allocation failure**: BTreeMap/node
overhead, graph links, construction scratch, current records, existing graph
and mutation-original copies additionally consume memory. Old and replacement
graphs coexist. Construction degree/ef retain their existing limits.

After a durable commit, before replacing authoritative records, the owner
tracks each changed ID against its first snapshot-era vector. Repeated mutations
retain the original and final current byte count, not an unbounded event log.
The catch-up buffer uses the existing HNSW **256-distinct-ID / 8-MiB
original-plus-current-vector-byte** limits. It handles deletes, inserts,
delete/reinsert in the same batch, metadata puts and cosine zero transitions.
If either limit overflows, the job is cancelled and eventually reports
`CatchUpOverflow`; writes stay committed and **no stale graph is published**.
The old graph independently obeys its original buffer limits and may also become
unavailable. There is no implicit fallback to exact or synchronous construction.

Call `poll_hnsw_rebuild` on the owner between requests. It returns `Running` or
`Cancelling` without waiting for construction. Once the worker has terminated,
poll joins it, transfers the bounded catch-up buffer and replaces the current
index in a single serialized owner operation. The buffer's original vectors
remain paired with snapshot graph norms. Queries subsequently score current
live vectors; deleted IDs cannot reappear. The worker never changes owner state
directly. Publication and freeing the old graph take owner-thread work and are
not a hard real-time bound.

| Poll outcome | Meaning |
|---|---|
| `Idle` | No pending job. |
| `Running` / `Cancelling` | Worker still owns resources; poll again later. |
| `Published` | New graph and catch-up buffer are active. |
| `Cancelled` | Caller cancellation discarded the replacement. |
| `Superseded` | An explicit synchronous/lazy build selected another graph; worker cannot overwrite it. |
| `CatchUpOverflow` | Bounded mutation catch-up was exceeded; snapshot discarded. Retry explicitly with a new snapshot or throttle mutations. |
| `Err(HnswRebuildFailed)` | Worker panicked/failed; active index is unchanged and storage remains usable. |

Terminal polls consume the job; the next poll is `Idle`. A `Published` index
may already have buffered mutations and need replacement soon. It is not a
guarantee of future readiness or ANN recall. Invalid operations do not enter
catch-up. A poisoned database rejects start/poll/search and never publishes
a snapshot, retaining the existing indeterminate-commit safety contract.

Use `search_approximate_ready_with_metric` for a latency-sensitive caller:
it returns `IndexNotReady` when the old graph has overflowed and no replacement
has been published. **Do not** call lazy search as a hidden retry: it still
builds synchronously, and supersedes any pending replacement. The application
must choose whether to reject, retry later or explicitly request exact search.
Schedule/poll frequently enough and control write rate to meet availability
requirements; arbitrary sustained mutation rates cannot be promised here.

Cancellation is cooperative, checked between node insertions and before
returning the completed graph. It is not an immediate thread kill.
`cancel_hnsw_rebuild` returns false for no job, otherwise requests cancellation;
it does not replace the active graph. A previously selected overflow/superseded
reason is preserved. `close()` cancels and joins a pending worker before
unlocking and reports worker failure; `Drop` also cancels/joins, but cannot
report errors. Shutdown may wait for an in-progress insertion. Reopen recovers
committed data, not worker/index state. No sidecar or storage-format change.

## Reproducible benchmark

`vlite-bench` generates deterministic fixed-point vectors using SplitMix64,
ingests them with atomic batches and sync-before-success, closes and reopens
the database, warms up one query, and times exact squared-L2 queries. Every
query is checked against an independent brute-force calculation on the original
generated vectors **outside** the measured query time. Incorrect results make
the benchmark fail rather than print successful metrics.

Optional `--vectors <file> --query-vectors <file>` loads bounded finite f32
text rows instead of generating inputs. `--mixed-writes N --ann ...` commits
N existing-ID replacements before every approximate query, verifies against
the current dataset and retains rebuild pauses in query samples.
`--mixed-deletes N` adds deletes, advancing IDs modulo the initial record count;
the independent oracle excludes deleted IDs and tracks later reinsertion.
External mixed writes require `--update-vectors <file>` with exactly
`queries * mixed-writes` rows (synthetic replacements are not implicit).
Per-query live counts/recall, one-based rebuild query numbers, final live count
and decoded update checksums are reported alongside raw latency samples.
These flags are current-source additions after v0.1.2, not new SDK/engine APIs.
Place these
flags before the filtering/ANN flags. See the
[native campaign guide](../benchmarks/README.md#native-profiling-real-vectors-and-mixed-updates)
and [licensed real-data campaign](../benchmarks/README.md#real-glove-embeddings-and-mixed-deletions)
for format, bounds, repeated trials, recall targets and measured memory scope.

For background lifecycle measurements, prefix `--rebuild-mode background` and
optionally `--query-interval-ms 250` (0..1000). The native runner starts with a
synchronous graph in both modes, polls before each mixed mutation/query, starts
a new worker after every terminal/idle poll, and uses ready-only queries.
Rejections are recorded, never turned into fast successful queries or hidden
fallbacks. `--rebuild-mode lazy` preserves the default synchronous path.
Pacing is also supported for lazy runs; it sleeps before each request, not at
a fixed external arrival rate. Paced/background workloads are bounded to
200 queries, 100,000 records and 128 MiB snapshot vector bytes.
The [paired campaign guide](../benchmarks/README.md#background-rebuild-latency-and-availability)
describes publication/overflow counts, served versus delivered recall, and
request timing that includes snapshot/publication/commit work.
`--rebuild-mode maintenance` uses `maintain_hnsw` with default high-water
thresholds instead of eagerly starting after every terminal/idle poll. Its
`background.policy.capacity_samples` expose post-write/pre-query readiness,
pressure, active/catch-up capacity and processed-record progress. Full-request
timing also includes this diagnostic sampling/formatting; no synchronous
fallback is allowed.

Prefix `--filter-every N` to benchmark exact metadata filtering, selecting IDs
divisible by positive N (approximately 1/N of the dataset). It writes boolean
metadata for every vector, filters before ranking, and verifies eligible hits
against the full-sort oracle. Output adds `filter_every`, `eligible_records`,
and `returned_per_query`; k can exceed the eligible count. This flag cannot
be combined with `--ann`. Examples varying selectivity and k:

```text
cargo run --release --bin vlite-bench -- --filter-every 10 10000 32 100 100 10 42
cargo run --release --bin vlite-bench -- --filter-every 100 10000 32 100 100 1000 42
```

Exact search retains at most `min(k, live count)` candidates. Allocation-bound
tests measure requested query allocation bytes independently of dataset setup;
whole-process peak memory still includes the live database, benchmark dataset,
and full-sort oracle. This is not a bounded-memory storage engine, nor a promise
that heap selection is faster when k approaches the eligible count.

One Windows x86_64 release-mode comparison (32 dimensions, 100 queries, seed42,
sequential runs on the same host) measured p50 1.8302 ms for the previous
full-sort binary versus 0.7373 ms for the bounded heap at 10,000 records/k10.
At 1,000 records/k1,000 the heap was slower: 0.1339 ms versus 0.1143 ms.
All results matched the independent oracle. These are workload-specific
observations, not speed guarantees; repeat on your hardware and include
filtered workloads and large k when evaluating the trade-off.

```text
cargo run --release --bin vlite-bench -- 1000 32 100 100 10 42
docker compose run --build --rm --entrypoint vlite-bench vlite 1000 32 100 100 10 42
```

Arguments are `records dimensions queries batch_size k seed`, in that order,
with the shown defaults. `batch_size=1` still uses the batch frame API, allowing
a controlled sync-per-record versus larger-batch comparison. The seed and
dataset checksum identify identical inputs across platforms. A private
temporary database is created under the current directory (Docker: `/data`),
or `VLITE_BENCH_DIR` when set, and is removed on success or ordinary errors.
There is no overwrite or destructive cleanup of existing databases. Abrupt exit
can leave temporary benchmark data; cleanup is then an explicit user decision.

Output is one JSON object with schema version 1, engine/file-format versions, OS/architecture,
debug/release profile, all inputs, hexadecimal dataset checksum, ingestion seconds and
vectors/second, reopen milliseconds, nearest-rank query p50/p95/p99 milliseconds,
raw query samples, initial/final logical database bytes, decoded dataset/query
checksums, source/query vector-data byte counts and recall@k. With the
current exact engine, recall@k must be 1.0; this is a correctness check, **not**
an ANN performance claim. Distances and tied IDs must also match the oracle.
`process_peak_rss_bytes` is explicitly `null`: vector-data counts are not
process-memory measurements. Use an external process-memory profiler when
comparing actual memory consumption.

To compare HNSW against the same independently checked exact results:

```text
cargo run --release --bin vlite-bench -- --ann 16 100 100 1000 32 100 100 10 42
docker compose run --build --rm --entrypoint vlite-bench vlite --ann 16 100 100 1000 32 100 100 10 42
```

The optional prefix is `--ann m ef_construction ef_search`; construction seed
is fixed at 42 and reported separately from the dataset seed. The remaining six
positional arguments and existing exact-only output remain compatible. The
`hnsw` JSON object adds construction/search parameters, index-build milliseconds,
warm approximate query p50/p95 and measured ID-set recall@k against exact top-k
(ties follow numeric ID). Missing candidates reduce recall. Each returned
candidate's distance, ID uniqueness and ordering are checked separately.
The build runs after the exact baseline; approximate query timing excludes
build, warmup and correctness comparisons. No benchmark forces a recall target
or reports guessed speedups.

To attach **actual whole-process peak resident memory**, run the standard-library
Python wrapper with the release executable path:

```powershell
python tests\measure_benchmark.py target\release\vlite-bench.exe --ann 16 100 100 1000 32 100 100 10 42
```

On Linux/macOS use `python3 tests/measure_benchmark.py
target/release/vlite-bench --ann 16 100 100 1000 32 100 100 10 42`.
The wrapper replaces the null RSS field with the child's measured peak:
Windows `GetProcessMemoryInfo.PeakWorkingSetSize`, Linux/macOS `getrusage`
maximum RSS (converted to bytes). It reports source and scope. This includes
the source dataset, database, exact ground truth, graph, and both query paths;
it is **not marginal index memory**, and OS definitions differ. Run a fresh
wrapper per workload. No Python/runtime profiler is added to the production
container. Native CI exercises the wrapper on all three OSes.

Ingestion timing includes operation construction, encoding, writes, sync and
live-map updates, but excludes dataset generation, header creation and reopen.
Reopen timing includes file open, locking and full replay. Query timings exclude
the oracle and warmup, include result allocation/sorting, and are sequential
single-process measurements. Reopen is a **warm-cache** measurement, not a
cold-disk claim. Filesystem/device, CPU, compiler, background load, dataset,
batch size and build profile affect results; record those alongside outputs.
Do not compare this synthetic workload directly with another project's
published benchmark or infer physical power-loss safety from throughput.

Bounds reject invalid workloads before creating files: at most 1,000,000
records, 10,000 queries, 256 MiB of input vector data and 500 million query
distance components, plus the library's dimension and batch limits. Query k
must be between 1 and the record count. These bounds are not a peak-RAM or
execution-time guarantee.

## Storage maintenance

`vlite-maintain` provides offline inspection, backup, restore-to-new-path and
copy-out compaction. Stop every database owner before running it; every operation
holds the source's exclusive OS lock and returns `Locked` rather than waiting.
The source is opened read-only. Inspection validates the entire file; it is not
a quick header-only check.

```text
cargo run --bin vlite-maintain -- inspect vectors.vlite
cargo run --bin vlite-maintain -- backup vectors.vlite backup.vlite
cargo run --bin vlite-maintain -- restore backup.vlite restored.vlite
cargo run --bin vlite-maintain -- compact vectors.vlite compact.vlite
```

Library equivalents are `vlite::maintenance::{inspect, backup, restore, compact}`.
Additional `upgrade`, `verify` and `drill` operations support opt-in v4 workflows;
see [collections and advanced retrieval](CAPABILITIES.md).
Inspection returns format version, dimensions, live records, validated commit-frame count, file
bytes, valid-prefix bytes and incomplete-tail bytes. The CLI prints these as
`key=value` fields. Corruption returns an error; an incomplete suffix is reported
by the library and makes CLI inspection exit nonzero. No inspection repairs or
changes data.

Backup and restore produce byte-identical, fully validated v1, v2 or v3 files. Restore
**never overwrites an existing database**. Both reject corruption and incomplete
tails. Compaction writes only current live records, removes tombstones and old
values, writes format v3, and resets the output log sequence to start at one. It preserves IDs,
dimensions, payloads and vector component bits, including negative zero. Exact
search behavior is unchanged. An empty live set produces a 32-byte file that can
accept new writes. Inspection's commit count is a file-local frame count, not an
unchangeable lifetime mutation counter; a batch counts as one commit frame.

The preceding format-v3 compaction and byte-identical restore descriptions
apply to legacy/default files. For a v4 source, compaction retains v4,
collections and settings, and restore/compaction assign a new file identity.
Revision numbers are file-local, so callers must reacquire conditional tokens
after copy-out maintenance.

Output is first written to a private temporary file in the destination directory,
fully validated and synced, then published using no-clobber semantics. Existing
files, directories, and aliases to the source are never replaced. A normal error
before publication removes staging data; abrupt process termination can leave
an orphan temporary file. No automatic cleanup scans or repair run at startup.
Only the final destination name denotes a successfully published output.

Unix additionally syncs the destination directory after publication. If this
fails, `PublicationIndeterminate` identifies the final path: a complete file
already exists, but directory-entry durability is uncertain. Inspect it before
retrying rather than deleting or overwriting it. Windows output contents are
synced before publication, but directory-entry persistence across physical power
loss is not guaranteed. Filesystems must support the staging helper's
no-clobber publication; unsupported operations fail explicitly. The usual
hardware, sync, and cooperative-lock limitations below still apply.

Compaction is intentionally **copy-out, not in-place**. To reclaim space safely:
stop all owners; compact to a new path; inspect it and verify application data;
change your application's configured database path; retain the original until
you have verified the new database and backups. Delete the old file only when you
explicitly decide it is no longer needed. Do not rename over an open database.
Compaction temporarily needs space for both files and RAM for the live record
map; it does not immediately shrink the source.

The Docker runtime contains smoke, maintenance and benchmark executables. After creating the smoke fixture,
maintenance can be tested across containers using the same volume:

```text
docker compose run --build --rm vlite
docker compose run --rm --entrypoint vlite-maintain vlite inspect /data/smoke.vlite
docker compose run --rm --entrypoint vlite-maintain vlite backup /data/smoke.vlite /data/backup.vlite
docker compose run --rm --entrypoint vlite-maintain vlite restore /data/backup.vlite /data/restored.vlite
docker compose run --rm --entrypoint vlite-maintain vlite compact /data/restored.vlite /data/compact.vlite
docker compose run --rm vlite verify /data/compact.vlite
```

Rerunning copy commands with the same destinations fails intentionally. Use new
names or a separate Compose project for a fresh run; do not delete real backups
merely to rerun examples. Staged outputs use restrictive `tempfile` permissions
(owner-only on Unix); share backups only through explicitly managed OS access.
Backups are not encrypted unless the source is an encrypted v5 database (then
they are byte-identical encrypted copies) and inherit the same sensitive payloads as the source.

## One file and ownership

Each database is exactly one self-contained file. This version creates **no WAL,
journal, index, or lock sidecars**. Maintenance briefly creates a private staging
file in the destination directory before publication. The append log is the canonical store;
the complete live-vector map is rebuilt in RAM on every open. Future versions
may introduce explicitly documented transient sidecars, but none are required
to move a closed database of either supported version.

`Database::open` / `open_encrypted` hold an exclusive, nonblocking OS file lock
for the handle's lifetime. `Database::open_read_only` /
`open_read_only_encrypted(path, &key)` instead open the file read-only with a
shared OS lock. Any number of readers may coexist across processes, but no
reader coexists with a writer. Conflicting opens and repairs return
`Error::Locked` immediately. `is_read_only()` identifies a read-only handle.
Each handle replays committed state at open time; close and reopen to see later
commits. It is not a live tail or MVCC reader alongside a writer. Incomplete tails
are rejected, never silently repaired. Mutations return `Error::ReadOnly` without
writing or poisoning the handle. Building an in-memory HNSW index is allowed;
the datafile is never written by a read-only handle.

Rust `&self` reads can run concurrently; mutations require exclusive access.
The REST/gRPC server shares one exclusively file-locked database through an
`Arc<RwLock<Option<Database>>>`: read routes use shared guards on blocking
workers, writes use an exclusive guard. A stale-index ANN search retries under
the write guard to rebuild. Explicit background builds construct from a snapshot
and catch up/publish exclusively. Python `open(path, read_only=True)`, Node
`open(path, {readOnly: true})` and C ABI Open `"read_only": true` expose shared
reader mode. The C ABI serializes calls within a process: use separate Python
processes for parallel embedded requests.

Native exact scans use Rayon with at least 4,096 candidate rows and
rows × dimensions ≥ 524,288, when the Rayon pool has multiple workers. The
merged results retain distance-then-ID ordering; wasm32 scans remain serial.
`close()` consumes the handle; dropping it releases its lock with no commit-on-drop.

Use local filesystems that honor OS file locks and flush requests. Do not use NFS,
SMB, cloud-synced directories, multiple hard-link/symlink aliases, or concurrent
external file editors. Do not rename, replace, copy, truncate, or otherwise modify
an open database. For backup or transfer, close every handle first and copy the
single file. Locks are cooperative on some platforms and do not prevent hostile
or noncooperating software from modifying the file.

## Commits, recovery, and durability

Each `put`, successful `delete`, or nonempty `write_batch` appends one checksummed frame with a commit
footer, then calls `File::sync_all()`. Only after sync succeeds does the in-memory
state change and the operation return success. There is no relaxed durability
mode. A subsequent open validates all headers, record lengths, sequence numbers,
checksums, and commit footers before exposing any vectors.

| Situation | Behavior |
|---|---|
| Complete valid frames | Replay in sequence; newest record or tombstone wins. |
| Interrupted final append | `open` returns `IncompleteTail`, without modifying the file. |
| Full frame with invalid checksum/footer, invalid lengths, bad sequence, or invalid header | `Corrupt`; neither open nor tail repair discards it. |
| Unsupported format version with a valid header checksum | `UnsupportedVersion`; no automatic migration. |
| Commit write or sync fails | `CommitIndeterminate`; reads/search/writes on that handle then return `Poisoned`. Close and inspect/reopen. |
| Repair truncation or sync fails | `RepairIndeterminate`; inspect/reopen before retrying. |
| Creation fails after creating the path | Error; a partial file may remain and is never silently overwritten. |

A failed operation is **not** evidence that the mutation is absent. A complete
frame may have reached storage before an error or process exit; reopening can
therefore reveal an unacknowledged operation. Reconcile by ID after reopening;
batch retry is also indeterminate and there are no retry tokens.

An incomplete suffix is not silently dropped. After backing up the **closed**
file, an application can explicitly authorize destructive tail repair:

```rust,no_run
let report = vlite::Database::discard_incomplete_tail("vectors.vlite")?;
println!("discarded {} bytes", report.discarded_bytes);
let db = vlite::Database::open("vectors.vlite")?;
# Ok::<(), vlite::Error>(())
```

Repair validates the entire complete prefix and truncates only a structurally
incomplete final frame. It returns the discarded byte count and retained commit
count; a clean file is a no-op. It never skips a complete bad frame or searches
past corruption.

**Important limits:** truncated acknowledged data can look exactly like an
interrupted unacknowledged write. Explicit repair accepts potential loss of that
last truncated commit. Removing entire frames from the end of a file is
undetectable without an external trusted history. Checksums do not authenticate
data and cannot detect every possible storage fault. There is no automatic
fallback to an older snapshot that silently discards a known committed frame.

Durability depends on the OS, filesystem, device, and controller actually honoring
sync. The library does not guarantee against hardware lies, media failure, power
loss on unsupported storage, or external file modification. `create` syncs file
contents but does **not** portably sync its parent directory: persistence of a
new filename across machine power failure requires platform-specific directory
durability handling by the caller. Tests simulate interrupted writes and process
exit, not physical power loss.

## Opt-in file format v4

Format v4 is explicitly selected, never the implicit result of opening an older
file. Older binaries refuse it. Legacy v1-v3 parsing/writing remains unchanged.

The 32-byte header keeps the magic, little-endian version/dimensions and CRC-32.
V4 bytes 16..24 contain a nonzero random file lineage, bytes 24..26 contain the
collection allocation floor and bytes 26..28 remain zero. Create records rebuild
the high-water mark; IDs are never reused. Copy-out maintenance preserves the
floor and assigns a new lineage.

The existing 36-byte frame prefix and 16-byte commit footer remain checksummed.
V4 record/batch frames use prefix bytes 5..7 for a little-endian collection ID,
with byte 7 reserved zero. Operations 5/6/7 create, drop and update collection
settings. Each live collection has independent dimensions and ID namespace.
Settings declarations persist, but derived metadata/text/HNSW indexes do not.

V4 atomic batches can end with an import checkpoint entry (op 8). Ordinary
checkpoints have a 52-byte body containing chunk number, per-chunk row counts,
flags and a full 32-byte SHA-256 identity. Open-job snapshots used by compaction
have a 60-byte body adding high count bits; cumulative counts are preserved
without truncation. A marker's length must match its flags. Invalid histories,
collection reuse, counter overflow and malformed fields fail as corruption;
valid checksums do not bypass semantic validation.

`tests/format_v4.rs` includes independent encodings, valid-checksum malformed
records, allocation-floor and full-width checkpoint tests. The fixed v4 fixture
joins the older fixtures below; do not regenerate fixtures to conceal a format
incompatibility. Pre-release development v4 files from before these invariants
were finalized are not a supported migration source.

## File formats v1, v2 and v3

All integers and IEEE-754 `f32` components use little-endian encoding, never native
Rust struct layouts. Checksums are IEEE CRC-32 (reflected polynomial `0xEDB88320`,
initial/final XOR `0xFFFFFFFF`, check value `0xCBF43926` for `123456789`).

The 32-byte database header:

| Offset | Size | Value |
|---|---|---|
| 0 | 8 | `VLITEDB` followed by NUL |
| 8 | 4 | Format version, 1, 2 or 3; new files use 3 |
| 12 | 4 | Dimensions |
| 16 | 12 | Reserved, zero |
| 28 | 4 | CRC-32 of bytes 0 through 27 |

Every single-record mutation consists of a 36-byte record header, body, and 16-byte commit
footer:

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

Put component counts must equal database dimensions; deletes have zero components
and zero payload bytes. Length limits and header CRCs are checked before allocating
record bodies. Unknown operations and nonzero reserved fields are rejected.
Replaying a tombstone removes that ID. Sequence exhaustion rejects the next write.
The immutable header is never updated after creation.

Versions 2 and 3 additionally support operation **3=batch** in the same outer frame
layout. Outer ID (offset 16) must be zero, offset 24 is the operation count and
offset 28 is the **total encoded body byte length**, not payload bytes. The
header checksum, `VCMT` footer, whole-frame checksum and repeated sequence work
as above. One batch advances the sequence by one.

The body is exactly the declared number of consecutive entries:

| Entry offset | Size | Value |
|---|---|---|
| 0 | 1 | Operation: 1=put, 2=delete; nested batches forbidden |
| 1 | 3 | Reserved, zero |
| 4 | 8 | ID |
| 12 | 4 | Payload length; zero for delete |
| 16 | variable | Put: database-dimension count of f32 values then payload; delete: empty |

Both batch count and encoded byte limits are checked before allocating the
outer body. Entries must fit entirely, contain valid finite vectors and satisfy
payload limits; trailing extra bytes are rejected. All entry checks happen
before applying any operation. Version 1 readers in this release reject batch
records inside v1 files as corruption rather than interpreting unknown data.

Version 3 additionally supports **4=put with metadata**, both standalone and
inside a batch. Standalone op4 has ID at prefix offset 16, dimensions at 24, and
**total extended-body byte length** at 28. For an op4 batch entry, its offset 12
is the total extended-body length, not plain payload length. An extended body is:
dimension-count little-endian f32 components, u32 payload-byte length, payload,
then the typed metadata encoding below. It must contain all bytes exactly.
Empty-metadata standalone puts may use legacy op1; explicit
`Mutation::PutWithMetadata` uses op4 even for an empty map.

Metadata starts with u32 field count, followed by fields in strictly ascending
UTF-8 name-byte order. Each field has a u16 name-byte length, UTF-8 name, u8 tag,
then value bytes: tag1 string = u32 byte length plus UTF-8, tag2 integer = i64 LE,
tag3 float = f64 LE, tag4 boolean = one byte 0 or 1. Duplicate/unsorted names,
invalid UTF-8, non-finite floats, unknown tags, trailing bytes and limit violations
are corruption even if checksums are valid. Op4 body lengths are bounded before
allocation. Op4 in a v1/v2 file (including v2 batch entries) is corruption; new
meanings never silently reuse an old format version.

Fixed fixtures `tests/fixtures/v1.hex`, `v2.hex` and `v3.hex` were encoded
independently with Python `struct` and `zlib.crc32`, not generated by the Rust
writer during tests. Tests pin complete legacy/current file bytes, replay and
single-write compatibility, current batch encoding and explicit copy-out
upgrade. The fixtures are format contracts: do not regenerate them to conceal
an incompatible writer change. New incompatible frame meanings require a new
format version and explicit migration; reserved bytes and operation numbers
must never be silently repurposed under an existing version.

## Capacity and platform status

Space grows with **all historical mutations**, not just live records; deletes do
not reclaim disk space until you switch to a compacted copy and retire the old
file. Open time is linear in log size, memory is proportional
to live vectors/payloads/metadata. Exact search scans n metadata maps, computes
distances for e eligible records, and uses O(e*d + e*log(1+L) + L*log(1+L))
distance/selection time plus metadata evaluation, where L=min(k,n).
Candidate/result memory is O(L), not O(e); the retained results alone are sorted.
There is no total file or RAM quota and allocation exhaustion is not a
recoverable database error. Plan capacity accordingly.
The optional HNSW graph adds O(n*m) adjacency storage plus construction/search
scratch allocations; vectors remain in the authoritative live map. Its
rebuild and candidate-search costs depend on configuration and data distribution,
not a fixed latency bound.

The source uses portable Rust I/O, standard OS locks, and explicit binary
encoding, targeting Windows, Linux, and macOS. Windows x86_64 (MSVC) has been
tested with Rust 1.99.1 and the declared Rust 1.89.0 minimum. The Linux amd64
Docker image is tested using Docker Engine in Ubuntu 24.04 WSL, including
non-root volume persistence across separate containers and the full test suite.
The batch release's full test suite passed in native Windows,
Linux and macOS GitHub Actions jobs on minimum and stable Rust, along with lint
and Linux Docker end-to-end checks. The same CI matrix runs the optional REST
feature and native TCP tests; Docker additionally checks the HTTP container
lifecycle and SIGTERM shutdown. Docker on macOS and Linux arm64 images have not been tested.
Container testing does not verify physical power-loss durability. No on-disk compatibility
with SQLite, redb, or Qdrant is claimed; their storage/ownership/index-separation
designs are conceptual references only.
