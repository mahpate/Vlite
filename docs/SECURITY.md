# Server security

How VLite signs people in, protects transport, limits credentials, isolates
tenants and exports traces. Protect the private `VLITE_ADMIN_DIR`: it holds
dashboard accounts, audit history, credential hashes, tenant configuration and
backups. TLS does not replace bearer authentication or collection and operation
authorization.

## Dashboard sign-in and accounts

People sign in to the dashboard with a username and password. Apps and scripts
keep using API keys, and the admin token stays for automation and recovery.
gRPC, the SDKs and MCP are unchanged.

| Role | Can |
|---|---|
| Super admin | Manage the server, tenants, users and API keys. |
| Editor | Read, search, write, import, index and manage collections. |
| Viewer | Read and search. |

Editors and viewers are bound to one tenant, and see only its collections, or
to all collections.

With the property graph (compiled into every build since v0.5.1), signed-in
viewers also get `graph_read` and editors `graph_read` and `graph_write`,
narrowed per account by `graphs` and `graph_access`
([graph scoping](GRAPH.md#surfaces)). Graphs are
tenant-owned: tenanted accounts and tenant API keys reach only
`t<tenant_id>__*` graphs, under tenant-local names. Vector isolation is kept: graph batches
that attach, detach, write or delete vectors, and `delete_node` on a node
with attachments, need `write` on a collection the account's tenant owns,
named by its stored name `t<tenant_id>__<name>`. Retrieve
(`POST /v1/graphs/{graph}/retrieve`) needs `read` on the pivot collection by
the same rule, and evidence from any other collection is dropped and counted
as `redacted`. Tenant API keys may hold graph operations with tenant-local
`graphs` bindings. The 64-graph limit is per file, shared by every tenant.

- **Requirements:** password sign-in needs protected mode, meaning an admin
  token and the admin directory. Editor and viewer accounts also need a
  format v4 or newer database. Sign-in is accepted only over HTTPS or when the
  server listens on loopback only. When a TLS-terminating or local reverse
  proxy fronts the API, set `VLITE_TRUSTED_PROXY=1`; Docker Compose sets it.
  Open local mode (`--insecure-local`) has no sign-in.
- **First super admin:** there is no default account or password. While no
  account exists, the sign-in screen asks for the admin token. Sign in once
  with it, open **Administration → Users & access** and create a super admin
  account. To create it at
  startup instead, set `VLITE_ADMIN_USERNAME` and `VLITE_ADMIN_PASSWORD_FILE`,
  a private file of up to 4 KiB that holds only the password. The server
  creates the account only while no super admin exists and never logs the
  password. Remove both settings and the file afterwards.
- **Passwords:** 15–128 characters, with no composition rules. Common or
  obvious passwords are rejected. The server stores only Argon2id hashes. A
  password set by an admin is temporary and must be changed at first sign-in.
  Users who forget their password ask a super admin to reset it.
- **Sessions:** an HttpOnly, SameSite=Strict cookie. By default a session ends
  after 30 minutes idle or 8 hours in total; change these with
  `VLITE_SESSION_IDLE_MINUTES` (1–1440) and `VLITE_SESSION_MAX_HOURS`
  (1–720). A server restart signs everyone out. Repeated failures slow down sign-in for that username, with delays of
  up to 15 minutes.
- **Tenants:** suspending a tenant blocks its users' sign-in and ends their
  sessions. Deleting a tenant removes its user accounts.

The routes are listed in [API.md](API.md#dashboard-sign-in-and-accounts).

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

`POST /v1/admin/tls/reload` requires a super admin. It rereads all
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
Quota failures require deleting records, raising a tenant limit or issuing a
higher-limit credential; waiting alone does not resolve them. JSON/Prometheus metrics include
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

Tenant routes need a super admin and `VLITE_ADMIN_DIR`:

* `POST /v1/admin/tenants` with `{"name":"Acme","max_vectors":10000}` creates
  a tenant and returns `tenant.id`; `max_vectors` is optional (unlimited).
* `GET /v1/admin/tenants` returns active tenants with `state`
  (`active`/`suspended`), `collections` and `vectors`. Display names are not IDs
  and need not be unique.
* `GET /v1/admin/tenants/{id}/usage` reports each tenant collection with its
  physical name and vector count, total `vectors`, `max_vectors`,
  `remaining_vectors` (`null` when unlimited, saturating at 0), `over_quota`
  (usage above a lowered limit) and bound API key counts
  (`active`/`expired`/`revoked`).
* `PUT /v1/admin/tenants/{id}/settings` with `{"name":"Acme EU"}`,
  `{"max_vectors":50000}`, `{"max_vectors":null}` (unlimited) or both renames
  and/or changes the aggregate limit. Unknown fields or an empty body return 422.
  Lowering a limit below usage keeps existing vectors and refuses new ones.
* `POST /v1/admin/tenants/{id}/suspend` and `.../resume` are idempotent. While
  suspended, every API key bound to the tenant gets 403 `tenant_suspended` on
  REST and `PERMISSION_DENIED` with the same code on gRPC, and new keys for the
  tenant are refused with 409 `tenant_suspended`. Data is kept; the bootstrap
  admin can still read, write (within the tenant quota) and drop its
  collections. Resume restores unexpired, unrevoked keys. Suspension persists
  in `tenants.json` across restarts. Older servers refuse to start on a store
  that contains a suspended tenant, so resume tenants before downgrading.
* `DELETE /v1/admin/tenants/{id}` requires every tenant collection (and, with
  the `graph` feature, graph) to be dropped first (409 `tenant_not_empty`
  otherwise). Deletion tombstones the immutable ID
  and immediately makes every bound credential fail closed (403
  `tenant_unavailable`). IDs are never reused. Unknown or deleted IDs return 404
  `tenant_not_found` on the lifecycle routes.

Apps reach a tenant with API keys bound to its ID. People use dashboard
accounts bound to it; see
[dashboard sign-in and accounts](#dashboard-sign-in-and-accounts). Every tenant
mutation (create, update, suspend, resume, delete) is audited.

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
