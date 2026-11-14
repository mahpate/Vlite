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
`raw_access` is false unless explicitly granted in the private bindings file,
and a grant that lists `read_scopes` cannot hold it;
even with it, the caller still needs the ordinary tenant, collection and
operation permissions. It is also the opt-in for naming an existing evidence
record in a `remember` item. That transfers the record to the memory, so
forgetting the memory deletes the record too, and it is a one-way change to the
file: a 0.7.0
server answers 500 for every memory operation of a scope that holds such a
memory, and other scopes and the evidence record are unaffected (see
[Upgrading from 0.7.0](API.md#start-the-server-with-memory)). Reserved resources are omitted from raw collection/graph
lists, slow-query diagnostics, and change feeds; feed cursors
still advance across the unfiltered page. `GET /v1/database` reports only the
default collection; startup rejects `default` as a binding's evidence collection,
so bindings cannot reserve legacy default-collection routes. With any binding
configured, global metrics, graph guard
statistics, replication export, webhook list/status, all import operations,
and automatic-maintenance routes return `403 forbidden_scope`, including for
credentials with `raw_access`. Webhook creation answers a reserved-name filter
exactly as it answers a missing-name filter (both are accepted with `201`), so
a subscription cannot be used to find out what is reserved; the dispatcher
still filters reserved events from all subscriptions. The gRPC adapter applies
the memory reservations of the REST router it is built after, resolves every
name through the same per-request guard as the REST routes, and the generated
guard differential runs the REST differential's checks on every RPC (it never
compares a REST answer with a gRPC answer). The server still
refuses `--grpc-bind` and scheduled automatic index maintenance at startup
while a binding is configured or reserved resources remain.
The [dashboard](CONSOLE.md#monitor-the-server) shows the refused views above as
unavailable, and sign-in still works, while agent memory is enabled or the
database still holds memory binding records after the bindings file is removed.

`vlite-server` can opt back in to the metrics routes with
`VLITE_METRICS_WITH_MEMORY=1` (library embedders: `ServerOptions::metrics_with_memory`).
`GET /metrics` (JSON and Prometheus) and `GET /v1/admin/metrics/summary` then
answer as they do without memory. Their totals (database bytes, record and
collection counts, search counters) include the reserved resources, so a caller
who can read metrics learns how much memory data exists. Reserved names stay out
of per-collection rows, and scoped callers still see only their own collections.
Graph statistics, replication export and the other routes above stay `403`.
The switch is off by default; leave it off when aggregate memory sizes must
stay private.
It does so for the super admin, for a read-only token on the routes its role
may call and for a scoped credential only where the refusal cannot be an
ordinary scope denial; a tenant editor's refusal on the import routes looks like
any other `403` and shows the server's own message.

Each bound graph holds a binding record, the node `binding:v1`, with the
binding's name, tenant, generation, state, receipt window, graph and evidence
names and a digest of its grants. Every start checks the file against the
records of all graphs (see [Agent memory](API.md#start-the-server-with-memory)),
adopts a graph only when it is empty, holds the binding's record, or holds
nothing but memory nodes, and writes the record on a writable primary. A
database whose graphs hold records keeps those graphs and their evidence
collections reserved, with no grants and no memory routes, even when the file
names no binding or is gone, and the refusals above (gRPC, automatic
maintenance, imports, replication export) then apply too.

The admin-only `POST /v1/admin/memory/bindings/reload` rereads the configured
file and applies only grants and their higher generations. New or retired
bindings, receipt windows, tenant, state, graph and evidence changes still
need a restart. Reload is refused after a gRPC router is built.
The admin-only `POST /v1/admin/memory/bindings/preflight` is a read-only,
advisory check; reload rereads and revalidates. It holds a shared database
lock, so writers can queue behind it, but never takes the reload lock.
Admin-only `GET /v1/admin/memory/bindings/status` takes neither lock and
reports an advisory snapshot; after reload, poll until `file` is `agrees`
and `drained` is true. Neither handler provisions, writes, installs or drains.
Responses name no binding, tenant, principal, scope, graph, collection or path;
refusal codes and messages are reload's fixed constants. These handlers add
no audit event or log line of their own. This is not a promise of no logging:
ordinary request records and metrics, authentication-denial events (which carry
the caller's own credential ID, never a name from the bindings file) and the
generic `internal_error` line of a 500 still apply, and non-disclosure alone
does not prove an absence of logging.

Each REST request uses one grants version, including its blocking and commit
workers. New grants are served only after all changed records are durable on
a writable database. Until older requests finish, reads and raw writes
admitted under old grants may complete; a memory write reaching the write lock
after its binding's record advances fails closed with 409
`memory_capability_unavailable`, feature `binding_generation`.

`drained: true` means no request or worker using older grants is still running
at return time. `drained: false` means the change is applied, but the
10-second wait ended first; send the same request again to wait again.
No in-flight request is cancelled.

After its owned task starts, the reload continues through apply, bounded
drain and success reporting even if the caller disconnects. The
`memory_bindings_reloaded` success event contains only counts, the drain
result and outcome; its persistent audit append is best effort. Refused or
failed reloads do not emit that success event. The process-local request
ring may omit cancelled HTTP requests, and this event does not name an actor.

A failed reload is not a revocation. If a record write fails after earlier
records committed, served grants stay unchanged but records are partially
advanced: memory writes on those bindings fail closed, while reads and raw
access keep the old grants. Retry with the same file or restart with it to
converge; storage failures may require reopening the database first.
`POST /v1/admin/credentials/{id}/revoke` remains the immediate emergency
control for new requests, independent of grants reload. Revoking a grant
does not erase its memory.

Without raw access on the owning binding, a raw request naming its graph or
collection answers as if that resource were missing, using the ordinary
`404 collection_not_found` or `409 graph_not_found` response. Ordinary grant
checks still run first: an out-of-scope name retains `403 forbidden_scope`.
Retrieve evidence remains redacted when outside ordinary grants; an explicitly
named reserved evidence collection instead answers like a missing collection.
Creating a resource under a reserved name answers like an occupied ordinary
name (`409 collection_exists` or `409 graph_exists`); that collision cannot be
hidden. A credential's attempt on a reserved name is the one thing its holder
cannot see. The server audits it but exports no count of it: at most once a
minute for each credential and route it appends a `reserved_hidden_attempt`
entry (credential ID, method, route template and request ID, never the name)
to the persistent audit log, which only the administrator reads. The limiter
that spaces the entries is shared with credential denials and tracks at most
4,096 credential and route pairs within a minute, and an attempt that finds it
full leaves no entry. The append is synchronous, so
the first attempt per credential, route and minute can answer slightly slower
than a request for a missing name. A reserved name in a retrieve request's
`evidence.collections` is hidden the same way but is not audited. The per-node
attachment check on `delete_node` still requires raw
access to each reserved evidence collection attached to that node.
Provision raw-holder credentials with `collections:["*"]` before granting raw
access in the bindings file; credential creation treats a named reserved
collection as missing. A memory agent's credential needs the same care: every
memory route requires the credential to name the binding's evidence collection
(or hold `["*"]`), plus `graph_read` or `graph_write` and its route's evidence operations ([agent memory guide](MEMORY.md#set-up-a-binding)), and once the bindings file
is in place a request to create a credential that names that collection answers
`404`. Create such credentials before you configure the binding (they keep
working, with the early-grant limit below), or give them `["*"]`. A root
credential with `["*"]` and `read` also reaches every other collection and the
global change feed, so keep it for trusted agents; a tenant credential's
wildcard stays inside its tenant.

**Known limits.** A tenant that owns a binding cannot be deleted and answers
`409 tenant_not_empty` even when no visible collection remains. Sequence numbers are
database-wide: graph `view_sequence` and `revision`, change-feed `head` and
memory receipt `sequence` and recall `meta.sequence` advance for hidden writes, so a caller can count the
activity of other tenants and of memory, but not read it. In a filtered change
feed, `has_more`, `next_cursor`, `head` and gaps in the `sequence` and
`ordinal` of the remaining events show the same thing. The sequence is the
commit sequence of the one log that the memory contract defines the receipt
`sequence` to be, so it has no per-binding form: use separate database files for
tenants that must not observe each other's write volume. Idempotency keys are
scoped to the binding and the memory scope, not to the credential. Credentials
that may write the same scope are peers: one can replay another's request by
repeating its key and exact content, and can read its receipt with the key
alone. Give separate scopes to credentials that must not observe each other.
A read scope (`read_scopes`) is not a peer: it allows only `recall` and
`facts/query`, so its holder cannot replay, look up a receipt or read erasure
status in that scope, and every other route answers the same 403 as for an
ungranted scope. A later write that commits deletes receipts that expired at least 5 minutes
earlier on the server's clock from the current graph; that is not erasure: the bytes stay in the log
until compaction and in earlier backups; no replica receives the deletes (replication export is refused while a binding is configured); and they appear in the change
feed and webhooks only to holders of `raw_access` on the binding. Keep the server clock monotonic: a step forward past a receipt's expiry plus 5 minutes, and then back, lets a retry inside its original window run again instead of replaying.
A reservation outlives the file: a graph that holds a binding record stays
reserved when its entry is retired and removed (only after its receipt window
has ended) and when the whole file is removed. To give such memory up, keep the
entry retired with a `raw_access` grant, delete the graph's nodes (the record
node `binding:v1` included) with that credential, and drop the graph. A raw
credential can delete the record at any time (the raw graph route refuses to
create or change it): every memory write then answers `409
memory_capability_unavailable` with feature `binding_generation` until the next
start, which writes it again, and a start refuses a record that is not exactly a
record (a corrupt record is never repaired). Startup refuses a graph or
collection that holds ordinary data: a graph is adopted when it is empty and its
evidence collection holds no records, when it holds the binding's record, or,
once, when a 0.7.0 server adopted it and it holds nothing but memory nodes. One
node with the `VliteMemoryScope` label no longer satisfies that check, and from
0.7.1 the raw graph batch route refuses, on every graph and for every caller, to
create or replace a node whose label starts with `VliteMemory` or whose key
starts with `scope:v1:`, `mem:v1:`, `receipt:v1:`, `receipt-outcome:v1:` or
`content:v1:` or is exactly `binding:v1`, to patch a node with such a key, or to
add such a label to a node (`422 invalid_input`, the whole batch; deleting such a
node and removing such a label from a node with an ordinary key stay allowed). A
raw caller that
wrote those names under 0.7.0 now gets 422. Nodes planted by an earlier release
are not detected, and a forgery made only of well-formed memory nodes is
indistinguishable from memory, so bind only resources that were created for
memory. A graph's declared schema is not part of that check and survives
adoption: a schema that requires a property of a memory label would make memory
writes fail, so read `GET /v1/graphs/{graph}/schema` before binding a graph you
did not create yourself. The administrator's HTTP request log and persistent audit
show reserved names and record IDs, and collection list `id` values have gaps
where reserved collections sit; both are accepted: the administrator can see
everything, so keep the log and audit directories private, and collection IDs
are never renumbered, because clients cache them. `raw_access` covers every
scope of its binding's resources, not one memory scope, which is why a grant
with read-only scopes cannot hold it. It is also what lets a
`remember` item name an existing evidence record (an evidence reference, which
claims a record no memory owns): without it that request answers 403 before any
record is looked at, so a credential that holds only memory grants cannot claim
records by guessing their IDs. While any resource is reserved, the backup routes
(list, create, verify, drill and schedule status)
answer `403 forbidden_scope` to every credential, whatever its operations:
backups hold reserved data, and so do their database-wide counts, so they are
the administrator's alone. A raw credential
can attach reserved evidence records to nodes of an unreserved graph, and other
users of that graph can then neither delete nor detach those nodes. A reserved
graph takes only its own binding's evidence collection through the public
routes: attaching any other record answers `409 graph_record_not_attachable`,
raw holders included, so an ordinary record cannot be pinned by a reserved
graph (records attached by an earlier version stay attached until the holder
detaches them). A credential created before a binding,
whose grant already names a reserved collection, answers 404 for that name while
a missing name outside its grant answers 403, because the grant check runs
first; create such credentials after the bindings file is in place. Webhook
subscriptions and credential graph lists that name a reserved resource begin to
match it once the reservation ends (the graph is dropped, or its record is
deleted and the file no longer names it); removing the file alone does not end
it. Startup refuses `default` as an evidence
collection, so a binding never changes the legacy routes. Live vector quotas
count reserved records, so a caller with a quota sees its headroom change when
memory writes. The administrator's tenant list and usage report list and count
the reserved collections too, so their headroom is what the quota enforces; no
credential can read those reports. A binding's receipt window, graph and
evidence names never change once memory has been written, and its generation
must rise whenever its grants or state change: the file is checked against the
binding record at each start
(see [Agent memory](API.md#start-the-server-with-memory)).
A symbolic link as the bindings file is followed, as for the token files
(Kubernetes mounts Secret and ConfigMap keys that way), and what it reaches
must be a regular file. The file decides which resources are reserved, so keep
its directory, and the directory of the link's target, writable only by the
operator.

These checks apply only at the server boundary. Direct embedded/file access and
operator-managed backups are outside this authorization policy and must be
protected separately.

## Cloud backups (Azure Blob)

Cloud backup routes (`/v1/admin/cloud-backup*`) need a super administrator.
Read-only, scoped and tenant credentials and non-super-admin accounts get 403
before any work, including pin creation and release. The persisted policy in
`<admin>/cloud-backup/state.json` has no credential field and unknown fields
are rejected, so a key or connection string cannot be saved there.

- **Managed identity** is the default. Tokens are requested only for the
  configured account's Blob endpoint, are never logged or persisted, and an
  expiry that is missing or unreadable is refused rather than assumed. HTTP
  redirects are not followed, and non-loopback endpoints must use HTTPS.
- **Least privilege.** Grant the identity data-plane access to the data and
  coordination containers only, not the account or subscription. No
  management-plane role is needed.
- **Emulator shared key.** `emulator_shared_key` works only with a loopback
  endpoint. The key comes from `VLITE_CLOUD_BACKUP_SHARED_KEY_FILE` (or
  `--shared-key-file`), must be a small private file and is read per process.
  It is never written to the policy, status or audit log.
- **What leaves the host.** A set holds the database snapshot and a recovery
  bundle with tenants, account names and roles, the credential inventory and
  the names of secrets to provision. Password and credential hashes,
  sessions, tokens, webhook URLs and secrets, TLS material, the audit log,
  provider configuration, filesystem paths and host identity are excluded.
  The database itself is not encrypted by VLite before upload; rely on Azure
  Storage encryption and access control, and keep encrypted databases off
  this path (they are refused). The credential inventory has a fixed shape
  (ID, name, tenant, scope, state and expiry) and parsing refuses any other
  field, so a secret cannot ride along. Only files the server writes may sit
  in the admin directory; anything else (a token or TLS file, for example)
  fails the backup with `recovery_config_unrecognized` until it is moved.
- **Restore grants nothing silently.** A restore creates a new lineage, so old
  scoped credentials and sessions stop working. The offline `cloud-restore`
  imports accounts disabled with unusable passwords and creates one fresh
  super admin from a password file you supply. It builds the database and
  admin stores in a private staging directory (owner-only directories and
  files on Linux), verifies that only the fresh admin can sign in, and
  publishes them with one rename into a new `--output` directory that must
  not exist; it never writes into an existing database or admin directory.
  On Windows the staging directory inherits the parent's access list, so
  choose a parent writable only by the operator.
- **Pins.** A restore or drill pins its set so retention cannot delete it
  mid-download. Only the owning operation releases that pin; an operator can
  force-release one left by a crashed owner, which is audited separately.
  Pin IDs are not secrets, and anyone with write access to the coordination
  container can still change set records, so keep that access to the backup
  identity.
- **Audit.** Policy changes, runs, publication outcomes, drills, collections,
  deletions, restores and pin changes are written to the audit log.

Azure immutability policies, Microsoft Entra role assignments and managed
identity have been exercised only against fakes and a local Azurite emulator
in this release, not against real Azure.

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

**gRPC disk ANN sidecar path.** `BuildDiskAnn` and `SearchDiskAnn` take a
sidecar `path` from the administrator token only, an operator-only choice. Every
other caller, a scoped credential or the read-only token, must leave `path`
empty: the server then uses the file the REST routes use,
`disk-ann-<collection id>.dann` in `VLITE_ADMIN_DIR`, and a non-empty `path` is
refused with `INVALID_ARGUMENT` (`422 invalid_input`). Before 0.7.1 a credential
scoped to one collection could name any path, so it could write or read any
file the server process reaches. Clients that sent a path with a scoped
credential must drop it. The sidecar holds plaintext vectors, so keep its
directory private (see [gRPC](GRPC.md)).

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
| `read_only_token` | The read-only token. Of the routes that write these events, it reaches only the four memory read routes (recall, receipt lookup, facts query, erasure get), where the handler refuses it and audits the refusal as a `memory_request`; none of the other events below shows this form. |
| The credential ID | A scoped API key. Its 12 characters from `a-z2-7` cannot equal another form. |

These events carry an `actor`: `kv_put`, `kv_delete`, `kv_batch`, `memory_request`,
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
keeps `credential_id` and adds an `actor` that equals it.

**Changed in 0.7.1.** The `actor` of `collection_created`, `collection_dropped`
and `collection_settings_changed` was `admin` for the bootstrap token and for
every super-admin session; it is now `admin_token` or `user:<account id>`, and
the credential ID is as before. A query, alert or export that matches these
events with `actor` equal to `admin` must also match the two new forms.
`kv_put`, `kv_delete`, `memory_request` and `credential_denied` gain an `actor`;
their `principal` and `credential_id` are unchanged. The audit log is only
appended to, so lines written by an earlier version keep their form: collection
events with `actor` `admin`, the others with no `actor`. After a rollback to
0.7.0 both forms sit in one log, and the 0.7.0 server reads such a log without
error. Read both forms of a log that spans an upgrade or a rollback.

## Key-value permissions

Collection KV GET/HEAD and list need the collection's `read` scope; PUT and
DELETE need `write`. The multi-get route (`POST …/kv/entries/get`) needs `read`,
and the batch route (`POST …/kv/batch`) needs `write`. These scopes also apply
to `vlite.v1.Kv`: `GetKvEntry`/`ListKvEntries`/`GetKvEntries` need `read`, and
`PutKvEntry`/`DeleteKvEntry`/`WriteKvBatch` need `write`. Graph scopes grant
none of them. Read-only credentials cannot mutate: the read-only token may call
multi-get but not batch. Tenant-local names and immutable
lineage/collection bindings are checked before collection lookup;
a binding for a dropped collection does not authorize its replacement.
KV never implicitly creates a collection and shares ordinary record quotas.
Mutation audit entries contain operation, principal, [actor](#audit-actors),
collection, request ID, outcome and, for a put, its expiry deadline, never
namespace, key or value. A failed request records the collection as `(invalid)`
and still records the actor. A batch writes one `kv_batch` event, not one per
operation: it records the request ID, principal, actor, collection, outcome and
duration (`duration_ns`) like the other key-value events, plus the counts `puts`
and `deletes`. It has no expiry deadline and never records a namespace, key or
value.

A namespace is an application partition **inside** an authorized collection,
not an authorization boundary. Put sensitive KV data in dedicated collections
and bind credentials narrowly: vector, graph-attachment and import permissions
can also expose, overwrite, delete or expire ordinary KV records. A principal
that can write the collection can also fill a key's 16 record IDs with ordinary
records, so that puts for that key fail with `kv_slots_exhausted` (409 on REST,
`FAILED_PRECONDITION` on gRPC), and a batch that holds such a put fails as a
whole. A principal that can write but not read can learn two things about a key
that it may already overwrite, and never its value. Whether the key is live: a
delete with `absent` answers 412 for a live key and succeeds with
`deleted: false` for a missing one, and changes no live entry either way. And
whether a guessed commit sequence is its current revision: a refused write
changes nothing, so the principal can test that condition without changing the
entry whenever it can make the request fail in a way that differs from the
failed condition (the answer is 412 when the guess is wrong and that other
failure when it is right). A single put does this when it would leave the scope
over a live-record quota (429 `quota_exceeded`), and a single delete on a record
that is attached to a graph node (409 `graph_attached_record`). A batch widens
the class, because it can add the failure on any other key after the operation
under test: a put of a key whose 16 record IDs the principal has filled (409
`kv_slots_exhausted`), or either failure above, or, on a file of format 1, the
409 `upgrade_required` that any batch which writes gets. Each such
`kv_slots_exhausted` answer counts in `kv_slots_exhausted_total`. Use
dedicated collections for key-value data,
grant `write` on them only to principals you trust with their keys, and watch
`kv_slots_exhausted_total`: it counts each REST or gRPC request answered this
way, across all credentials (a refused batch counts once; a REST request whose
client disconnected before the answer was produced is not counted, as with
every REST counter; a gRPC call is counted even if its client cancels), and a
rise nobody expects points at records planted over a key's IDs. The failed
`kv_put` or `kv_batch` audit event names the caller
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
