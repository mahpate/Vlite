# REST API reference

The server bundles its current machine-readable contract at `/openapi.json`.
Protected routes take a bearer credential and apply the deny-by-default
scoped-credential policy; the dashboard sends its session cookie instead.
Collection routes keep their collection-bound authorization checks. Errors use
the `{"error":{"code":"...","message":"..."}}` envelope.

The [website API browser](../site/api.html) provides the same searchable
reference for REST, Node.js, Rust, C ABI, Python, gRPC, MCP and WebAssembly.
Choose an interface and area, then select a call for its signature, behavior,
access requirements and a permanent link. The full reference remains available
without JavaScript. HTTP coverage is checked against registered server routes,
not only the OpenAPI snapshot; dashboard asset routes are included separately.

## HNSW quality

`POST /v1/index/hnsw/quality` measures the default collection's ready index.
`POST /v1/collections/{collection}/index/hnsw/quality` measures a named
collection's index. Both require the `index` scope for that collection and use
the same optional JSON body:

```json
{"samples": 16, "k": 5, "ef_search": 64, "seed": "42", "max_distance_evaluations": 100000}
```

Omitted fields use the server defaults. Optional `metric`, `config`,
`quantization` and `rerank_multiplier` select the index identity, as on the
search and build routes. The response contains `recall_at_k`, `samples_used`,
`distance_evaluations` and `skipped_reason`; inspect the latter when recall is
null: `budget` means the evaluation budget was exceeded; `empty` means no usable
samples. This call compares ANN results with exact search; it does not build an
index. `samples` is 1 to 32 (default 8), and `k` is 1 to 100 (default 10).

## Dashboard sign-in and accounts

