# Server security

All additions described here are opt-in. Existing loopback plaintext servers,
bootstrap admin/read-only tokens and non-tenant scoped credentials retain their
existing behavior. Continue protecting the private `VLITE_ADMIN_DIR`: it contains
audit history, credential hashes, tenant configuration and backups. TLS does not
replace bearer authentication or collection/operation authorization.

## Native REST TLS and gRPC mutual TLS

Build with `--features server` (REST) or `--features grpc` (REST and gRPC):

```text
vlite-server data.vlite --token-file admin.token --bind 0.0.0.0:8080 --allow-remote --tls-cert server-chain.pem --tls-key server-key.pem
```

`--tls-cert` and `--tls-key` must be supplied together. The PEM chain is leaf
first. Files, private-key parsing and key/certificate agreement are validated
before the database is opened or created. The startup address is
`LISTENING https://...`. TLS 1.2 and TLS 1.3 use rustls/ring; HTTP/1.1 and HTTP/2
are supported. A trusted reverse proxy remains an alternative; `--allow-remote`
never implies that plaintext bearer transport is safe.

For native gRPC TLS add `--grpc-bind`, `--grpc-tls-cert` and `--grpc-tls-key`.
Adding `--grpc-tls-client-ca client-ca.pem` makes client certificates mandatory:
unknown issuers, missing certificates and invalid client chains are rejected at
the TLS handshake. Clients must still send a bearer token for protected RPCs.
REST does not require client certificates; deploy proxy mTLS if that is needed.

`POST /v1/admin/tls/reload` requires the bootstrap admin role. It rereads all
registered REST/gRPC identity files and gRPC client CA files, validates every
candidate first, then publishes all replacements. Malformed/unreadable files
return 422 `tls_reload_failed`; **all old configurations remain active**. No
listeners yields 409 `tls_disabled`. Existing connections keep their original
TLS session; new handshakes see the replacements. This works on Windows without
SIGHUP. Rotate files in a private directory and call reload only after every
file is ready. There is no automatic watcher. In-process users can construct
`TlsIdentity::from_files`, register it with `Service::register_tls`, and serve
`TlsListener`; in-memory identities cannot be reread from disk.

Handshake concurrency and waiting queues are bounded to 64, with a ten-second
handshake deadline. One silent peer cannot serialize acceptance of other peers.

## Credential request rates and live-vector quotas

`POST /v1/admin/credentials` accepts these additional optional fields:

* `requests_per_second`: integer 1..1,000,000. Shared monotonic token bucket
  across REST and gRPC, with capacity equal to one second of requests. Each
  authenticated protected request consumes one token, including rejected
  authorization attempts. Public health/dashboard routes are not charged.
* `max_vectors`: non-negative aggregate stored-vector count across collections
  covered by this credential, including records written by other principals.
* `max_vectors_per_collection`: non-negative stored-vector count per addressed
  collection. Updating an existing ID does not consume an additional slot.
* `tenant_id`: immutable tenant ID described below.

These settings persist with the credential and survive rotation/restart; a
rotation's replacement credential has its own bucket. Rate bucket occupancy is
process-local and resets full on restart. Limits do not apply to bootstrap
admin/read-only roles, except that tenant aggregate quotas are enforced on
privileged writes into tenant collections too. REST replies with 429 and
`Retry-After: 1`, using `rate_limited` or `quota_exceeded`. gRPC uses
`RESOURCE_EXHAUSTED`, the same stable code and `retry-after: 1` metadata.
Quota failures require deleting records or issuing a higher-limit credential;
waiting alone does not resolve them. JSON/Prometheus metrics include
`rate_limited_total` and `quota_exceeded_total`.

Put, batch and import-chunk projections run under the database owner lock before
commit, preventing concurrent requests from overfilling a quota. Counts concern
records visible within a scope, not historical writes or attribution of which
credential originally wrote a record. Import skip-invalid mode conservatively
checks the entire submitted chunk before skipping rows; it can reject a chunk
whose valid subset would fit. Already-expired records still occupying engine
storage can consume quota until physically removed. Streaming quotas apply to
each committed chunk; a request rate is charged once per RPC, not per message.

## Tenants

Tenants require bootstrap admin authentication and `VLITE_ADMIN_DIR`:

* `POST /v1/admin/tenants` with `{"name":"Acme","max_vectors":10000}` creates
  a tenant and returns `tenant.id`; `max_vectors` is optional (unlimited).
* `GET /v1/admin/tenants` returns active tenants. Display names are not IDs and
  need not be unique.
* `DELETE /v1/admin/tenants/{id}` requires every tenant collection to be dropped
  first (409 `tenant_not_empty` otherwise). Deletion tombstones the immutable ID
  and immediately makes every bound credential fail closed. IDs are never reused.

Issue a scoped credential with `tenant_id` and either tenant-local collection
names or `["*"]`. The wildcard grants only that tenant's namespace, not global
administration, backups, default collection access or other tenants. Tenant
credentials use local collection names consistently in create, list, fetch,
settings, vector/search and import requests over REST/gRPC. A tenant wildcard
with `collections` operation can create its own collections. Explicit grants
still bind the original collection ID and lineage and cannot recreate deleted
bindings. Deny-unmapped scope authorization remains in effect.

Physical names are `t<32-hex-tenant-id>__<local-name>`; global admins can inspect
these names. The engine's 63-character name bound leaves 28 characters for a
tenant-local name. Tenants are grouping/isolation policy over the same database,
not separate files, cryptographic isolation or independent processes. All tenants
share the server admission limits and storage failure domain. The private
`tenants.json` uses bounded validated loading and synced atomic replacement;
1024 lifetime tenant IDs (including tombstones) are supported. Invalid stores
fail startup instead of silently removing isolation. A post-replacement directory
sync failure reports 503 `tenants_commit_indeterminate` with memory following the
visible file; inspect state before retrying. Tenant audit mutations are synced
before persistence. Use global operator credentials only in trusted tools.

## Optional OpenTelemetry

Build with `--features otel` (and optionally `grpc`). Configure OTLP/gRPC via
`--otlp-endpoint http://collector:4317` or `OTEL_EXPORTER_OTLP_ENDPOINT`.
Without an endpoint no exporter is installed; builds without `otel` reject an
endpoint rather than silently ignoring it. HTTPS endpoints use trusted system
CA roots. Collector configuration belongs to the operator; do not send traces
outside your trust boundary.

Each REST request and gRPC admission/core call emits `vlite.request` with an
explicit allowlist: `request_id`, matched `route`, addressed `collection` when
known, and HTTP-equivalent response status. Import job-only RPCs do not provide
a collection name. Vectors, filters, payloads, metadata values, bearer tokens,
raw request bodies and query strings are never trace attributes. Streaming spans
cover admission; stream completion/failures retain existing metrics and logs.
The batch exporter is shut down after normal graceful server shutdown. Export
failures do not change database durability or request success.
