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

## Memory-bound resources

When `VLITE_MEMORY_BINDINGS_FILE` is configured, each binding reserves its
stored graph and evidence collection from raw server routes. Memory operation
grants do not grant raw graph, record, search, feed or attachment access.
`raw_access` is false unless explicitly granted in the private bindings file;
even with it, the caller still needs the ordinary tenant, collection and
operation permissions. Reserved resources are omitted from raw collection/graph
lists, tenant usage, slow-query diagnostics, and change feeds; feed cursors
still advance across the unfiltered page. `GET /v1/database` reports only the
default collection; startup rejects `default` as a binding's evidence collection,
so bindings cannot reserve legacy default-collection routes. With any binding
configured, global metrics, graph guard
statistics, replication export, webhook list/status, all import operations,
and automatic-maintenance routes return `403 forbidden_scope`, including for
credentials with `raw_access`. Webhook
subscriptions accept reserved-name filters just as they accept missing-name
filters; the dispatcher still filters reserved events from all subscriptions. The gRPC
listener and scheduled automatic index maintenance also fail closed until those
paths can enforce the same checks.

Without raw access on the owning binding, a raw request naming its graph or
collection answers as if that resource were missing, using the ordinary
`404 collection_not_found` or `409 graph_not_found` response. Ordinary grant
checks still run first: an out-of-scope name retains `403 forbidden_scope`.
Retrieve evidence remains redacted when outside ordinary grants; an explicitly
named reserved evidence collection instead answers like a missing collection.
Creating a resource under a reserved name answers like an occupied ordinary
name (`409 collection_exists` or `409 graph_exists`); that collision cannot be
hidden. The per-node attachment check on `delete_node` still requires raw
access to each reserved evidence collection attached to that node.
Provision raw-holder credentials with `collections:["*"]` before granting raw
access in the bindings file; credential creation treats a named reserved
collection as missing. A memory agent's credential needs the same care: every
memory route requires the credential to name the binding's evidence collection
(or hold `["*"]`) as well as the `memory` operation, and once the bindings file
is in place a request to create a credential that names that collection answers
`404`. Create such credentials before you configure the binding (they keep
working, with the early-grant limit below), or give them `["*"]`. A root
credential with `["*"]` and `read` also reaches every other collection and the
global change feed, so keep it for trusted agents; a tenant credential's
wildcard stays inside its tenant.

