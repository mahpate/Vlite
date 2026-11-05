# Optional gRPC transport

VLite's versioned protobuf services are `vlite.v1.VLite` and `vlite.v1.Kv`.
They provide a bounded data transport over the same exclusively owned database
as REST, not another storage engine. Build the server with the optional `grpc`
Cargo feature:

```text
cargo build --locked --features grpc --bin vlite-server
```

gRPC is disabled unless `--grpc-bind` is explicitly supplied. Bind to loopback
by default. Remote listeners require `--allow-remote` and bearer
authentication. Do not expose bearer credentials over an untrusted plaintext
network: either terminate TLS at a trusted proxy or serve native TLS:

```text
vlite-server data.vlite --token-file admin.token --allow-remote \
  --grpc-bind 0.0.0.0:50051 \
  --grpc-tls-cert server-chain.pem --grpc-tls-key server-key.pem
```

The certificate chain (leaf first) and private key are PEM files. Both flags are
required together. They are read and validated, including that the key matches
the leaf, before the database is opened. A TLS listener reports
`LISTENING grpcs://<address>`, and clients connect with `https://` and must trust
the issuing CA. Native TLS uses rustls with TLS 1.2 or later; it does not
request client certificates unless `--grpc-tls-client-ca client-ca.pem` is set.
That flag requires a certificate signed by the configured client CA on every
connection; bearer authentication still applies. REST supports native HTTPS
using `--tls-cert`/`--tls-key`. Bootstrap admins can call
`POST /v1/admin/tls/reload` to transactionally reload both listeners and client
CA roots without restarting; invalid files leave all old configurations active.
Existing TLS sessions are unchanged. See [SECURITY.md](SECURITY.md) for tenant
namespace isolation, persisted credential rate/vector limits, and optional OTLP
tracing. Rate/quota errors use `RESOURCE_EXHAUSTED` with `retry-after: 1`.
API keys bound to a suspended tenant get `PERMISSION_DENIED` with
`vlite-error-code: tenant_suspended`; tenant lifecycle administration and the
signed-in `identity` are REST-only.

## Scope

The protobuf service covers listing, inspecting, creating and dropping
collections; vector get/put/delete; atomic single-collection batches; exact,
ready-only ANN and hybrid retrieval; and resumable import reservation, chunk,
status, finish and abort operations. Ready-only vector search never builds an
index. Hybrid search supports exact or ready-only vector legs, optional text-
only retrieval, metadata filters and weighted reciprocal-rank fusion.
Resumable-import RPCs use the same durable chunk identity and replay behavior as
REST.

Import RPC responses wrap the corresponding REST JSON body in
`ImportJsonResponse.json`, preserving decimal-string job IDs without narrowing
64-bit identifiers. A rejected import chunk returns `INVALID_ARGUMENT` with
`vlite-error-code: import_rows_invalid`; its
`vlite-error-details-base64` trailer contains base64-encoded UTF-8 JSON in the
same shape as the REST rejection body, including bounded per-row validation
details.

REST remains the operational interface for auto retrieval, index quality and
maintenance, backup/restore, credentials and diagnostics. There is no
reflection API.

## Extended engine RPCs

The additive RPCs expose the engine's V5 record and retrieval APIs. `PutVector`
accepts optional absolute `expires_at_ms`, which must be later than the server
clock: an expiry at or before it is refused with `INVALID_ARGUMENT`
(`vlite-error-code: invalid_input`) and nothing is written. `UpdateMetadata`
changes metadata without replacing the vector or payload. Both require a V5
file. Grouped search,
recommendations, facets, numeric statistics and geographic search preserve the
engine's typed metadata, filters, metrics and stable ID tie-breaking.

`SearchRequest.rerank_mode` optionally applies exact metric re-scoring or MMR to
returned vector candidates. `rerank_candidates` sets candidate depth (bounded to
4096 and at least `k`); MMR defaults to lambda 0.5. Hybrid result sets cannot be
post-reranked. Named-vector configuration, version upserts, vector/sparse puts,
named and sparse searches, and dense+sparse fusion are also V5-only. These RPCs
operate on the collection named in the request and follow the corresponding
read/search/write scoped-credential permissions.

