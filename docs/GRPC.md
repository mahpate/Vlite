# Optional gRPC transport

VLite's versioned protobuf contract is `vlite.v1.VLite`. It is a bounded data
transport over the same exclusively owned database as REST, not another storage
engine. Build the server with the optional `grpc` Cargo feature:

```text
cargo build --locked --features grpc --bin vlite-server
```

gRPC is disabled unless `--grpc-bind` is explicitly supplied. Bind to loopback
by default. Remote listeners require `--allow-remote` and a trusted TLS
terminating proxy; the binary itself serves plaintext. Do not expose bearer
credentials over an untrusted plaintext network.

## Scope

The protobuf service covers listing, inspecting, creating and dropping
collections; vector get/put/delete; atomic single-collection batches; and exact
or ready-only ANN search. Ready-only search never builds an index. Use REST's
explicit index build or maintenance controls before querying it.

REST remains the operational interface for imports, hybrid/auto retrieval,
index quality and maintenance, backup/restore, credentials and diagnostics.
There is no reflection or streaming data API in this release.

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
requests are capped at 8 MiB and responses at 16 MiB. A large response is an
explicit transport failure, not a truncated successful result.
