# Optional gRPC transport

VLite's versioned protobuf contract is `vlite.v1.VLite`. It is a bounded data
transport over the same exclusively owned database as REST, not another storage
engine. Build the server with the optional `grpc` Cargo feature:

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
accepts optional absolute `expires_at_ms`; `UpdateMetadata` changes metadata
without replacing the vector or payload. Both require a V5 file. Grouped search,
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
rechecked while polling.

`BuildDiskAnn` and `SearchDiskAnn` use an operator-specified sidecar path. They
are not available for encrypted databases; the sidecar contains plaintext
vectors, so callers must protect its directory and restrict these admin/index
operations. `Info` reports format, sequence, lineage, timestamp support, and
whether the database is encrypted plus its non-secret key ID—never key material.
This gRPC server currently owns a primary `Database`, not a
`ReplicaFollower`; `is_replica` is false and the optional replica detail is
absent. Replica transport/status RPCs are not yet provided.

There is no server-configured embedding provider in the current server
configuration. Consequently this API does not expose text-to-vector embedding;
clients can send vectors from their own provider. The embedded engine's
`Embedder` trait and optional HTTP provider remain usable by applications.

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