`Query` accepts the bounded read-only query language described in
[VECTORS.md](VECTORS.md); vector parameters are supplied separately in the
request map. `Changes` returns a durable cursor token and commit-boundary batch.
`WatchChanges` starts at that token (or the beginning when empty), emits batches
as they appear, and continues polling until the client cancels. Watch/change
access is admin-only because feed batches may contain events from any
collection. The watch interval is clamped to 100 ms–60 s, and credentials are
rechecked while polling. Since v0.6.0 both also serve graph-enabled files:
`ChangeKind` adds `GRAPH_CREATED`, `GRAPH_DROPPED` and `GRAPH_BATCH`, and
`ChangeEvent` adds `graph`, `revision` and `graph_operations`
(`GraphChangeOperation`); see
[Change feed and webhooks](GRAPH.md#change-feed-and-webhooks).

`BuildDiskAnn` and `SearchDiskAnn` use an operator-specified sidecar path. They
are not available for encrypted databases; the sidecar contains plaintext
vectors, so callers must protect its directory and restrict these admin/index
operations. `Info` reports format, sequence, lineage, timestamp support, and
whether the database is encrypted plus its non-secret key ID—never key material.
This gRPC server currently owns a primary `Database`, not a
`ReplicaFollower`; `is_replica` is false and the optional replica detail is
absent. Replica transport/status RPCs are not yet provided.

`EmbedText` exposes text-to-vector embedding as described below; clients can
also supply vectors from their own provider. The embedded engine's `Embedder`
trait and optional HTTP provider remain usable by applications.

### Managed inference

`EmbedText {collection, text, provider}` returns `{vector, dimensions, provider}`.
An empty provider or `hashing` selects the local hashing embedder; `http` retains
the existing opt-in HTTP embedder behavior. Any other provider is an
administrator-bound alias, never a caller-supplied endpoint or credential.
Managed providers remain off by default. Without provider configuration,
hashing and existing search modes issue no provider calls.

`Search` accepts `RERANK_MODE_CUSTOM=3` and optional `rerank_provider` (tag 18).
The provider is required for custom mode and forbidden for other modes.
Custom mode uses the shared REST workflow: `1 <= k <= candidates <= 1000`,
default candidates `max(k,50)`, vector-only search, and no MMR lambda.
Existing exact/MMR behavior is unchanged: at most 4096 candidates and default
`4*k`, with MMR lambda default 0.5. These defaults intentionally differ.

Custom results preserve the original metric `distance`. Each returned `Hit`
has optional `rerank_score` (tag 3), the negated provider cost (positive zero
for zero cost). Results are sorted by descending score, then numeric ID.
`SearchResponse.ranking` (tag 3) is `{provider, order:"score_desc_id_asc"}`.
Non-custom responses omit both optional additions.

Both operations require bearer authentication and search scope before provider
lookup; read-only bearer credentials are also allowed. Bindings use database
lineage, collection identity and the collection's owning tenant, not the caller's
tenant. The shared workflow releases the database
guard during inference and rechecks authorization, collection identity,
dimensions, binding generation, candidate revisions/expiry/visibility and the
original deadline before returning. Empty `payload_fields` never inspects
payload bytes. Otherwise only named top-level fields of a JSON object are sent;
absent named fields are omitted, invalid/non-object JSON fails, and metadata
is never sent implicitly.

All `EmbedText` input, including hashing, is limited to 16,384 UTF-8 bytes.
The managed adapters enforce
1 MiB request / 4 MiB response caps, at most 2 s connect / 10 s total timeouts,
non-blocking per-provider/per-tenant admission, TLS outside explicit loopback
HTTP exceptions, endpoint allowlisting, no redirects and at most one retry
only for replay-safe aliases. Responses require finite values, exact embedding
dimensions and exactly `k` unique ranking IDs from the candidate set.
For managed operations, a shorter standard `grpc-timeout` also cancels outbound
work; an already expired deadline prevents dispatch. Malformed timeout metadata
fails closed as `invalid_provider_input`. Tonic may enforce the transport
deadline first, returning `CANCELLED` with message `Timeout expired` and no
`vlite-error-code` trailer. A client `RST_STREAM` does not cancel the detached
managed inference task; it remains bounded by the 10 s cap and admission slots.

Provider errors use the existing `vlite-error-code` trailer:

| Stable code | gRPC status |
|---|---|
| `provider_not_configured` | `FAILED_PRECONDITION` |
| `invalid_provider_input` | `INVALID_ARGUMENT` |
| `provider_invalid_response` | `UNAVAILABLE` |
| `provider_unavailable` | `UNAVAILABLE` |
| `provider_timeout` | `DEADLINE_EXCEEDED` |
| `provider_busy` | `RESOURCE_EXHAUSTED` |
| `provider_snapshot_conflict` | `ABORTED` |

`provider_busy` reuses the existing `retry-after: 1` trailer. The provisional
`ABORTED` snapshot-conflict mapping corresponds to REST 409
`provider_snapshot_conflict`. Conflicts are sanitized and have no retry hint;
retry the whole request. Missing bearer credentials remain `UNAUTHENTICATED`;
wrong scope or tenant remains
`PERMISSION_DENIED`. Errors do not expose provider inputs, vectors, payloads,
tenant identifiers or secrets.

The additions preserve existing v1 tags and enum meanings. Old clients can
continue using their existing modes and ignore the optional additions. An older
server has no `EmbedText` RPC and returns `UNIMPLEMENTED`; generated clients
propagate that status without fallback. Its `Search` handler rejects unknown
`RERANK_MODE_CUSTOM` as `INVALID_ARGUMENT` (`invalid_input`); it must not be
treated as exact/MMR or retried with another mode. New clients must handle both
cases explicitly. Rust bindings are generated by `build.rs`; the Python smoke
test generates its stubs from the same proto. No generated stubs are checked in.

## Key-value entries

`vlite.v1.Kv` provides `GetKvEntry`, `PutKvEntry`, `DeleteKvEntry` and
`ListKvEntries`. The four ordinary KV RPCs join 35 `VLite` RPCs (39 total);
there are no lease RPCs. Addresses require a nonempty collection name.
Values are raw bytes, versions/deadlines are `uint64`, and optional expiry
distinguishes an unset deadline from zero. Get/list require collection `read`;
put/delete require collection `write`. Validation, CAS, tenant bindings,
quotas, audit, durability and pagination match [the REST KV rules](API.md#key-value-entries).
The list page boundary is measured using the canonical REST JSON encoding,
including base64 values, not the smaller protobuf encoding.
A client needs a receive limit of at least 16 MiB + 1 KiB for `ListKvEntries`
(the canonical JSON page cap is 16,777,216 bytes; the protobuf page is not larger)
and a send limit above 1 MiB.

## Streaming export and import

`ExportVectors` is a server stream of `Vector` messages for one collection, in
strictly ascending ID order without duplicates. It reads `page_size` records
(0 means 256; maximum 1000) per page. Each page is an atomic snapshot, but
writes committed between pages may or may not appear: the stream is not a
point-in-time snapshot of the whole collection. Pass the last received ID as
`after` to resume after a disconnect. Credentials and revocation are rechecked
for every page, so a revoked token ends the stream with an error. Read-only
tokens may export.

`StreamImport` is a client stream over an already reserved import job. Each
message names the same `job` and carries one numbered chunk with the same
durable, atomic, idempotent semantics as `ImportChunk`. Set `finish` only on
the last message; a control-only finish message (`chunk` 0, no operations) is
allowed. The response is the finish result or, without `finish`, the current
job status. If the stream fails, chunks already committed stay committed and
the job remains open: read `GetImportStatus` and resume from `last_chunk + 1`,
or replay the last chunk safely.

Both transports share `requests_total` and `request_failures_total`. The admin
`/metrics` view also reports `grpc_requests_total`,
`grpc_request_failures_total` and `grpc_stream_messages_total`. A streaming
call is recorded once when it is accepted. A later mid-stream failure, such as
revocation during export, also increments both failure counters and writes a
`stream_error` log line with the original request ID.

IDs and revisions are protobuf `uint64`, vector components are `float`, payloads
are raw bytes, and typed metadata is represented with protobuf oneofs. Generate
clients from the shipped `proto/vlite/v1/vlite.proto`; do not hand-convert
full-width IDs through JavaScript `number`.

## Authentication and failure semantics

Send the same bearer credential as REST in `authorization` request metadata.
Admin, read-only and scoped credentials use the same authorization and
collection-lineage bindings. A client cannot gain write access by changing
transports. Public standard gRPC health is a liveness signal, not permission to
read data.

Invalid input maps to `INVALID_ARGUMENT`, missing records to `NOT_FOUND`,
scope failures to `PERMISSION_DENIED`, and unavailable ready indexes to
`FAILED_PRECONDITION`. Conditional conflicts use `ABORTED`. Capacity exhaustion
uses `UNAVAILABLE`. The `vlite-error-code` metadata preserves VLite's application
error code.

**Do not blindly retry writes.** A commit-indeterminate failure uses `UNKNOWN`:
the commit may already be durable. Read back the record or inspect application
state before deciding what to do. A disconnected client does not cancel or
roll back a database commit.

REST and gRPC share the owner and bounded request/worker resources. Unary
requests and individual stream messages are capped at 8 MiB and responses at
16 MiB. A large response is an
explicit transport failure, not a truncated successful result.