**Known limits.** A tenant that owns a binding cannot be deleted and answers
`409 tenant_not_empty` even when no visible collection remains. Sequence numbers are
database-wide: graph `view_sequence` and `revision`, change-feed `head` and
memory receipt `sequence` advance for hidden writes, so a caller can count the
activity of other tenants and of memory, but not read it. Idempotency keys are
scoped to the binding and the memory scope, not to the credential, so any
credential that may write a scope can replay another credential's request by
repeating its key and exact content. The bindings file is not stored in the
database: a restart without a binding ends its reservation at once. Keep a
binding you no longer use as `"state": "retired"` instead of deleting it, and
move or delete its data first if it must become raw-readable. Startup refuses a
graph or collection that holds ordinary data, but one node with the
`VliteMemoryScope` label satisfies that check, so bind only resources that were
created for memory. The administrator's HTTP request log and persistent audit
show reserved names and record IDs, and collection list `id` values have gaps
where reserved collections sit. `raw_access` covers every scope of its
binding's resources, not one memory scope. A credential that holds the `backup`
operation can create, verify and drill backups, and their database-wide counts
of collections, records and commits include reserved resources; the backup
files themselves hold reserved data and are operator-managed. A raw credential
can attach reserved evidence records to nodes of an unreserved graph, and other
users of that graph can then neither delete nor detach those nodes. The same
holds the other way: a raw credential can attach an ordinary record to a node of
its reserved graph, and nobody without raw access can then delete, expire or
re-attach that record or drop its collection until the holder detaches it; the
refusals show that something holds it. A credential created before a binding,
whose grant already names a reserved collection, answers 404 for that name while
a missing name outside its grant answers 403, because the grant check runs
first; create such credentials after the bindings file is in place. Webhook
subscriptions and credential graph lists that name a reserved resource begin to
match it if the binding is removed. Startup refuses `default` as an evidence
collection, so a binding never changes the legacy routes. Live vector quotas
count reserved records, so a caller with a quota sees its headroom change when
memory writes, while tenant usage reports leave reserved collections out and
can show more headroom than the quota allows. An attempt to use a reserved name
answers like a missing
resource, so it is not counted as a credential denial in the metrics or the
audit log. A binding's receipt window, generation and names are read from the
file at each start and are not checked against the database, so do not change
them once memory has been written (see
[Agent memory](API.md#start-the-server-with-memory)).
The bindings file is read through symbolic links; keep it in a directory that
only the operator can write.

These checks apply only at the server boundary. Direct embedded/file access and
operator-managed backups are outside this authorization policy and must be
protected separately.

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
`rate_limited_total`, `quota_exceeded_total` and `kv_slots_exhausted_total`
(key-value puts refused because all 16 of a key's record IDs were taken; see
[Key-value permissions](#key-value-permissions)).

Put, batch and import-chunk projections run under the database owner lock before
commit, preventing concurrent requests from overfilling a quota. Counts concern
records visible within a scope, not historical writes or attribution of which
credential originally wrote a record. Import skip-invalid mode conservatively
checks the entire submitted chunk before skipping rows; it can reject a chunk
whose valid subset would fit. Already-expired records still occupying engine
storage can consume quota until physically removed. Streaming quotas apply to
each committed chunk; a request rate is charged once per RPC, not per message.

## Audit actors

Audit events that record who acted carry an `actor`: a stable identity that
holds no secret. It is never built from a cookie, token, session key, user
name or request content. It has four forms:

| `actor` | Who acted |
|---|---|
| `admin_token` | The bootstrap admin token, or any caller of a server with no admin token (open local mode). |
| `user:<account id>` | A signed-in dashboard account of any role. An account has the same actor in every one of its sessions. |
| `read_only_token` | The read-only token. It can reach only the four memory read routes (recall, receipt lookup, facts query, erasure get), where the handler refuses it and audits the refusal as a `memory_request`; none of the other events below shows this form. |
| The credential ID | A scoped API key. Its 12 characters from `a-z2-7` cannot equal another form. |

These events carry an `actor`: `kv_put`, `kv_delete`, `memory_request`,
`collection_created`, `collection_dropped`, `collection_settings_changed`,
`credential_denied`, and the account events `user_created`, `user_updated`,
`user_deleted`, `user_password_reset` and `user_password_changed`. The account
that the server creates at startup from `VLITE_ADMIN_USERNAME` is recorded with
the actor `bootstrap`, the one value outside the four forms. Other audit events
do not carry an `actor`.

`kv_put`, `kv_delete` and `memory_request` keep their `principal` field as it
was: `admin` for the bootstrap token and for every super admin, so it cannot
tell them apart, and the credential ID or `user:<account id>` for a scoped
caller. `actor` is added beside it. A memory route serves only a scoped
credential that a binding grants, so a bootstrap-token or super-admin request to
one is refused with 403 and audited with its own actor. `credential_denied`
keeps `credential_id` and adds an `actor` that equals it. The collection events
already had an `actor`, which read `admin` for the bootstrap token and for every
super-admin session before 0.7.1; it now reads `admin_token` or
`user:<account id>`, and the credential ID is as before.

## Key-value permissions

Collection KV GET/HEAD and list need the collection's `read` scope; PUT and
DELETE need `write`. These scopes also apply to `vlite.v1.Kv`:
`GetKvEntry`/`ListKvEntries` need `read`, and
`PutKvEntry`/`DeleteKvEntry` need `write`. Graph scopes do not grant KV access.
Read-only credentials cannot mutate. Tenant-local names and immutable
lineage/collection bindings are checked before collection lookup;
a binding for a dropped collection does not authorize its replacement.
KV never implicitly creates a collection and shares ordinary record quotas.
Mutation audit entries contain operation, principal, [actor](#audit-actors),
collection, request ID, outcome and, for a put, its expiry deadline, never
namespace, key or value. A failed request records the collection as `(invalid)`
and still records the actor.

A namespace is an application partition **inside** an authorized collection,
not an authorization boundary. Put sensitive KV data in dedicated collections
and bind credentials narrowly: vector, graph-attachment and import permissions
can also expose, overwrite, delete or expire ordinary KV records. A principal
that can write the collection can also fill a key's 16 record IDs with ordinary
records, so that puts for that key fail with `kv_slots_exhausted` (409 on REST,
`FAILED_PRECONDITION` on gRPC). Use dedicated collections for key-value data,
grant `write` on them only to principals you trust with their keys, and watch
`kv_slots_exhausted_total`: it counts each REST or gRPC request answered this
way, across all credentials (a request whose client left before the answer was
produced is not counted, as with every request counter), and a rise nobody
expects points at records planted over a key's IDs. The failed `kv_put` audit event names the caller
(`actor`) and the request ID, not the collection, and global metrics are
unavailable while memory bindings are configured (see above). A list call
scans the whole collection, so its cost grows with the number of entries rather
than with `limit`, and heavy listing can slow writers; the memory it holds is
bounded by `limit` and the 16 MiB page, not by the number of entries. CAS revisions
are not a durable lease-fencing service. See [REST KV](API.md#key-value-entries).

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
* `DELETE /v1/admin/tenants/{id}` requires every tenant collection and graph
  to be dropped first (409 `tenant_not_empty`
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
share the server admission limits and storage failure domain, and one commit
sequence: graph `view_sequence` and the change-feed `head` advance for other
tenants' writes. Import job IDs are allocated for the whole file, so a request
about another tenant's job answers `403` while an unused ID answers `404`, and a
tenant's first chunk waits behind earlier reserved jobs of any tenant (the
`409 import_conflict` names the earlier job). Scoped metrics and import
responses show a tenant's own collections by their stored names
(`t<id>__name`). The private
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