See [SECURITY.md](SECURITY.md#dashboard-sign-in-and-accounts) for roles,
passwords and sessions.

| Route | Caller | Purpose |
|---|---|---|
| `GET /v1/auth/config` | Public | Sign-in configuration |
| `POST /v1/auth/login` | Public | Sign in with a username and password |
| `GET /v1/auth/session` | Signed in | Current session |
| `POST /v1/auth/logout` | Signed in | End the session |
| `POST /v1/auth/password` | Signed in | Change your own password |
| `GET /v1/admin/users` | Super admin | List accounts |
| `POST /v1/admin/users` | Super admin | Create an account |
| `PUT /v1/admin/users/{id}` | Super admin | Update an account |
| `DELETE /v1/admin/users/{id}` | Super admin | Delete an account |
| `POST /v1/admin/users/{id}/password` | Super admin | Set a temporary password |

## Record expiry and metadata

`PUT /v1/vectors/{id}` and its collection-scoped equivalent accept optional
`expires_at_ms` (absolute Unix milliseconds) or `ttl_ms`/`ttl` (relative
milliseconds); the two forms are mutually exclusive and need format v5.
Expiry is checked at read/search time and does not create a separate feed event.
`PATCH /v1/vectors/{id}/metadata` and
`PATCH /v1/collections/{collection}/vectors/{id}/metadata` accept
`{"metadata":{...}}` and leave vector/payload bytes unchanged. An optional
`If-Match` is checked under the database owner lock.

## Key-value entries

Ordinary KV uses an **existing one-dimension collection**; these routes never
create a collection. Use dedicated collections: ordinary vector, graph and
import operations can overwrite KV records. A namespace partitions application
keys, not permissions.

| Route | Scope | Result (HTTP 200) |
|---|---|---|
| `GET /v1/collections/{collection}/kv/entry?namespace=n&key=k` | `read` | `key`, `value_base64`, `version`, `expires_at_ms` |
| `PUT /v1/collections/{collection}/kv/entry` | `write` | `version`, `expires_at_ms` |
| `DELETE /v1/collections/{collection}/kv/entry?namespace=n&key=k` | `write` | `deleted` |
| `GET /v1/collections/{collection}/kv/entries?namespace=n` | `read` | `items`, `next_after` |

A put body is `{"namespace":"n","key":"k","value_base64":"YQ=="}`.
Optional `expected` is `"any"` (default), `"absent"`, or
`{"revision":"42"}`; it is checked atomically with the write. DELETE takes
`expected=any|absent|<revision>` in the query. A revision mismatch returns
412 `precondition_failed`, including a missing/due key. On REST and gRPC its
message is always `revision precondition failed`, so a caller with `write` but
not `read` cannot learn whether a key exists or what its revision is. An
unconditional or absence-conditional delete of a missing key succeeds with
`deleted:false`.

Versions and deadlines are canonical uint64 **decimal strings**, without signs
or leading zeroes except `"0"` (maximum `"18446744073709551615"`). Expected
revision `"0"` is valid but cannot match a live entry. Optional
`expires_at_ms` is an absolute Unix-millisecond string or null; omitted/null
clears expiry. Already-due puts succeed, then read absent. Expiry needs v5.
Namespaces are 1..255 UTF-8 bytes and keys 1..1,024; strings are not normalized.
Values are canonical padded standard base64, at most **1,047,290 decoded bytes**
(1,396,388 encoded characters); an empty value is valid. Unknown/duplicate JSON
fields and wrong types are rejected with 422 `invalid_json`; parsed bound or
canonical-encoding violations return 422 `invalid_input`, including a list
`limit` that is empty, malformed or outside 1..10,000. Unknown/duplicate
query parameters return 400 `invalid_query`. Query decoding uses form
semantics: `+` means a space, and a literal plus must be encoded as `%2B`. The
existing 8 MiB body cap applies. `HEAD` on the two GET routes behaves like `GET`
without a body and needs `read`.

List `prefix` defaults to empty (at most 1,024 UTF-8 bytes), `after` is omitted
or a nonempty key, and `limit` is 1..10,000 (default 100). Keys sort by unsigned
UTF-8 bytes; `after` is exclusive. A page is the largest prefix fitting both
`limit` and **16,777,216 bytes of compact canonical JSON**, including the
`{"items":[...],"next_after":...}` envelope. `next_after` is the last returned
key only when another live matching entry exists at that page's clock;
otherwise it is null, including empty pages. One owner guard and expiry clock
cover a page; no snapshot spans pages. Each key appears once, as `GET` returns
it; look-alike records that ordinary routes left elsewhere are ignored.

Missing/due reads return 404 `kv_not_found`; a collection with other dimensions
returns 409 `not_kv_collection`. Sixteen occupied collision-probe slots return
409 `kv_slots_exhausted`; expiry before v5 returns 409 `upgrade_required`.
Operation scope, tenant and immutable collection bindings apply before lookup.
Ordinary record quotas also count KV entries. Mutations are audited without
keys, namespaces or values. The audit covers requests that reach the store,
including a failed compare-and-set and a missing or non-KV collection (logged
as `(invalid)`); a request rejected while parsing is not audited, only logged
without its address or content. Read-only handles refuse mutations.
`GET /v1/capabilities` reports `features.kv: true` for format 5 files; a
format 4 file still serves ordinary KV without expiry although the flag is false.

Ordinary KV follows configured durability: `full` syncs before acknowledgment,
`periodic` can lose up to a flush interval on OS crash/power loss, and `none` is
memory-only. Successful mutations are acknowledged after commit. There is no
per-call forced sync, automatic retry or lease guarantee; disconnects and
`commit_indeterminate` do not prove a write failed. Lease operations are not
exposed. The local `vlite_kv_*` MCP tools and `vlite-shell` `kv-*` commands work
on a local file only ([MCP](MCP.md#local-key-value-tools)); the Python,
JavaScript and .NET clients wrap these REST routes
([clients](../clients/README.md)).

## Key-value gRPC service

`vlite.v1.Kv` adds four unary RPCs alongside the 35 `VLite` RPCs (39 total):
`GetKvEntry`, `PutKvEntry`, `DeleteKvEntry`, and `ListKvEntries`. Generate clients
from `proto/vlite/v1/vlite.proto`. Each address requires an explicit nonempty
`collection`; unlike legacy vector RPCs, empty does not mean `default`.
Get/list require collection `read`, and put/delete require collection `write`.

The [KV rules above](#key-value-entries) also apply to gRPC, including byte
bounds, CAS, expiry, tenant bindings, quotas, audit and the canonical-JSON page
boundary. Protobuf values are raw `bytes`; versions and deadlines are `uint64`.
Unset `Expected` (or its oneof) means any. Unset put expiry clears TTL; an
explicit zero is a due deadline, not absence. List `after`, `limit` and
`next_after` preserve optional presence.

Errors carry `vlite-error-code` and `x-request-id`: missing entries use
`NOT_FOUND`, invalid input `INVALID_ARGUMENT`, CAS mismatch `ABORTED`,
collection/probe/upgrade conflicts `FAILED_PRECONDITION`, permission failures
`PERMISSION_DENIED`, and quotas `RESOURCE_EXHAUSTED`. Every service applies
8 MiB request and 16 MiB response caps; tonic size errors are `OUT_OF_RANGE`
without an application error trailer. No lease RPCs are provided.

## Agent memory (REST)

A binding is an explicitly provisioned local name for scoped memory, not a
graph or collection path. All nine routes are unary `POST` requests beneath
`/v1/memory/{binding}`; no GET, HEAD or DELETE aliases are provided.

| Suffix | First-slice status |
|---|---|
| `/remember` | Entity and episodic items; 200 |
| `/recall` | Bounded retained-memory recall; 200 |
| `/facts/query` | 409 `memory_capability_unavailable` |
| `/facts/correct` | 409 `memory_capability_unavailable` |
| `/consolidate` | C1: one retained source summary; 200 |
| `/forget` | IDs with `logical_delete`; 200, not secure erasure |
| `/receipts/lookup` | Retained receipts; remember lookup returns 409 `memory_capability_unavailable`, `details.feature=remember_receipt_lookup` |
| `/erasures/get` | 409 `memory_capability_unavailable` |
| `/erasures/resume` | 409 `memory_capability_unavailable` |

Authenticate with a bearer credential authorized for the operation and binding:
exact startup scope grants plus `graph_read`/`graph_write` and the required
evidence-collection read/search/write permissions. Mutation dry runs still need
write grants. Memory grants never confer raw graph or collection access.
Unknown and ungranted bindings return the same 403 before existence lookup.
Provision raw-holder credentials with `collections:["*"]`; named reserved
collections return the same 404 as missing collections during credential creation.

Requests are limited to 8 MiB; REST responses have no serving size cap.
Mutations require a body-only `idempotency_key`. A retained matching retry
returns the original result with `replayed:true`; a changed digest conflicts.
Bindings explicitly configure a receipt window; expiry ends replay guarantees,
not memory retention. After an indeterminate commit, reopen and reconcile the
receipt rather than blindly retrying. Full schemas retain future capabilities,
including 202 pending forget/resume results; these do not imply secure erasure.

Stable errors use `{error:{code,message,details?}}`: 400/413/415 `invalid_json`
(malformed JSON, duplicate keys, an oversize body or the wrong media type); 401
`unauthorized`; 403
`forbidden_scope`/`read_only`/`tenant_suspended`/`tenant_unavailable`; 404
`memory_not_found`/`receipt_not_found`/`erasure_not_found`; 409
`memory_not_configured`/`memory_capability_unavailable`/`memory_feed_unsupported`/
`idempotency_conflict`/`memory_slot_conflict`/`memory_attachment_conflict`/
`memory_deletion_pending`/`memory_job_reconciliation_required`/`index_not_ready`;
412 `precondition_failed`; 422 `invalid_input` (a wrong type or shape, an
unknown or missing field, or an out-of-range value; `details.field` names it)/
`memory_unsupported_ranking`/`memory_bounded_plan_unsupported`; 429
`rate_limited`/`quota_exceeded`
(`Retry-After: 1`); 503 `busy`/`unavailable`/`poisoned`/`commit_indeterminate`;
504 `deadline_exceeded`; 500 `internal_error`. Missing capabilities name
`details.feature`. `response_too_large` (413) applies to gRPC preflight and
MCP/CLI response bounds, not a REST response cap. See the
[memory contract](projects/roadmap/serving-memory-contract.md#6-authorization-errors-and-observability)
for authorization, limits and complete semantics.

### First-slice limits

A request that uses something this release does not serve is refused, never
ignored or partly applied: 409 `memory_capability_unavailable` with
`details.feature`, after authorization and before validation (a read-only node
answers 403 `read_only` first).

| Route | Refused (`details.feature`) |
|---|---|
| `remember` | `fact_kinds` (fact and relation items), `confidence`, `metadata`, `assertion`, `evidence` (a nonempty list), `reference_revision_pin` (a pinned reference revision), `expected` (unless `merge` is `merge_provenance`) |
| `recall` | `embedding_provider`, `fact_kinds`, `filter`, `temporal_read` (`as_of_ms`), `paths`, `metadata` (projections), `evidence_recall` (text or vector without an evidence collection), `query_text_length`; `require_complete_feed: true` answers 409 `memory_feed_unsupported` |
| `consolidate` | `consolidate_plan` (`dry_run`), `consolidate_delete` (`logical_delete`), `fact_kinds`, `confidence`, `references`, `evidence`, `metadata`, `assertion`, `expected`, `merge` |
| `forget` | `forget_target` (series or entity), `erasure_jobs` (session or scope target, `allow_job`), `forget_expire` (`mode: expire`) |
| `receipts/lookup` | `remember_receipt_lookup` |
| `facts/*`, `erasures/*` | `facts`, `erasure_jobs` |

Recall serves exact, bounded retrieval. `ranking.selection` `exact` or `mmr`
works on a vector alone; combined with text, entity seeds, other weights or a
half-life it answers 422 `memory_unsupported_ranking`. Vector recall measures
squared Euclidean distance, not cosine: `exact` orders candidates by it, and
every selection reports it in `scores.distance`, including `mmr`, which selects
by cosine and which the memory contract says retains cosine distance. Vectors
that are not unit length can therefore rank differently from a cosine ranking.
`mode: approximate`
answers 422 `memory_bounded_plan_unsupported` because no approximate index is
built for a binding, and so does `allow_partial: true` when a budget is
exhausted: partial results are not served.

A binding's identity is read from the bindings file at startup and is not
stored in the database. Do not change a binding's receipt window, generation
or stored names once memory has been written: after a restart with a changed
window, a retry of a request that is still inside its original window answers
409 `idempotency_conflict` instead of replaying.

### Start the server with memory

Memory is off until the server reads a private bindings file
(`--memory-bindings-file PATH` or `VLITE_MEMORY_BINDINGS_FILE`). Without it
there are no memory routes and no reservation, and no other route changes.
Create the graph (on a database with graph support) and the evidence
collection first, through the ordinary admin routes. Startup refuses to run
when either is missing, when the database has no graph support, or when a
resource already holds ordinary data, so bind only resources made for memory.
On a writable primary, startup then creates every active binding's granted
scopes. A read-only node or replica checks the file, writes nothing and refuses
memory writes with 403 `read_only`.

The file is strict JSON of at most 1 MiB. An unknown or duplicate key, a `null`
or a missing field rejects the whole file, and the startup error never repeats
its content.

```json
{"version": "memory-bindings-v1", "bindings": [{
  "binding": "notes", "tenant": "@root", "generation": 1, "state": "active",
  "graph": "notes_graph", "evidence_collection": "notes_ev",
  "schema_version": "memory-schema-v1", "ranking_version": "memory-ranking-v1",
  "digest_version": "memory-request-v1", "receipt_window_ms": 86400000,
  "grants": [{"principal": "abcdefghij27", "scopes": [{"agent": "a"}]}]
}]}
```

- `binding` is the name in the URL (`^[a-z][a-z0-9_-]{0,62}$`). `tenant` is
  `@root` or a tenant ID. `graph` and `evidence_collection` (one name, or
  `null`) are stored names that one binding owns; `default`, the collection of
  the legacy routes, cannot be an evidence collection. Without an evidence
  collection, text and vector recall answer 409 `memory_capability_unavailable`
  with `details.feature` `evidence_recall`; with one, text recall also needs
  that collection's text index to be ready (409 `index_not_ready` otherwise).
- `receipt_window_ms` (1 to 2,592,000,000) is how long a retry returns its
  original result.
- A grant names a credential ID (12 characters, never a token) and the exact
  scopes it may use: `agent`, optionally `user`, and `session` only with a
  `user`. A credential without a grant has no scope, an administrator
  included. `"raw_access": true` on a grant also lets that credential reach the
  bound graph and collection through the raw routes, with its ordinary
  permissions.
- `"state": "retired"` with `retired_at_ms` keeps `receipts/lookup` working and
  answers every other route 409 `memory_capability_unavailable` with
  `details.feature` `binding_retired`.
- At most 256 bindings, 1,024 grants per binding and 256 scopes per grant.

While any binding is configured the gRPC listener, automatic index
maintenance, imports, replication export and a few aggregate admin routes are
disabled, and the bound resources are hidden from the raw routes; see
[Memory-bound resources](SECURITY.md#memory-bound-resources). To roll back,
restart without the file: the routes and the reservation end at once, and the
data stays in the database as an ordinary graph and collection.

## Collection retrieval

* `POST /v1/collections/{collection}/search/grouped` takes `vector`, `field`,
  `groups`, `hits_per_group`, and optional `filter`/`metric`.
* `POST .../recommend` takes positive/negative decimal-string record IDs and/or
  example vectors, `k`, optional filter and metric.
* `POST .../facets` and `POST .../stats/numeric` take `field` and optional filter.
* `POST .../search/geo` takes vector, location field, center coordinates, radius,
  result limit and optional vector/geographic sort.
* `POST .../vectors/configure` declares a named vector. Named vector records use
  `PUT .../vectors/{id}/named`; retrieval is at `POST .../search/named`.
  Sparse and dense+sparse retrieval are at `POST .../search/sparse` and
  `POST .../search/dense-sparse`.
* `POST .../query` runs the bounded read-only grammar documented in
  [VECTORS.md](VECTORS.md).
* `POST .../embed` returns a vector sized to the collection. The built-in
  `hashing` provider is deterministic and lexical, not semantic. With the
  `http-embedder` build feature, `provider: "http"` uses the explicitly
  configured `VLITE_EMBEDDING_URL`, `VLITE_EMBEDDING_MODEL`, and optional
  `VLITE_EMBEDDING_KEY`; request text is sent to that configured endpoint.
* `POST /v1/search` and collection search accept `rerank: {mode: "exact"}` or
  `{mode: "mmr", lambda: 0.5, candidates: 50}`. Reranking is limited to 1,000
  candidates and vector search; candidates defaults to `max(k, 50)`.

These APIs require format V5 where applicable. Numeric IDs in request paths and
result objects are canonical decimal strings. Named vectors/sparse vector
contents are replaced as one atomic record-vector operation.

## Change feed, replicas, and admin

`GET /v1/changes?cursor=<token>&limit=1000` returns committed events, a durable
`next_cursor`, the observed head and `has_more`. The initial request without a
cursor starts at the beginning of retained history. Commits are never split;
`limit` is an approximate maximum. Cursor problems are client errors, never
HTTP 500: a damaged token answers 422 `invalid_cursor`; a token taken before a
compaction, restore or upgrade answers 409 `cursor_compacted` and the consumer
must resync from a fresh snapshot; a cursor (or replication `after_sequence`)
past the head of the log answers 409 `cursor_ahead`; a cursor (or
`after_sequence`) below a checkpoint's history floor, including a first
request without a cursor, answers 409 `cursor_before_history` and names the
floor in its message. Unlike `cursor_compacted`, the lineage and every later
cursor stay valid. The consumer takes a fresh snapshot and resumes from a
cursor at or above the floor. The change feed and
replication export need a format V4 or
newer file: on a V1–V3 file both routes answer HTTP 409 `requires_format_v4`
(`features.change_feed` in `GET /v1/capabilities` tells which). The server
never upgrades a file automatically. Since v0.6.0 graph-enabled files serve the
feed too: graph commits add `graph_created`, `graph_dropped` and `graph_batch`
events (with `collection: ""`), see
[Change feed and webhooks](GRAPH.md#change-feed-and-webhooks).

`GET /v1/admin/replication/export?after_sequence=0&max_bytes=1048576` exports a
whole-commit, checksummed log segment as base64. This is a shipping primitive,
not a network follower service; callers must deliver the segment to
`ReplicaFollower` out of band. `POST /v1/admin/restores` creates a new copy from
an existing admin backup. Optional `sequence` or `timestamp_ms` selects a PITR
target. A `sequence` beyond the end of the backup's log answers 422
`invalid_restore_target`; `timestamp_ms` on a file created without commit
timestamps answers 409 `timestamps_unavailable` (restore by `sequence`
instead). A restore never replaces the active database; select it for the next
startup through the existing admin selection route.

`POST /v1/collections/{collection}/index/disk-ann` builds the collection's
rebuildable sidecar under `VLITE_ADMIN_DIR`; `POST .../search/disk-ann` requires
a fresh sidecar: with none built it answers 409 `disk_ann_not_built`, and after
any later commit 409 `stale_index` (build it again). Disk ANN sidecars are
unavailable for encrypted databases (409 `sidecar_unavailable`).

`GET /v1/database` reports `is_encrypted` and a decimal-string
`encryption_key_id`; secret key material is never returned. The server accepts
`--encryption-key-file` (exactly 32 raw bytes, or a passphrase file),
`VLITE_ENCRYPTION_KEY` for a passphrase, or `VLITE_ENCRYPTION_KEY_HEX` for 64
hexadecimal raw-key digits. Never put the key itself on command lines or logs.
Encrypted creation uses V5. See [SECURITY.md](SECURITY.md).

The bundled OpenAPI file contains the route and request schemas. The HTTP
embedding provider is optional at build time; MMR and exact reranking operate
only on the bounded candidate list returned by the selected search strategy.
