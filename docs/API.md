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

A gateway answers the first five routes itself only with its opt-in
`dashboard` block, for tenant editors and viewers, and does not change
passwords; see
[Dashboard sign-in through a gateway](#dashboard-sign-in-through-a-gateway-od-004-opt-in).

## Record expiry and metadata

`PUT /v1/vectors/{id}` and its collection-scoped equivalent accept optional
`expires_at_ms` (absolute Unix milliseconds) or `ttl_ms`/`ttl` (a lifetime in
milliseconds, at least 1, counted from the moment the write is applied); the
two forms are mutually exclusive and need format v5. An `expires_at_ms` that is
not later than the server clock when the write is applied is refused with 422
`invalid_input`, and so is `ttl_ms` 0; nothing is written, and a record already
under that ID is unchanged. A request that fails for another reason, such as a
file older than v5 (409 `upgrade_required`), a stale `If-Match` (412) or an
invalid vector, answers that error first, except `ttl_ms` 0, a request error
refused before any of these. gRPC `PutVector` applies the same
rule to `expires_at_ms` as `INVALID_ARGUMENT`. Before 0.7.1 the REST refusals
answered 500 `internal_error`, and gRPC answered `OK` and stored a record that
was already expired and could not be read; a client that sent an already due
deadline must now send a later one. The
[key-value routes](#key-value-entries) accept an already due deadline by
contract. Expiry is checked at read/search time and does not create a separate
feed event.
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
| `POST /v1/collections/{collection}/kv/batch` | `write` | `results` |
| `POST /v1/collections/{collection}/kv/entries/get` | `read` | `items`, `next_index` |

A put body is `{"namespace":"n","key":"k","value_base64":"YQ=="}`.
Optional `expected` is `"any"` (default), `"absent"`, or
`{"revision":"42"}`; it is checked atomically with the write. DELETE takes
`expected=any|absent|<revision>` in the query. A revision mismatch returns
412 `precondition_failed`, including a missing/due key. On REST and gRPC its
message is always `revision precondition failed`: it never names the current
revision. An unconditional or absence-conditional delete of a missing key
succeeds with `deleted:false`, so a caller with `write` but not `read` can still
learn whether a key is live: `expected=absent` answers 412 for a live key and
changes no live entry either way.

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

**Batch.** `POST /v1/collections/{collection}/kv/batch` applies puts and
deletes of one collection, all or nothing. The body

```json
{"operations":[
  {"op":"put","namespace":"app","key":"a","value_base64":"AAEC","expected":"absent"},
  {"op":"delete","namespace":"app","key":"b","expected":{"revision":"42"}}]}
```

answers 200 `{"results":[{"version":"43"},{"deleted":true}]}`. `operations` is
required and holds 0 to 1,000 objects. A `put` takes `namespace`, `key`,
`value_base64` and an optional `expected`; a `delete` takes `namespace`, `key`
and the same `expected`. `expected` is `"any"` (default), `"absent"` or
`{"revision":"<u64 decimal>"}`, with the bounds of the single routes. A put has
no `expires_at_ms`: the field is unknown, so 422 `invalid_json`, like any other
unknown or duplicate field. The decoded values of all puts total at most
**4,194,304 bytes**, and no two operations name the same `(namespace, key)`;
either violation is 422 `invalid_input`, and its message names the operation.
`results[i]` answers `operations[i]`: a put gives
`{"version":"<nonzero u64 decimal>"}` and a delete gives `{"deleted":<bool>}`.
An empty batch answers `{"results":[]}`. The 8 MiB body cap applies to the whole
JSON body, so a batch inside every other limit is refused with 413
`invalid_json` when many of its keys are escaped in JSON; use fewer or shorter
keys, because a batch cannot be split without losing atomicity.

The batch behaves as if its operations ran one at a time, in order, against the
live state and one expiry clock, and were then committed once, as one frame: a
crash leaves all of it or none of it, and no reader sees part of it. Every put
of one batch gets the same version, the commit's sequence, so each is a valid
`expected` revision for its key; a put clears the deadline of the entry it
replaces. Each operation fails as its single call does: a failed condition is
412 `precondition_failed` with the same message, sixteen occupied slots are 409
`kv_slots_exhausted`, and a delete of an entry that a graph node is attached to
is 409 `graph_attached_record`. The first operation, in order, that cannot run
fails the whole batch, and nothing is written. A delete of a key with no live
entry succeeds with `deleted:false`, and a batch that changes no record commits
nothing. The record quota is judged once, under the same guard as the commit, on
the count that the whole batch leaves (the net change of all its slot changes,
where the last write to a slot wins), and only when the batch writes at least
one record, so an empty batch or a batch of deletes is never refused for quota,
like a single delete. A batch refused for slots counts once in
`kv_slots_exhausted_total`.

**Multi-get.** `POST /v1/collections/{collection}/kv/entries/get` takes
`{"keys":[{"namespace":"app","key":"a"}]}`: 0 to 1,000 objects with the bounds
of the single routes, and the same key may repeat. It answers 200
`{"items":[<entry>|null],"next_index":null}`. `items` is positional: each
element is the entry object that `GET …/kv/entry` returns (`key`,
`value_base64`, `version`, `expires_at_ms`), or `null` for a key that is absent
or due. One read guard and one expiry clock serve the call. The response is the
largest prefix of the keys whose items fit **16,777,216 bytes of compact JSON**
(the first key always fits), and `next_index` is then the index of the first
key not read, equal to `len(items)` and at least 1; otherwise it is null. No
snapshot spans calls: ask again for the remaining keys until `next_index` is
null. The route needs `read`, so the read-only token may call it.

Missing/due reads return 404 `kv_not_found`; a collection with other dimensions
returns 409 `not_kv_collection`. Sixteen occupied collision-probe slots return
409 `kv_slots_exhausted`; expiry before v5, or a batch that changes a record on a
format 1 file, returns 409 `upgrade_required`.
The batch and multi-get routes answer 403 `forbidden_scope` or `read_only`,
404 `collection_not_found`, 429 and 503 `commit_indeterminate` as the single
routes do. A server without them (0.7.1 and earlier) answers 404 `not_found` to
the admin token and 403 to every other credential, because its permission check
runs before it matches a route; look in `routes` of `GET /v1/capabilities`, as
described below.
Operation scope, tenant and immutable collection bindings apply before lookup.
Ordinary record quotas also count KV entries. Mutations are audited without
keys, namespaces or values; a batch is one `kv_batch` event with the number of
puts and deletes. The audit covers a request once it has been parsed and
validated: a request that then fails (a failed compare-and-set, a 409, 429 or
503, a missing or non-KV collection) records the collection as `(invalid)` and
still records the actor; a request rejected while parsing or validating is not
audited, only logged without its address or content. Read-only handles refuse
mutations.
`GET /v1/capabilities` reports `features.kv: true` for format 5 files; a
format 4 file still serves ordinary KV without expiry although the flag is false.
`features.kv_batch` is true under the same condition. `routes` lists the batch
and multi-get routes on every format of file, and a file before format 5 serves
both routes while the flag is false, so look for the two paths in `routes`, not
at the flag, to learn whether a server has them. A server without them lists
neither and has no `kv_batch` flag.

Ordinary KV follows configured durability: `full` syncs before acknowledgment,
`periodic` can lose up to a flush interval on OS crash/power loss, and `none` is
memory-only. Successful mutations are acknowledged after commit. There is no
per-call forced sync, automatic retry or lease guarantee; disconnects and
`commit_indeterminate` do not prove a write failed. Lease operations are not
exposed. The local `vlite_kv_*` MCP tools and `vlite-shell` `kv-*` commands work
on a local file only ([MCP](MCP.md#local-key-value-tools)); the Python,
JavaScript, .NET and Go clients wrap these REST routes
([clients](../clients/README.md)).

## Key-value gRPC service

`vlite.v1.Kv` is a separate service from `vlite.v1.VLite`. Its unary RPCs are:
`GetKvEntry`, `PutKvEntry`, `DeleteKvEntry`, `ListKvEntries`, `GetKvEntries`
and `WriteKvBatch`. Generate clients from `proto/vlite/v1/vlite.proto`. Each
address requires an explicit nonempty `collection`; unlike legacy vector RPCs,
empty does not mean `default`. Get, list and `GetKvEntries` require collection
`read`; put, delete and `WriteKvBatch` require collection `write`.

The [KV rules above](#key-value-entries) also apply to gRPC, including byte
bounds, CAS, expiry, tenant bindings, quotas, audit and the canonical-JSON page
boundary. Protobuf values are raw `bytes`; versions and deadlines are `uint64`.
Unset `Expected` (or its oneof) means any. Unset put expiry clears TTL; an
explicit zero is a due deadline, not absence. List `after`, `limit` and
`next_after` preserve optional presence.

`GetKvEntries` and `WriteKvBatch` are the multi-get and the batch above, with
raw `bytes` values and `uint64` versions; their messages are in the
[gRPC guide](GRPC.md#key-value-entries). A batch put has no deadline field.
`KvGetManyResponse.next_index` is optional and unset when every key was read.
The limits and error codes are those of the REST routes.

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
evidence-collection read/search/write permissions. A grant's optional
`read_scopes` are accepted only by `recall` and `facts/query`; every other
route needs a scope from `scopes`. Mutation dry runs still need
write grants. Memory grants never confer raw graph or collection access.
Unknown and ungranted bindings return the same 403 before existence lookup.
Provision raw-holder credentials with `collections:["*"]`; named reserved
collections return the same 404 as missing collections during credential creation.

Requests are limited to 8 MiB; REST responses have no serving size cap.
Mutations require a body-only `idempotency_key`. A retained matching retry
returns the original result with `replayed:true`; a changed digest conflicts.
A key is scoped to the binding and the memory scope, not to the credential:
credentials granted the same scope are peers and can replay or look up each
other's receipts with the key; a read scope is not a peer (see
[Memory-bound resources](SECURITY.md#memory-bound-resources)).
Bindings explicitly configure a receipt window; expiry ends replay guarantees,
not memory retention. A later write that commits deletes a bounded number of
receipts that expired at least 5 minutes earlier on the server's clock; lookups answer 404 either way.
After an indeterminate commit, reopen and reconcile the
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
| `remember` | `fact_kinds` (fact and relation items), `confidence`, `metadata`, `assertion`, `evidence` (the request's own list, or an item's list on a binding without an evidence collection), `reference_revision_pin` (a pinned reference revision), `expected` (unless `merge` is `merge_provenance`) |
| `recall` | `embedding_provider`, `fact_kinds`, `filter`, `temporal_read` (`as_of_ms`), `paths`, `metadata` (projections), `evidence_recall` (text or vector without an evidence collection), `query_text_length`; `require_complete_feed: true` answers 409 `memory_feed_unsupported` |
| `consolidate` | `consolidate_plan` (`dry_run`), `consolidate_delete` (`logical_delete`), `fact_kinds`, `confidence`, `references`, `evidence`, `metadata`, `assertion`, `expected`, `merge` |
| `forget` | `forget_target` (series or entity), `erasure_jobs` (session or scope target, `allow_job`), `forget_expire` (`mode: expire`) |
| `receipts/lookup` | `remember_receipt_lookup` |
| `facts/*`, `erasures/*` | `facts`, `erasure_jobs` |

Recall serves exact, bounded retrieval. `ranking.selection` `exact` works on a
vector, alone or with `text`: the vector candidates are ordered by cosine
distance, fused with the text candidates by reciprocal rank, and a candidate
found only by text is kept without a `scores.distance`. `mmr` works on a vector
alone. Entity seeds, other weights or a half-life, a missing vector, and text
with `mmr`, answer 422 `memory_unsupported_ranking`. `exact` and `mmr` measure
cosine distance, `1 - cos` (0 for the same direction, 1 for orthogonal and 2 for
opposite vectors): `exact` orders candidates by it and both report it in
`scores.distance`. A query vector of zero length has no direction and answers
422 `invalid_input`; a stored zero vector is never one of their vector
candidates. The default `weighted` selection keeps measuring squared Euclidean
distance. Upgrading from 0.7.0: `exact` and `mmr` reported squared Euclidean
distance (twice the cosine distance for unit vectors) and `exact` ordered by it,
so vectors that are not unit length can change order and every `scores.distance`
of those two selections changes; recalibrate any threshold on it. `exact` with
`text` answered 422 in 0.7.0 and is now served.
`mode: approximate`
answers 422 `memory_bounded_plan_unsupported` because no approximate index is
built for a binding, and so does `allow_partial: true` when a budget is
exhausted: partial results are not served.

On a binding with an evidence collection, an item of `remember` may carry one
evidence reference: `"evidence": [{"collection": "notes", "record": 7}]`, the
binding's name and the numeric ID of an existing record of its evidence
collection (an integer, or its decimal string). The memory then holds the record
as its evidence, `recall` returns it, and `forget` deletes it with the memory:
attaching transfers the record to the memory, so attach only a record you do not
need on its own.
Attaching claims a record that the memory routes cannot otherwise reach, so the
grant needs `raw_access`: a request with an item reference and without it
answers the same 403 as an unknown binding before any record is looked at, as
does a `collection` that is not the binding's name. A record that is missing,
hidden, or already held by a memory answers one 409 `memory_attachment_conflict`
whichever it is. A longer list, an unknown field and a `record` that is not a
`u64` answer 422 `invalid_input` with `details.field` `evidence`; the request's
own `evidence` list and the `evidence` of a `consolidate` output stay refused,
and an empty list is the same as none. An exact retry replays, and a retry that
names another record is 409 `idempotency_conflict`. An item that repeats an
existing memory (same scope, kind, content and source) keeps the evidence that
memory holds: a reference to another record is the same 409, never ignored. To
attach a record to a memory that already exists, merge a new source into it
(`merge: merge_provenance` with the `expected` revision); that attaches the record
only when the memory holds none yet. Naming a record makes the
file one-way: after the first reference do not downgrade to 0.7.0, which answers
500 for every memory operation of that scope (see Upgrading from 0.7.0 below).

A binding's identity is stored in the database. Each bound graph holds a record
(the node `binding:v1`) with the binding's name, tenant, generation, state,
receipt window, graph and evidence names and a digest of its grants, and every
start checks the file against the records. A binding's receipt window, graph and
evidence collection never change (a new window needs a new binding name).
Changing its grants or state needs a higher `generation`, and an unchanged entry
keeps its generation. A retired binding does not become active again and keeps
its `retired_at_ms`, which may not precede the start that retires it. An entry
may leave the file only after it is retired and its receipt window has ended,
and its graph and evidence collection are not reused by another binding. A start
that breaks a rule is refused with a fixed message that names nothing, and
writes nothing. Before every `remember`, `consolidate` and `forget` the server
rereads the record under the write lock; a missing or corrupt record, or one that
is no longer the binding and generation being served, answers 409
`memory_capability_unavailable` with `details.feature` `binding_generation` and
writes nothing.

### Change grants without a restart

`POST /v1/admin/memory/bindings/reload` is admin-only and takes no request
input. It rereads only the configured bindings file; it never rewrites it.
Change principals, `scopes`, `read_scopes` or `raw_access`, raise each changed
binding's `generation`, and replace the file atomically before sending the
request. An unchanged entry keeps its generation. Discover support through
`features.memory_bindings_reload` and the capabilities route list.

Before reload, call admin-only `POST /v1/admin/memory/bindings/preflight`
(no body; any body is ignored). It reads the configured file and runs reload's
checks under the shared database lock without provisioning, writing records,
installing grants or waiting for older requests. It never takes the reload lock.
The verdict is **advisory**: reload rereads the file and rechecks.

Completed evaluations return 200 with all five fields:

```json
{"outcome": "reload", "bindings": 3, "changed": 1, "records": 1, "error": null}
```

| `outcome` | `bindings` | `changed` | `records` | `error` | Reload now would answer |
|---|---|---|---|---|---|
| `reload` | n >= 1 | >= 1 | >= 0 | `null` | 200 with this change count |
| `unchanged` | n >= 1 | 0 | >= 0 | `null` | 200 `changed:0`; a recheck may still write records |
| `restart_required` | n | `null` | `null` | `{"code":"memory_restart_required","message":<fixed message>}` | 409, same code and message |
| `refused` | n | `null` | `null` | `{"code":"memory_bindings_conflict","message":<fixed message>}` | 409, same code and message |
| `unavailable` | `null` | `null` | `null` | `{"code":"memory_reload_unavailable","message":<fixed message>}` | 409, no configured file/binding or a gRPC router was built |
| `invalid` | `null` | `null` | `null` | `{"code":"invalid_input","message":<fixed message>}` | 422, file missing, not regular, too large or invalid |

`bindings` counts file entries, but is null for `unavailable` (including gRPC)
and `invalid`. `changed` and `records` are null for every refusal outcome.
`records` counts binding records this node would write, and is 0 on a read-only
database or replica. Preflight cannot predict `memory_reload_in_progress`,
provisioning or record-write storage failures, or file/database changes before
reload. Authentication/admission failures remain HTTP errors: 401 `unauthorized`,
403 `read_only` or `forbidden_scope`, 429 `rate_limited`, 503 `busy` or
`unavailable`, and 500 `internal_error` for poisoned locks or blocking task
join failures; surfaced storage errors use the ordinary read error mapping.

Admin-only `GET /v1/admin/memory/bindings/status` returns 200:

```json
{"configured": true, "reloadable": true, "bindings": 3, "file": "agrees", "drained": true}
```

`configured` means the served view has a binding; `reloadable` equals
`features.memory_bindings_reload`; `bindings` counts served entries.
`file` is `agrees` when entry counts and entries match (binding order does not
matter, grant order does), `differs` otherwise, `unreadable` for a failed file
read/validation, or `none` without a configured source. `drained` means every
request using older grants has finished. Status takes neither the database nor
reload lock, changes nothing and reports no in-progress state. Its advisory
snapshot may lag a concurrent reload by one call; poll after reload until
`file` is `agrees` and `drained` is true. Errors are 401/403/429 as above or 500
`internal_error` for a blocking task join failure. The handler never answers
503 because it takes no database lock; only the shared admission check before
route authorization can answer 503 `busy`, when the rate limiter for rate-limited
scoped credentials is full. Neither response names a binding, tenant,
principal, scope, graph, collection or path. Neither handler adds an audit event
or log line of its own; the generic request record, authentication-denial event
(both carry the caller's own credential ID, never a name from the bindings
file) and the generic `internal_error` log line of a 500 still apply.

Reload success is 200:

```json
{"bindings": 3, "changed": 1, "drained": true}
```

`bindings` counts entries in the file; `changed` counts entries different from
the served grants. An unchanged file is rechecked and returns `changed: 0`.
`drained: true` means every request using older grants, including its workers,
has finished. After a 10-second wait, `drained: false` means the change is
already applied but older requests may still finish: send the same request
again to wait again. See the [in-flight and failure contract](SECURITY.md#memory-bound-resources).
On a writable database, records are durable before new grants are served;
a restart with the same file adds no commit. A read-only node checks and
serves the new grants without writing records; memory writes stay 403 `read_only`.

Once apply has started, a client disconnect does not cancel the reload,
its bounded drain or its success reporting. A retry while it is still
running gets `memory_reload_in_progress`; retry afterwards to check again.

| Status | `error.code` and meaning |
|---|---|
| 403 | Read-only tokens and scoped credentials cannot reload. |
| 409 | `memory_reload_unavailable`: no configured file or served binding, or a gRPC router was built. |
| 409 | `memory_reload_in_progress`: another reload is running; retry after it returns. |
| 409 | `memory_restart_required`: the change is not grants-only; restart instead. |
| 409 | `memory_bindings_conflict`: startup checks refused the file, including stale or invalid generations. |
| 422 | `invalid_input`: the file is unavailable, not regular, too large or invalid. |
| 503 | Storage or admission failure, including `commit_indeterminate`, `poisoned`, `busy` or `unavailable`. |

New, removed, renamed or retired bindings, tenant or state changes, receipt
windows, graphs and evidence collections are not live changes: they need a
restart and must still satisfy the binding rules. First-time memory enable
also needs a restart. Reload is refused after a gRPC router is built; the
server refuses gRPC with memory bindings configured. For library embedders
with several REST routers, only the router answering the request reloads.

### Start the server with memory

Memory is off until the server reads a private bindings file
(`--memory-bindings-file PATH` or `VLITE_MEMORY_BINDINGS_FILE`). Without it
there are no memory routes, and no other route changes unless the database
already holds binding records: their graphs and collections stay reserved.
Create the graph (on a database with graph support) and the evidence
collection first, through the ordinary admin routes. Startup refuses to run
when either is missing, when the database has no graph support, or when a
resource already holds ordinary data. A graph is adopted only when it is empty
and its evidence collection holds no records, when it already holds this
binding's record, or, once, when a 0.7.0 server adopted it and it holds nothing
but memory nodes; one node with a memory label is not enough, so bind only
resources made for memory. On a writable primary, startup then writes each
binding's record (an unchanged record is not rewritten, so a restart with the
same file adds no commit) and creates every active binding's granted scopes. A
read-only node or replica runs every check, writes nothing and refuses memory
writes with 403 `read_only`.

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
  permissions, and is the opt-in for naming an existing evidence record in a
  `remember` item. That use transfers the record to the memory (forgetting the
  memory deletes the record too) and is a one-way change to the file: a 0.7.0
  server answers 500 for every memory operation of a scope that holds such a
  memory (see Upgrading from 0.7.0).
- `"read_scopes"` (optional, same entry fields as `scopes`) lists scopes the
  credential may only read: `recall` and `facts/query` accept them, every
  other route answers 403 `forbidden_scope` as for an ungranted scope. A scope
  appears at most once across both lists; `scopes` may be empty only when
  `read_scopes` is not; a grant with `raw_access` cannot list any `read_scopes`.
  Startup provisions read scopes, and changing them needs a higher
  `generation`. Older servers refuse a file that uses `read_scopes`.
- `"state": "retired"` with `retired_at_ms` keeps `receipts/lookup` working and
  answers every other route 409 `memory_capability_unavailable` with
  `details.feature` `binding_retired`.
- At most 256 bindings, 1,024 grants per binding and 256 scopes per grant,
  counting `scopes` and `read_scopes` together.

While any binding is configured the gRPC listener, automatic index
maintenance, imports, replication export and a few aggregate admin routes are
disabled, and the bound resources are hidden from the raw routes; see
[Memory-bound resources](SECURITY.md#memory-bound-resources). To stop serving
memory, restart without the file: the routes end, but the bound graph and
evidence collection stay reserved, with no grants, for as long as the graph
holds its record, so a removed file never exposes the memory. To make them
ordinary again, keep the entry in the file as retired with a `raw_access` grant,
delete the graph's nodes (the record node `binding:v1` included) with that
credential, and drop the graph.

**Upgrading from 0.7.0.** The first 0.7.1 start writes each binding's record (one
node and one commit per bound graph) and applies the rules above to a file that
was valid before. Four things can stop it or surprise you:

- An entry that is already `retired` and has no record yet counts as retired by
  this start, so a `retired_at_ms` in the past is refused (`a memory binding
  retirement time precedes this start`): set it to the current time or later.
- A bound graph that holds anything but memory nodes, for example ordinary nodes
  that a `raw_access` holder wrote into it under 0.7.0, is refused as holding
  ordinary data. Start once without the bindings file (the graph is then an
  ordinary graph, because it holds no record yet), delete those nodes with the
  raw routes, and start again with the file.
- Going back to 0.7.0 after that is safe for every graph that holds a scope
  node, unless a memory in it holds an evidence reference (the next point). A
  bound graph that never received a scope (a binding without grants)
  holds only the record, and 0.7.0 refuses it as ordinary data: delete the record
  node first.
- Item evidence references are a one-way change. A `remember` item that names an
  evidence record (a `raw_access` grant is the explicit opt-in) stores it in the
  memory node as the token `record:<number>`, which a 0.7.0 server cannot read.
  Under 0.7.0 every memory operation of the scope that holds such a memory
  answers 500 `internal_error`: recall by seed or by vector, a new `remember`,
  `forget` and `consolidate`. Only an exact replay of an earlier `remember` that
  carried no reference (200) and the replay of the one that carried it (409,
  feature `evidence`) still answer. Other scopes, other bindings and the evidence
  record are unaffected, and the server stays up. Do not downgrade after the
  first reference. To recover, run 0.7.1 or later and `forget` the memory, which
  also deletes the record; 0.7.0 then serves the scope again.
  `scripts/memory_evidence_downgrade_check.py` shows this against two server
  binaries.

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

### File compaction (experimental, off by default)

A single-file database keeps old pages after updates and deletes, so the file
grows. File compaction writes a compact copy and switches the server to it.
These routes need a super administrator (admin token or `super_admin`
account); read-only and scoped credentials get 403. Dashboard calls need the
usual same-origin and CSRF headers.

In the dashboard, the **File compaction** card under **Administration →
Backup & recovery** shows the file size, plain reasons when compaction is
unavailable, the current phase (announced to screen readers; there is no
invented progress percentage), **Cancel** while cancelling is still safe and
the bytes before and after the last run. **Compact now** asks you to confirm
the pause and the dropped history. **Enable automatic compaction** is off by
default; the start time and window are UTC, and each setting has a label,
helper text and an inline range error. Saving sends the settings version, so
two administrators cannot silently overwrite each other.

| Route | Purpose |
| --- | --- |
| `GET /v1/admin/compaction` | Status: `capability`, `available`, plain `reasons` with codes, file size, `settings` and `settings_version`, the schedule, the running and last job and retained recovery copies. Never starts work. |
| `PUT /v1/admin/compaction/config` | Save settings with `expected_version` (compare-and-set). |
| `POST /v1/admin/compaction/jobs` | Compact now: `expected_version`, `acknowledge_downtime: true`, `acknowledge_history: true` and a UUID `idempotency_key`. Answers 202 `{job, replayed}`. |
| `GET /v1/admin/compaction/jobs/{id}` | One job: `phase`, `outcome`, `code`, `selection`, `before_bytes`, `after_bytes`, `saved_bytes`. There is no progress percentage. |
| `POST /v1/admin/compaction/jobs/{id}/cancel` | Cancel before the switch; the server keeps the original file. |

**Where it runs.** Compaction needs the managed `vlite-server` runtime:
`VLITE_ADMIN_DIR` set, a persisted database (durability other than `none`)
and no gRPC listener. Other servers report `capability: false` with
`managed_runtime_required`. The only qualified profile is
`linux-local-ext4-v1`: Linux, with the database file and admin directory on
the same local ext4 filesystem, and sources up to 1 GiB. Everything else is
refused with a reason code rather than attempted:

* Other filesystems (overlay, 9p, SMB/Azure Files, NFS) and native Windows or
  macOS give `unqualified_filesystem`.
* Open (no-token) mode gives `authentication_required`.
* Encrypted files give `encrypted_compaction_unqualified`.
* Files older than format v5 give `unsupported_source`.
* Files over 1 GiB give `source_too_large`.
* Too little free space (about four times the file) gives
  `insufficient_space`.
* Memory bindings, webhook subscriptions and a running import give their own
  `*_present` or `import_state_unsupported` reasons.

**What happens.** A job passes through `preflight`, `quiescing`, `freezing`,
`building`, `validating`, `selecting` and `reopening`. Preflight leaves the
server running. From `quiescing` until the job ends, new reads and writes
answer 503 `maintenance_in_progress` with `Retry-After: 5`, and `/ready` is
not ready; requests already admitted finish first. `/health`, sign-in,
`/v1/capabilities`, the dashboard and the compaction routes keep working.
A 503 is retryable, but check a write whose outcome you did not see before
you send it again.

The original file is never changed, and it is kept as a recovery copy.
Compaction cannot start while a restore selection for the next startup is
pending (`restore_selection_pending`). While a job runs, restore selection
answers 409 `compaction_busy`.

If a job is interrupted after its selection was written, restart recovery
finishes it. Until then the server serves only authenticated status
(`recovery_required`), and it never falls back to an older file once new
writes may exist. While a committed compaction selection pins the startup
file, a different restore selection answers 409
`compaction_selection_pinned`.

**What is kept.** Records, payloads, graphs, credentials, accounts, the
database lineage, the global commit sequence and ID allocation high-water
marks are kept; IDs are never reused. Record ETags and graph revisions can
advance to the compaction sequence S, so an `If-Match` or graph
`expected_revision` taken before compaction fails with 412 or 409 rather than
overwriting newer data. Read again and retry; new revisions are always greater
than S. Change history from before compaction is not kept: change-feed cursors
taken earlier answer `cursor_compacted`, which is why both acknowledgements
are required.

**Automatic compaction.** Automatic compaction is off by default. Times are UTC.
Enabling needs `history_policy: "resnapshot_allowed"` with
`acknowledge_history: true`. A scheduled run starts only inside the daily
window, when enough of the window remains for `max_run_seconds`, after the
cooldown, and when the file has grown by both `min_growth_bytes` and
`min_growth_percent` above the size measured after the last run (the first
evaluation records that baseline). Growth is measured in file bytes, not
estimated from free pages. Defaults and bounds:

| Setting | Default | Bounds |
| --- | --- | --- |
| `daily_start_utc` | `02:00` | `HH:MM`, UTC |
| `window_minutes` | 30 | 5–120 |
| `max_run_seconds` | 900 | 60–3600 and at least 60 s shorter than the window |
| `min_file_bytes` | 256 MiB | ≥ 64 MiB, ≤ `max_source_bytes` |
| `min_growth_bytes` | 128 MiB | ≥ 64 MiB, ≤ `max_source_bytes` |
| `min_growth_percent` | 25 | 10–200 |
| `cooldown_hours` | 168 | 24–720 |
| `max_source_bytes` | 1 GiB | ≥ 64 MiB, ≤ the profile limit (1 GiB) |

Errors: 409 `settings_version_conflict`, `history_ack_required`,
`idempotency_key_reused`, `compaction_busy`, `job_finished`,
`selection_in_progress`, `selection_committed`, `selection_outcome_unknown` or
an unavailability reason. 422 `invalid_settings`, `invalid_input` or
`acknowledgement_required`. 404 `not_found`. 503 `recovery_required`. Settings
changes and job starts and cancellations are audited. To go back to the
original file, or to an earlier VLite release, restore from a backup or
recovery copy while the server is stopped. The restore guide in
[DOCKER-OPERATIONS.md](DOCKER-OPERATIONS.md#file-compaction) has the steps.

### Cloud backups to Azure Blob (opt-in, off by default)

The server can publish verified backup sets of its database to Azure Blob
Storage and restore one as an isolated copy. These routes need a super
administrator (admin token or `super_admin` account); read-only, scoped and
tenant credentials get 403. They need `VLITE_ADMIN_DIR`; without it they
answer 409 `admin_directory_disabled`. The dashboard's **Administration →
Cloud backup** card drives these routes; the offline `vlite-server cloud-sets`
and `cloud-restore` commands work without a running server.

| Route | Purpose |
| --- | --- |
| `GET /v1/admin/cloud-backup` | Policy, installation ID, run status (last attempt, success, error code, unknown publication, the run in progress as `job`, `interrupted` runs, and `clock_reset_unix_ms` when future schedule times were pulled back to the clock), the next scheduled run, measured on the wall clock, and `policy_uncertain`. |
| `PUT /v1/admin/cloud-backup` | Save the policy (body below). Does no remote work. |
| `POST /v1/admin/cloud-backup/run` | Publish one set now. Answers 201. **Not idempotent**: retrying after an unseen outcome can publish another set, so read the status or the set list first. |
| `POST /v1/admin/cloud-backup/collect` | Cleanup now: removes unpublished sets older than `orphan_grace_hours` and finishes interrupted deletions; deletes published sets only when `retention_enabled` is set. A run collects automatically only with retention on. |
| `GET /v1/admin/cloud-backup/sets` | Sets under the prefix with state, pins and the last drill. |
| `POST /v1/admin/cloud-backup/sets/{id}/drill` | Download, verify, restore to scratch and probe a set; records the result. |
| `POST /v1/admin/cloud-backup/sets/{id}/restore` | Write a new-lineage `restore-*.vlite` copy into the admin directory. Answers 201. |
| `POST /v1/admin/cloud-backup/sets/{id}/pins` | Hold a set against retention: `{"reason": "…"}` (1–200 characters). Answers 201 with the pin ID. |
| `DELETE /v1/admin/cloud-backup/sets/{id}/pins/{pin}` | Release an operator hold. A pin owned by a running restore or drill (`"kind": "operation"`) is refused with 409 `cloud_pin_owned`; if its owner crashed, `?force=true` releases it (audited as `cloud_backup_pin_force_released`, 409 `cloud_pin_in_use` while this server still runs that operation). |

During server shutdown the `PUT`, `POST` and `DELETE` routes answer 503
`cloud_backup_closed`.

The policy is persisted with the run status in one atomically replaced
`<admin>/cloud-backup/state.json` and survives restarts. A `PUT` that answers
500 `cloud_backup_policy_uncertain` could not confirm its write; the `GET`
view then shows the policy actually in effect. If the file cannot be read back
either, `GET` shows the last confirmed policy with `"policy_uncertain": true`
and no next run: no scheduled run starts, and the run, collect, set list,
drill, restore and pin routes answer 503 `cloud_backup_policy_uncertain` until
a reread (every `GET` and scheduler tick retries) or a successful `PUT`
confirms the policy. It holds no secret:

```json
{"target": {"account": "contosobackup", "container": "vlite-data",
            "coordination_container": "vlite-meta", "prefix": "vlite/prod/"},
 "enabled": true, "interval_secs": 3600,
 "retention_enabled": false, "keep_sets": 3, "keep_days": 30,
 "orphan_grace_hours": 24, "drill_every": 24}
```

* `target.auth` defaults to `managed_identity`; `client_id` selects a
  user-assigned identity. `emulator_shared_key` is accepted only with a
  loopback `endpoint` (a local Azurite) and reads the key from the file named
  by `VLITE_CLOUD_BACKUP_SHARED_KEY_FILE`.
* `interval_secs` 300–604800 (default 3600). `enabled: false` stops scheduled
  runs; manual runs still work.
* Retention keeps a set while it is among the newest `keep_sets` (3–1000) or
  younger than `keep_days` (30–3650), pinned, the newest verified set or the
  newest set whose last drill passed. Staging sets that never published are
  removed after `orphan_grace_hours` (1–720).
* `drill_every` drills every Nth published set (0–100, 0 turns scheduled
  drills off).
* `archive` (optional, absent by default) turns on the continuous log archive:
  `{"enabled": true, "interval_secs": 300}` (60–86400, default 300). Each run
  appends a verified checkpoint (log segment and recovery bundle) after the
  latest published set; `status.archive` reports it (a run with nothing new
  still reports the latest checkpoint as `tip`). The offline
  `vlite-server cloud-restore --archive-checkpoint latest|<sequence>`
  restores to an exact checkpoint only: times and other sequences are refused
  with `pitr_target_unsupported`, never rounded. Without the field the policy
  and its capture hash are unchanged. See
  [DOCKER-OPERATIONS.md](DOCKER-OPERATIONS.md#continuous-log-archive-and-checkpoint-restore-opt-in).

**What a set holds.** A consistent snapshot of the database file and a
recovery bundle: tenants, dashboard account names and roles, the scoped
credential inventory and the names of the secrets a restore target must
provision. It never holds a secret value, password or credential hash,
session, token, filesystem path or host identity. Each
object is named by its SHA-256 and created once; it is read back and checked
before the manifest is written, and a set becomes restorable only when its
record is published by a compare-and-set. A publication whose outcome was
unknown is resolved by the next run before anything else. The manifest also
names the policy revision, a SHA-256 of the policy document and the retention
terms in force. Integrity rests on the SHA-256 chain from record to manifest
to objects; blob version IDs and ETags are not recorded, and manifests are
not signed, so authenticity rests on who may write the containers.
The bundle names its database object's SHA-256 and the same policy capture;
a set that pairs it with anything else is refused as `cloud_set_corrupt`.
Before upload the bundle is certified (well-formed, unique tenants, accounts
and credentials whose tenant references resolve), else 409
`recovery_config_inconsistent`. The admin directory may hold only files the
server writes; any other entry, such as a token or TLS file, answers 409
`recovery_config_unrecognized` (the message gives only a count). The admin
stores are read before and after the database copy and a change answers 409
`recovery_config_unstable`; the reads are not serialized with admin writes,
so a change undone in between goes unseen, which a rekey-only restore cannot
turn into access.

**Restore is rekey-only.** The restored copy gets a new lineage, so existing
scoped credentials, sessions and replication cursors do not carry over:
reissue credentials after you switch. The report's `credentials_to_reissue`
lists each credential's grant and its `requests_per_second`, `max_vectors`
and `max_vectors_per_collection` limits, never a secret. The route never
touches the live database; select the copy with `POST /v1/admin/restores/select` and restart.
`{"mode": "seed"}` (same-lineage standby seeding) is refused with 409
`cloud_seed_mode_deferred`. For a restore on a fresh host, use the offline
`vlite-server cloud-sets` and `vlite-server cloud-restore` commands (see
[DOCKER-OPERATIONS.md](DOCKER-OPERATIONS.md#cloud-backups-to-azure-blob)).

**Memory.** A server with memory bindings is backed up too. A run copies the
binding set in force together with the database snapshot, and its bundle
has the schema `vlite.recovery-bundle/2` with that set; every other bundle
stays `vlite.recovery-bundle/1`, which older servers still read. A database
that holds binding records but no bindings file names a binding gets a `/1`
bundle, and its restored copy keeps those graphs and collections reserved
with no grant. A drill and a restore run the startup binding checks against
the restored copy: bound graphs and evidence collections exist, the records
are valid and the captured set passes the rotation rules. A failure fails
the drill (`cloud_restore_memory_failed` in its error) or answers 409
`cloud_restore_memory_failed`; a malformed memory section answers 409
`cloud_restore_memory_invalid`. The drill detail and the restore report
carry `memory` counts (`bindings`, `active`, `retired`,
`principals_to_rekey`, `orphan_records`) and never name a binding or
principal. The restore route writes no bindings file: take it from the
command-line restore or the source. Its principals are the old credentials,
so before serving the copy with `--memory-bindings-file`, re-issue
credentials, replace each binding's principals with the new IDs and raise
its `generation`; never widen `raw_access`. Without the file the restored
memory stays reserved with no grant. A restore brings back memories and
evidence that were forgotten or erased after the backup was taken.
Compaction still refuses a server with memory (`memory_reservations_present`).

**Refused, not attempted.** These answer 409 with a reason code: an
encrypted database (`encrypted_cloud_backup_unqualified`), an in-memory or recovery-mode
server (`cloud_backup_requires_database_file`), a prefix owned by another
installation (`cloud_prefix_owned`) and a database over 1 GiB, the supported
cloud-backup size (`cloud_database_too_large`, also refused before a restore
or drill downloads the payload). A restore or drill that would need more free
space than twice the database plus its recovery bundle and a 64 MiB margin
answers 507 `restore_insufficient_space` before the payload download (after
the manifest and coordination records are read; the check runs on Linux and
macOS; Windows does not report free space to it). Errors: 409 conflicts
such as `cloud_backup_not_configured`, `cloud_publication_lost`,
`cloud_set_unpublished`, `cloud_set_unavailable`, `cloud_pin_owned` or
`cloud_pin_in_use`; 404 `cloud_set_not_found`
or `cloud_pin_not_found`; 400 or 422 `invalid_json` (fixed text that never
repeats the body) and 422 `invalid_input`; 502 `cloud_store_failed` or
`cloud_set_corrupt`; 500 `cloud_backup_policy_uncertain` (`PUT`); 503 `busy`,
`cloud_store_unavailable`, `cloud_publication_unknown` or
`cloud_backup_policy_uncertain`. Policy changes, runs, collections, drills,
restores and pins are audited.

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

## Serve status and reader freshness

`GET /v1/admin/serve-status` is administrator-only and works independently of
`/metrics`, including when memory bindings are configured. It reports `mode`
(`standalone`, `writer` or `reader`), `draining`, `read_only`, hexadecimal
`lineage`, `binding_generation`, `format_version`, `sequence`, `epoch` (the
claimed writer-authority epoch on an authority-enabled writer, otherwise null),
`archive`, `reader`, `uptime_ms` and `note`. A gateway-trusted backend grants
only scoped tenant roles and has no admin path, so this route is unavailable
there. Existing archive progress
includes checkpoint ages and errors. Positions and archive error text are
withheld when memory bindings block metrics. Super admins see the same report
in the dashboard under **Observability → Live monitor → Server role and
replication**.

`binding_generation` is 0 without configured bindings; otherwise it is a
deterministic, nonzero, JSON-safe fingerprint of the binding configuration.
Copy this decimal number from the **writer's** serve-status into the tenant's
control-state directory record as `binding_generation`. A reader must load the
same configuration to be eligible for that record's reads.

`reader` is null without archive-apply progress. A following reader reports:

```json
{"applied_sequence": 42, "age_ms": 100, "bound_ms": 1000,
 "stale": false, "failed": false}
```

`applied_sequence` is null when memory bindings block metrics; `age_ms` is null
before the first confirmation. No failure text is exposed. With a positive
`bound_ms`, a reader that has never confirmed freshness or whose age exceeds
the bound refuses `/ready` and authorized reads with 503 `reader_stale`.
`/v1/admin/serve-status` remains available for diagnosis. A bound of 0 disables
staleness gating. A permanently failed follower fails readiness with
`reader_apply_failed`. gRPC staleness gating is not provided.

Every REST response to an authenticated request (including a later 403, 429
or 503 refusal) from a reader with archive-apply progress carries:

* `X-VLite-Lineage`: the database lineage as 16 hexadecimal digits, omitted if
  lineage is unavailable.
* `X-VLite-Binding-Generation`: the decimal binding fingerprint.
* `X-VLite-Applied-Age-Ms`: decimal milliseconds since the last freshness
  confirmation, omitted before the first confirmation.
* `X-VLite-Applied-Sequence`: the decimal applied sequence, omitted when memory
  bindings block metrics.

These headers include no position token and provide no read-your-writes
guarantee. Requests that do not authenticate (anonymous probes of public
routes such as `/health` and `/ready`, and 401 refusals) never receive them,
so anonymous callers cannot learn reader identity or freshness. Standalone
servers, writers and readers without apply progress do not send them. Writer/reader roles and gateway replicas add no write capacity.

## Automatic failover (unqualified, opt-in, off by default)

Automatic failover is **unqualified**: the failover drills and loss-report
review of GATE-005 have not passed, so it is not supported for production.
It is off unless an operator turns it on, and it does nothing unless an
operator also runs a supervisor. Nothing changes for servers without writer
authority.

The switch is one small policy object stored beside the writer authority
record (`<prefix>auto-failover.json`, schema `vlite.auto-failover/1`). Every
supervisor reads it before each probe, immediately before fencing and again
after fencing, so one switch stops them all; it cannot recall a fence
command that is already running.

`GET /v1/admin/auto-failover` (super administrator) returns:

```json
{"available": true, "enabled": false, "revision": null,
 "updated_unix_ms": null, "source": null, "invalid": false,
 "qualified": false, "gate": "GATE-005", "warning": "..."}
```

`available` is false on a server without an authority target; the PUT then
fails with 409 `auto_failover_not_configured`. `invalid` is true when the
stored object cannot be read as a policy; it then counts as off.

`PUT /v1/admin/auto-failover` takes
`{"enabled": bool, "expected_revision": "<revision>", "acknowledge_unqualified": bool}`
and rejects other fields.

* Turning it **on** needs `acknowledge_unqualified: true` (otherwise 422
  `auto_failover_unacknowledged`) and the `revision` from the last GET
  (omit it only while no policy exists). A stale or missing revision fails
  with 409 `auto_failover_conflict`; an unreadable stored policy fails with
  409 `auto_failover_invalid` until it is turned off. When turning on, a
  revision that is not 32 lowercase hexadecimal characters is 422
  `invalid_json`.
* Turning it **off** ignores any revision and replaces an invalid policy. It
  always writes a fresh disabled revision, including when already off or no
  policy exists; this invalidates an enable based on an earlier revision.
  `vlite-server failover policy disable` does the same without the dashboard
  or a running writer. Disabling before the supervisor's final transfer check
  aborts promotion; a transfer already past that check may still complete.
* Storage errors return 503 `auto_failover_unavailable` or
  `auto_failover_indeterminate` (the change may have been written; read it
  again before retrying).
* Open development administration refuses the PUT with 401
  `admin_auth_required`; configure administrator authentication before using
  this mutation. A reader refuses the PUT with 403 `reader_mode`. A successful change is
  audited as `auto_failover_policy_changed` with `actor`, `enabled` and
  `revision`. Super admins use the same switch in the dashboard under
  **Administration → Backup & recovery → Automatic failover**.

The supervisor is a separate process run by the operator:

```text
vlite-server failover auto --config <json>
vlite-server failover policy show|enable|disable --target-file <json> [--shared-key-file <path>] [enable: --expected-revision <rev> --acknowledge-unqualified true]
```

The config JSON requires `target_file`, `writer_ready_url` (http or https),
`probe_interval_ms` (500 to 60000), `probe_timeout_ms` (100 or more, below
the interval), `failure_threshold` (2 to 100) and `fence_command` (an argument
list run without a shell); `shared_key_file`, `probe_token_file` (a bearer
token for `/ready`, needed unless the writer trusts a proxy),
`promote_command` and `command_timeout_ms` (1000 to 3600000, default 300000)
are optional. It counts consecutive failed readiness probes of the serving
writer for the same authority epoch and revision, and acts only on a writer
it has itself seen ready in that epoch. A successful response must identify
`status: "ready"`, `mode: "writer"` and the expected authority `epoch`;
otherwise it holds instead of counting. HTTP 401, 403, redirects, 429
throttling and other unexpected responses hold instead of counting. At the threshold it checks the switch and
authority again, runs the fence command, which must stop every old writer
replica and print exactly `{"old_replicas":0}`, checks the switch again,
takes a loss report as `failover report` does, refuses if authority changed,
runs the same transfer as `failover execute`, and then runs the promote
command, if any. Commands receive `VLITE_FAILOVER_EPOCH`,
`VLITE_FAILOVER_REVISION` and `VLITE_FAILOVER_WRITER_ID`; the promote command
also receives `VLITE_FAILOVER_NEW_EPOCH`. It stops after one attempt. A
promotion prints the result, with the full loss report and its text, and
exits 0; if the promote command failed after authority moved it exits
nonzero with `auto_failover_standby_failed`, and an abort exits nonzero with
`auto_failover_aborted: <reason>`. Commands get no shell, a time limit and
bounded output. Run exactly one supervisor per authority target. The
supervisor cannot itself verify that the old writer has stopped; the fence
command is the operator's assertion, exactly like `--old-replicas 0`. See the
[HA operations guide](HA-ACA.md#automatic-failover-unqualified-off-by-default).

## Gateway mode and gateway-trusted backends (experimental, opt-in, off by default)

Disposable measurement tooling is separately opt-in: build the `ha-meas004` and
`ha-meas005` examples with `--features server`. `vlite-server gateway-measure
bootstrap --config <json>` creates a private synthetic fixture and normal
credentials/keys without publishing control state; `gateway-measure mint
--gateway-config <json> --token-file <path> --output <new-private-file>` exports a
short-lived normally verified principal, never an auth bypass. A database named
`gateway-measure` now requires a `./` prefix. See the executable
[MEAS-004/005 CLI/config contract](../deploy/azure-container-apps/ha/measure-gateway-contract.md)
for exact app/job entrypoints, limits, ABBA load, owned-process observers,
command-bracket propagation analysis and cleanup. Outputs are
`proposal_evidence_only`, not qualification; unknown scale/clock/coverage and
read-cost capabilities remain explicit. No serving endpoints or deployment gates
are added or changed.

A gateway is a `vlite-server gateway` process that opens no database. It
authenticates tenant credentials against a shared control-state object in
Azure Blob Storage, checks the operation, takes one request from its edge
rate-limit share and forwards the request to the tenant's backend with a
signed principal envelope. A gateway-trusted backend is an ordinary
`vlite-server <database>` started with `--gateway-trust-file`. It accepts
data requests only with such an envelope and checks the envelope itself.
Nothing changes unless you use these commands or that flag. The default
standalone server is unchanged, except that a database literally named
`gateway`, `gateway-keygen` or `control-state` now needs a `./` prefix (as
`cloud-sets` and `cloud-restore` already did).

This is a local, experimental MVP of the feature-aca-ha plan (design and
status: [gateway.md](projects/feature-aca-ha/gateway.md)). It has been tested
with loopback processes and a local Azurite only. It is not qualified for
production. Opt-in local writer authority, epoch fencing, the gateway's
response-epoch check and manually prepared failover are implemented (see
[HA-ACA.md](HA-ACA.md)); automatic failover, ACA deployment, live ACA
qualification and measured timing or replica values are not. No zero-window
fencing guarantee is claimed. Gateway replicas add no write capacity: every
tenant has one backend.

### What a gateway serves

* **Tenant credentials only.** A single `Authorization: Bearer vlk_...`
  credential that belongs to a tenant. Admin and read-only tokens and
  super-admin routes are never served through a gateway. Sign-in, sessions
  and the dashboard are served only with the opt-in `dashboard` block, and
  only for tenant editor and viewer accounts (see
  [Dashboard sign-in through a gateway](#dashboard-sign-in-through-a-gateway-od-004-opt-in)).
* **Its own health and readiness.** `GET /health` answers
  `{"status":"ok"}`. `GET /ready` answers `{"status":"ready","generation":A}`
  only while its control-state copy was confirmed within `s_ctl_ms`.
  Otherwise it answers 503 `control_state_stale`, and 503 `draining` after a
  stop signal. Neither route needs a database or `/metrics`.
* **Every registered route.** Writes always go to the writer. Reads also go
  to the writer by default; a dedicated tenant can opt into reader routing.

Before forwarding, the gateway checks, in order:

1. The path. Dot segments, backslashes, and percent-encoded dots, slashes or
   backslashes get 400 `invalid_path`.
2. The route. An unregistered route gets 404 `not_found`, and a wrong method
   gets 405 `method_not_allowed`. The standalone public routes
   (`/v1/auth/login`, `/v1/auth/config` and the dashboard) also get 404
   through a gateway without a `dashboard` block. With one, the gateway
   answers them and the other `/v1/auth/*` session routes itself; they are
   never forwarded.
3. A fresh control-state snapshot, else 503 `control_state_stale`.
4. The credential and tenant, with the standalone rules. Without a tenant
   credential (or, with a `dashboard` block, a valid session cookie) the
   answer is 401 `unauthorized`; unknown, revoked and expired
   credentials are refused as on a standalone server; a suspended tenant gets
   403 `tenant_suspended`. A credential without a tenant gets 403
   `gateway_tenant_required`, and a tenant with no directory record gets 503
   `tenant_not_routed`.
5. Authorization. The credential's scope is checked as on a standalone server,
   against the lineage in the tenant's directory record.
6. The edge rate limit. A credential with `requests_per_second` gets one token
   bucket on each gateway replica, holding and refilling
   `requests_per_second / max_replicas` (never rounded up). An empty bucket
   answers 429 `rate_limited` with `Retry-After`. A share below one request a
   second answers 503 `rate_limit_share_below_one` for every request of that
   credential rather than allowing more than the limit. Buckets are capped at
   `max_buckets`; a new credential then gets 503 `rate_limit_capacity` until a
   full bucket can be evicted.
7. The body. Bodies over `max_request_bytes` get 413 `payload_too_large`. A
   body whose next piece does not arrive within `upstream_timeout_ms` gets
   408 `request_body_timeout`, and nothing is sent upstream.

More than `max_in_flight` concurrent requests get 503 `overloaded`. Dashboard
sign-in (`POST /v1/auth/login`) is admitted before that limit and never holds
one of its slots: each replica runs at most 16 sign-ins at once, and the
next gets 503 `overloaded` while other requests are unaffected. The whole
sign-in body must arrive within `upstream_timeout_ms`, or the sign-in gets
408 `request_body_timeout` and no cookie.

**Forwarding.** The gateway keeps the method, path, query, body and status. It
drops the caller's `Authorization`, `Cookie`, `x-vlite-principal`, `Host`,
`Forwarded`, `X-Forwarded-*`, `X-Real-IP` and proxy headers, hop-by-hop
headers and any header that `Connection` names, and it adds its own envelope.
Responses are streamed back without hop-by-hop or `Set-Cookie` headers; the
only cookie a gateway sets is its own dashboard session cookie. The
upstream URL is the record's `writer` origin plus the caller's path and query.
A record endpoint with a path, query, fragment or user name, or a path that
URL normalization would change, is refused (503 `tenant_route_invalid` or 400
`invalid_path`). The gateway follows no redirect and uses no proxy.
`upstream_timeout_ms` bounds connecting, waiting for the response head, and
then each wait for the next piece of a streamed response. A long stream may
last longer than it as long as data keeps arriving; a stall for longer than it
cuts the response short. The same bound applies to a client that stops
reading a streamed response: it loses the response (`gateway_client_stalled`)
and its in-flight slot is released.

**Nothing is retried.**

| Upstream failure | Answer |
| --- | --- |
| Connection refused or not established (nothing was sent) | 502 `backend_unavailable` |
| Any later failure of a write, including no response head in time | 502 `write_outcome_unknown` (the write may have been applied; read before you repeat it) |
| A read that timed out before its response head | 504 `backend_timeout` |
| Any other read failure | 502 `backend_failed` |

A response that fails after its status was sent is cut short and logged as
`response_cut_short`.

### Gateway-trusted backend

Start an existing database with `--gateway-trust-file <json>`:

```sh
VLITE_AUTH_TOKEN_FILE=/run/secrets/admin-token \
  vlite-server /data/pool.vlite --bind 127.0.0.1:8081 --gateway-trust-file trust.json
```

Before the database opens, the backend reads the control state once; it does
not start without that read. In this mode:

* Only one valid `x-vlite-principal` envelope authenticates. Bearer tokens,
  including the admin token, sessions and network position grant nothing; a
  request without an envelope gets 401 `unauthorized`. The startup rule that
  requires an admin token still applies, but that token authorizes nothing.
* The envelope must have a known key, a valid signature, this backend's
  `audience`, an issue time and expiry within `skew_ms`, a lifetime of at most
  `max_lifetime_ms`, and a generation `A` no lower than the highest this
  backend has confirmed. Failures get 401 `envelope_invalid`,
  `envelope_expired`, `credential_expired` or `envelope_stale`.
* The backend checks its own control-state copy: the envelope's tenant must
  have a directory record that names this backend's `audience` and database
  lineage, and must be admitted (403 `tenant_suspended`). One `shared-pool`
  backend therefore serves several tenants, each confined to its own scope. A
  dedicated backend serves only its tenant.
* The poll thread raises the minimum accepted `A` to every generation it
  confirms, whether or not requests arrive. An envelope minted before a
  revocation is refused once the backend has polled. A copy older than
  `s_ctl_ms` fails closed (503 `control_state_stale`), and a generation lower
  than one already seen is refused and logged (`control_state_regressed`).
* The existing scope, admin-only, lineage, memory and quota guards run as for
  a `vlk_` credential. Tenant vector quotas come from the control state, and
  the backend keeps its own per-credential rate limit.
* An envelope whose principal id is `user:<32 hex>` comes from a gateway's
  dashboard sign-in. It is accepted only in the shape of a standalone editor
  or viewer session: a tenant, the tenant's whole scope, no rate or quota
  limit, and only editor or viewer operations (`read`, `search`,
  `search_build`, `write`, `import`, `index`, `collections`, `graph_read`,
  `graph_write`). It is served as a session, never as a `vlk_` credential;
  any other shape gets 401 `envelope_invalid`. A backend built before OD-004
  refuses it.
* `/ready` is unauthenticated and answers
  `{"status":"ready","generation":A,"minimum_generation":M}`, or 503
  `control_state_stale` or `draining`.
* Refused at startup: `--insecure-local`, `--grpc-bind`,
  `--serve-mode reader`, `--durability none` and `VLITE_ADMIN_DIR` (the
  backend keeps no credentials of its own).

The trust file (unknown fields refused, at most 64 KiB):

```json
{"audience": "shared-pool",
 "public_key_bundle_file": "/run/keys/gateway-public.json",
 "control_state": {"target_file": "target.json", "object": "gateway/control-state.json"},
 "s_ctl_ms": 2000, "poll_interval_ms": 200,
 "skew_ms": 1000, "max_lifetime_ms": 5000,
 "readiness_propagation_ms": 1500}
```

The example values are local test inputs, not recommendations. `audience` has
1–2048 printable characters. Durations are at most 3600000 ms;
`poll_interval_ms` must be positive and below `s_ctl_ms`; `max_lifetime_ms`
must be positive.

Experimental and not usable yet: instead of `control_state`, a trust file can
name one tenant's control projection,
`"control_projection": {"store": {"target_file": "target.json", "object": "gateway/projection-<tenant>.json"}, "tenant": "<32 lowercase hex>"}`.
Exactly one of the two is required. Such a backend decides only from that
tenant's projection and never falls back to the shared control state. It has
no qualified clock evidence for the projection's freshness, and no field of
the trust file can supply one, so its start is refused with
`projection_freshness_unproven` before the database is opened.

### Gateway configuration

`vlite-server gateway --config <json>` validates the whole file and both key
files, reads the control state once, prints `LISTENING http://<address>` and
logs JSON events to stderr (`gateway_started`, which also states that gateway
replicas add no write capacity, `gateway_upstream_failed`,
`control_state_poll_failed`, `draining`, `server_shutdown`, and with a
`dashboard` block `gateway_user_login`, `gateway_user_login_failed` and
`gateway_user_logout`). The file is at most 64 KiB and unknown fields are
refused:

```json
{"bind": "127.0.0.1:8080",
 "control_state": {"target_file": "target.json", "object": "gateway/control-state.json"},
 "s_ctl_ms": 2000, "poll_interval_ms": 200, "max_replicas": 2,
 "envelope_lifetime_ms": 5000,
 "public_key_bundle_file": "/run/keys/gateway-public.json",
 "signing_key_file": "/run/secrets/gateway-signing-key",
 "active_kid": "k1",
 "readiness_propagation_ms": 1500, "drain_timeout_ms": 5000,
 "max_request_bytes": 1048576, "max_in_flight": 64,
 "upstream_timeout_ms": 10000, "max_buckets": 1024}
```

| Field | Meaning and bounds |
| --- | --- |
| `bind`, `allow_remote` | Listener address. It must be loopback unless `allow_remote` is `true`. The gateway serves plain HTTP; put TLS in front of it. |
| `control_state` | `target_file`, `object` and optional `shared_key_file` (below). |
| `s_ctl_ms`, `poll_interval_ms` | Longest a confirmed control-state copy is used, and the poll period: positive, `poll_interval_ms` below `s_ctl_ms`. |
| `max_replicas` | Positive. The number of replicas each credential's rate limit is divided by. |
| `envelope_lifetime_ms` | Positive. Keep it within the backends' `max_lifetime_ms`, or they refuse the envelopes. |
| `public_key_bundle_file`, `signing_key_file`, `active_kid` | The bundle and signing key from `gateway-keygen`. The active key must be in the bundle and match the signing key. The signing key file is at most 1024 bytes. |
| `readiness_propagation_ms` | 0 or more. How long `/ready` reports `draining` before the listener closes. |
| `drain_timeout_ms` | Positive. How long in-flight requests may finish after the listener closes; they are then cancelled (`drain_timeout`), as described below. |
| `max_request_bytes` | 1–8388608. |
| `max_in_flight` | 1–65536. |
| `upstream_timeout_ms` | Positive. Bounds connecting, waiting for the response head and each wait for the next piece of a request or streamed response body, in either direction, not the whole request or response. |
| `max_buckets` | 1–1000000. |
| `dashboard` | Optional. Dashboard sign-in for tenant editor and viewer accounts (OD-004); see [below](#dashboard-sign-in-through-a-gateway-od-004-opt-in). Absent means no sign-in, sessions or dashboard, as before. |

Durations are at most 3600000 ms. No field has a default except
`allow_remote` (`false`) and the optional `dashboard` block: S_ctl, the poll
interval and the replica maximum must be chosen per deployment and are not
measured here.

On SIGTERM or Ctrl+C (Ctrl+Break on Windows), a gateway or trusted backend
logs `draining` and answers 503 `draining` on `/ready` for
`readiness_propagation_ms` while it still serves. It then closes the listener,
finishes in-flight requests (a gateway waits at most `drain_timeout_ms`),
stops its control-state poller and exits.

A gateway cancels every request still running when `drain_timeout_ms` passes
and logs `drain_timeout`. Nothing is retried or re-routed:

| State at the deadline | Answer |
| --- | --- |
| Not yet sent upstream (including a body still arriving) | 503 `draining` |
| A write already sent | 502 `write_outcome_unknown` (read before you repeat it) |
| A read already sent | 503 `draining` |
| A response already streaming | cut short |

Cancelled requests get at most one more second to release their slots, and
the control-state poller at most one more second to stop (a poll still
reading is left to finish on its own, logged `poller_stop_timeout`). A gateway
therefore exits within about `readiness_propagation_ms` + `drain_timeout_ms`
+ 2 seconds of the signal.

### Dashboard sign-in through a gateway (OD-004, opt-in)

Experimental, opt-in and not qualified on ACA. With a `dashboard` block, any
gateway replica can sign in a tenant's editor and viewer accounts and serve
the standalone dashboard, unchanged, so a person can inspect their own
tenant's data through the gateway. A session started on one replica works on
every other replica with the same users file and session key; the replicas
share no session state. Without the block a gateway behaves exactly as
described above: no sign-in, no cookie is ever read, and the dashboard and
public auth routes get 404.

```json
"dashboard": {"users_file": "/run/secrets/dashboard-users.json",
              "session_key_file": "/run/secrets/dashboard-session-key",
              "idle_timeout_ms": 1800000, "lifetime_ms": 28800000,
              "trusted_proxy": false}
```

| Field | Meaning and bounds |
| --- | --- |
| `users_file` | Required. A copy of the admin directory's `users.json` (`{"schema":1,"users":[...]}`, at most 1 MiB) that holds only tenant `editor` and `viewer` accounts. It contains Argon2id password hashes: deliver it as a secret file to gateways only, never in Blob, the control state or an image. A super admin or an account without `tenant_id` refuses startup. |
| `session_key_file` | Required. Base64 (standard or URL-safe) of at least 32 random bytes, file at most 1024 bytes. Signs new and re-issued cookies. Every replica must hold it, here or as an accepted key, and it is used for nothing else. It is never logged or echoed. |
| `accepted_session_key_files` | Optional, default none. At most 3 more key files in the same format that only verify cookies, never sign them, for [rotating the session key](#rotating-the-dashboard-session-key). Each must differ from the others and from `session_key_file`. |
| `idle_timeout_ms` | 300000 to `lifetime_ms`. Default 1800000 (30 minutes), or `lifetime_ms` if that is lower. |
| `lifetime_ms` | 300000–86400000. Default 28800000 (8 hours). |
| `trusted_proxy` | Default `false`. Set `true` only behind an HTTPS ingress that sets `X-Forwarded-Proto` and appends the client address to `X-Forwarded-For`, and that is the only way to reach the gateway. |

Unknown fields are refused. A bad block stops the gateway with
`gateway_config_invalid` and names the setting, never file content, for
example `dashboard.lifetime_ms must be 300000..=86400000`,
`cannot read dashboard users file: entity not found`,
`dashboard users file is larger than 1048576 bytes`,
`dashboard users file is not a valid users.json`,
`dashboard users file has no accounts`,
`... super admins and accounts without a tenant cannot sign in through a gateway`,
`cannot read dashboard session key: entity not found`,
`dashboard session key must be base64 of at least 32 random bytes`,
`dashboard accepted session key must be base64 of at least 32 random bytes`,
`dashboard.accepted_session_key_files may list at most 3 keys` or
`dashboard.accepted_session_key_files repeats a key or the session key`.
`gateway_started` reports `"dashboard": true` and `dashboard_session_kids`:
the key ids of the session key and then the accepted keys. A key id is 16
hex characters derived one-way from the key, safe to log and compare across
replicas; the key itself cannot be recovered from it.

**Transport.** Password sign-in needs a loopback `bind` or
`trusted_proxy: true`. Otherwise `GET /v1/auth/config` reports
`"password_login": false, "reason": "insecure_transport"` and sign-in gets
403 `insecure_transport`. The cookie is
`vlite_session=...; Path=/; HttpOnly; SameSite=Strict`. Behind a trusted
proxy a request with `X-Forwarded-Proto: https` gets
`__Host-vlite_session=...; Path=/; HttpOnly; SameSite=Strict; Secure`
instead, so serve the ingress over HTTPS only. The per-replica sign-in
throttle counts the client address only behind a trusted proxy.

**Routes the gateway answers itself.** These are never forwarded to a
backend.

| Route | Answer |
| --- | --- |
| `GET /dashboard` and its assets | The standalone dashboard files, unchanged. |
| `GET /v1/auth/config` | `{"mode":"protected","password_login":true,"reason":null,"setup_required":false}` |
| `POST /v1/auth/login` | `{"username","password"}`, body at most 16 KiB. The standalone per-account and per-address throttle (429 `too_many_attempts`) and Argon2id check (401 `invalid_credentials` for a wrong password or unknown name, 403 `account_disabled`). Then the tenant must be admitted (403 `tenant_suspended` or `tenant_unavailable`) and routed (503 `tenant_not_routed`), and an account that must change its password gets 403 `password_change_required`. 200 `{"user":{...},"expires_unix_ms":...,"idle_timeout_seconds":...,"must_change_password":false}` with the session cookie and `Cache-Control: no-store`. |
| `GET /v1/auth/session` | With the cookie and no `Authorization`: `{"method":"password","role":"scoped","user":{...},"expires_unix_ms":...,"idle_timeout_seconds":...,"must_change_password":false,"accounts":true,"setup_required":false}`. |
| `POST /v1/auth/password` | With the cookie: 409 `password_change_unavailable`. A gateway's accounts are a read-only export; change passwords on the admin server. |
| `POST /v1/auth/logout` | With the cookie: clears both cookie names, `{"signed_out":true}` (`false` if the cookie did not verify). |

`/v1/auth/session`, `/password` and `/logout` with an `Authorization` header
are API requests and are handled as before.

**Data requests.** A request without `Authorization` that carries the
session cookie is checked by whichever replica receives it, in this order:

1. CSRF, as standalone: a `Sec-Fetch-Site` other than `same-origin` or
   `none` gets 403 `csrf_rejected`, and a method other than GET, HEAD or
   OPTIONS also needs an `X-VLite-Request` header (the dashboard sends it).
   Sign-in and sign-out are checked the same way.
2. The cookie, `v1.<key id>.<claims>.<seal>`: the key id must name this
   replica's session key or an accepted key, the HMAC-SHA256 seal under
   that key must cover the key id and claims, issue and last-seen times may
   be no more than 60 s ahead, and the idle timeout and the lifetime (the
   lower of the cookie's and this replica's) must not have passed.
3. The control state: a session already served at a newer generation than
   this replica has read gets 503 `control_state_behind`; retry.
4. The account in this replica's users file. An account that has another
   version, is disabled, must change its password or moved to another
   tenant ends the session, and so does an unknown account. Two cases mean
   this replica's users file is older than the session instead, and get
   503 `dashboard_users_behind` during a rollout; retry: a cookie with a
   newer account version, and an unknown account created after every
   account in this file was created or last changed.
5. The tenant in the current snapshot: suspended gets 403
   `tenant_suspended`, deleted or unknown 403 `tenant_unavailable`, and no
   directory record 503 `tenant_not_routed`.

A session that ends gets 401 `session_expired`, both cookies cleared and
`Cache-Control: no-store`, and the dashboard returns to its sign-in form.
Otherwise the request continues at step 5 of [What a gateway
serves](#what-a-gateway-serves) as the account's standalone session scope
for its tenant: editors get `read`, `search`, `search_build`, `write`,
`import`, `index`, `collections`, `graph_read` and `graph_write`, viewers
`read`, `search` and `graph_read`, both limited by the account's graph
settings. Admin routes, `/metrics` and `/v1/database` get 403 as for a
standalone editor or viewer, and the tenant always comes from the account,
never from the request. The request is forwarded with a
`user:<id>` principal envelope that the backend checks again (see
[Gateway-trusted backend](#gateway-trusted-backend)). Once the cookie's last-seen
time is 60 s old, the response carries a re-issued cookie; this is the only
`Set-Cookie` a gateway sends with forwarded responses.

**Rollout.**

1. Upgrade every gateway-trusted backend before any gateway uses a
   `dashboard` block; older backends refuse `user:` envelopes with 401
   `envelope_invalid`.
2. Manage accounts on the admin server that owns the admin directory you
   publish the control state from (`POST /v1/admin/users` with `role`
   `editor` or `viewer` and the tenant's `tenant_id`, the same id as in the
   control state). An account with `must_change_password` set, the default
   for new accounts and after a password reset, gets 403
   `password_change_required` through a gateway; have the user change the
   password on the admin server before you export.
3. Export the accounts: copy `<admin dir>/users.json` and remove every
   `super_admin` and every account without `tenant_id`, for example
   `jq '.users |= map(select(.role != "super_admin" and .tenant_id != null))' users.json > dashboard-users.json`.
4. Create the session key once:
   `openssl rand -base64 32`, or in PowerShell
   `[Convert]::ToBase64String([Security.Cryptography.RandomNumberGenerator]::GetBytes(32))`.
5. Store both files as secrets (for ACA, app secrets mounted read-only as
   files, like the signing key; OD-009) and mount them on gateways only.
   Backends need neither. The ACA templates under `deploy/` do not mount
   them yet.
6. Add the `dashboard` block, set `trusted_proxy` to match the ingress, and
   restart every gateway replica. Check `gateway_started` for
   `"dashboard": true` and `GET /v1/auth/config` for `"password_login": true`.
7. Sign in at `https://<gateway>/dashboard` and check that the tenant's
   collections are listed.

**Ending sessions.** Sessions are stateless. Sign-out clears only that
browser's cookie; a copied cookie stays valid until its idle timeout or
lifetime. To end sessions earlier:

* One account: disable it, reset its password or change its role or tenant
  on the admin server (each bumps its version), re-export, update the secret
  and restart every gateway replica. Its sessions end on each replica that
  has the new file; replicas still holding the old file keep accepting them
  until they restart. Disable an account rather than removing it from the
  export: if it was the account created or changed most recently, its
  sessions get 503 `dashboard_users_behind` instead of ending, until they
  idle out or another account changes.
* A whole tenant: suspend it
  (`control-state update --suspend-tenant <id>`); every replica refuses its
  sessions within S_ctl. Suspension pauses sessions rather than ending them:
  after `--resume-tenant`, cookies that have not reached their idle timeout or
  lifetime work again. After a suspected cookie leak, also disable the
  affected accounts or replace the session key.
* Everyone: replace the session key, list no accepted keys, and restart
  every replica.

<a id="rotating-the-dashboard-session-key"></a>**Rotating the session
key.** To replace the key without signing everyone out, restart every
replica three times and check `dashboard_session_kids` in each
`gateway_started` before the next step:

1. Add the new key file to `accepted_session_key_files` on every replica.
   A replica that does not hold the new key ends every session another
   replica signed with it.
2. Make the new key `session_key_file` and list the old one in
   `accepted_session_key_files`. New cookies, and cookies re-issued after
   60 s of use, are now sealed with the new key.
3. After at least the idle timeout, remove the old key. Sessions used since
   step 2 carry the new key; any cookie still sealed with the old key ends.

**Limitations.** Super admins, accounts without a tenant and password
changes are not served through a gateway. The sign-in throttle is per
replica, so the effective limit grows with the replica count. Idle time and
cookie refresh have 60-second granularity. Sign-in and sign-out events
(`gateway_user_login`, `gateway_user_login_failed`, `gateway_user_logout`)
go to stderr; a gateway writes no audit log. Signed-in dashboard users have
no per-user `requests_per_second` limit at the gateway (as on a standalone
server, their session grant carries none); only `max_in_flight` bounds them.
Dashboard
assets do not count against `max_in_flight`. The tests run in-process
(gateway replicas and a gateway-trusted backend over loopback HTTP); no
multi-process, Azurite or ACA run covers this yet.

### Keys, control state and the tenant directory

**Keys.** Generate an Ed25519 key pair. The command refuses existing output
files and prints no key:

```sh
vlite-server gateway-keygen --kid k1 --generation 1 \
  --signing-key-out /run/secrets/gateway-signing-key \
  --public-bundle-out gateway-public.json
```

The signing key file is a base64url seed and goes only to gateways, through a
secret store. The public bundle
(`{"schema_version":1,"delivery_generation":1,"keys":[{"kid":"k1","public_key":"..."}]}`,
one or two keys) goes to gateways and backends. To rotate keys, give backends
a bundle with both keys, then switch the gateways' `active_kid` and signing
key, then remove the old key. Bundles are read only at startup. No key has a
default value.

**Control-state target.** `target_file` is the same target JSON used by the
cloud backup policy and by `cloud-sets` and `cloud-restore`: `account`,
`container`, `coordination_container`, `prefix`, optional `endpoint`, and
`auth` (`managed_identity` with an optional `client_id`, or
`emulator_shared_key` for a loopback Azurite only). The control state is the
object named by `object` under the coordination container's prefix. The name
uses `[a-z0-9._/-]`, has no empty, `.` or `..` segments, and cannot be
`owner.json`, `catalog.json` or under `sets/`. The emulator key comes from
`shared_key_file` or `VLITE_CLOUD_BACKUP_SHARED_KEY_FILE`.

**Publish and update.** A standalone server issues tenants and credentials
into its `VLITE_ADMIN_DIR` (`POST /v1/admin/tenants`,
`POST /v1/admin/credentials`). `control-state` imports that directory with
its existing compare-and-swap. You never edit the object by hand:

```sh
# Generation 1. Refused with control_state_exists if the object is present.
vlite-server control-state publish --target-file target.json \
  --object gateway/control-state.json --admin-dir /data/admin \
  --directory-file directory.json

# Generation A+1 with exactly one change. Give one of the four change flags,
# --directory-file, or one change flag plus --directory-file.
vlite-server control-state update --target-file target.json \
  --object gateway/control-state.json --admin-dir /data/admin \
  [--directory-file directory.json] \
  [--revoke-credential <id> | --suspend-tenant <id> | --resume-tenant <id> |
   --reimport-if-generation <A>]
```

Each prints `{"generation":A,"object":"..."}`. Errors print `{"error":...}`
on stderr and exit 1. An `update` with none of these flags is refused.

* `--revoke-credential`, `--suspend-tenant` and `--resume-tenant` change the
  admin directory first (audited with `source: "control-state-cli"`). They
  then make the same change to the published state and keep everything else
  in it. They never republish the rest of the admin directory, so an older or
  concurrent admin directory cannot undo another change. An id that is not
  published gets 404 `credential_not_found` or `tenant_not_found`, and
  nothing is published.
* `--reimport-if-generation <A>` publishes the admin directory's credentials
  and tenants, for example newly issued credentials or new tenants. It
  publishes only while the published generation is still `A`; otherwise it
  answers 409 `control_state_generation_mismatch` and publishes nothing.
  Use the generation printed by the last `publish` or `update`, or reported
  by a gateway's `/ready`. Restrictions already published stay even when
  the admin directory is older: a revoked credential stays revoked, the
  earlier expiry wins, and a suspended or deleted tenant stays so. Resume a
  tenant with `--resume-tenant`.
* `--directory-file` alone replaces only the tenant routes.

A lost race is re-read and the change is re-applied to the winning
generation (a re-import is then refused, because the generation moved). An
unknown store outcome answers `control_state_indeterminate` without a retry.
Repeating a revoke, suspend or resume after that is safe: the change is
idempotent and publishes another generation. Only one `control-state`
command runs at a time per admin directory. A second one answers
`admin_dir_busy`, which comes from the `control-state.lock` file in that
directory. A standalone server does not take that lock, so do not run the
command while a server uses the directory. Each gateway and
backend picks up a new generation at its next successful poll; a backend then
refuses envelopes minted from an older generation. Until every gateway has
polled, some requests can therefore get 401 `envelope_stale`; callers may
retry them.

**Directory file.** A JSON array of at most 1 MiB with one record per tenant;
unknown fields are refused (422 `directory_invalid`):

```json
[{"tenant": "<tenant id>", "kind": "shared-pool", "writer": "http://10.0.0.5:8081",
  "audience": "shared-pool", "lineage": 1311768467294899695},
 {"tenant": "<tenant id>", "kind": "dedicated", "writer": "http://10.0.0.6:8081",
  "audience": "tenant-three", "lineage": 81985529216486895, "epoch": 1}]
```

* `writer` is a scheme, host and port only.
* `audience` must equal the target backend's trust-file `audience`.
* `lineage` is the backend database's lineage as a decimal number. Credential
  responses and `GET /v1/admin/serve-status` show it as hex, so convert it
  (`printf '%u\n' 0x<hex>`).
* `dedicated` records need a positive `epoch`. `shared-pool` records have no
  `epoch` and no `reader`. An authority-enabled writer stamps responses with
  `X-VLite-Epoch` (its claimed epoch). An HA reader stamps the epoch of the
  archived head it follows, once known. For a `dedicated` record, the
  gateway requires exactly one canonical stamp that is at least both the
  dispatched record's epoch and the current directory record's epoch. A
  missing, malformed or older stamp is refused finally, with no other
  backend tried: a read gets 503 `backend_epoch_stale`, and a write gets 502
  `write_outcome_unknown` because the deposed writer may have applied it.
* A `dedicated` record may also carry a `reader` and `read_routing`
  (`writer_only`, the default, or `readers`, which needs a `reader`). Any
  record may carry `binding_generation` (default 0). With `read_routing:
  "readers"`, the gateway sends reads to the record's reader and falls back
  to the writer on connect failure/timeout, a 503, or a missing or mismatched
  `X-VLite-Lineage` or `X-VLite-Binding-Generation`, or a missing, malformed
  or older `X-VLite-Epoch`. A reader following a prepared transfer stamps
  the older base epoch, so its reads also fall back to the writer. The
  writer's answer is still epoch-checked finally as above. Writes always go
  to the writer; `writer_only` keeps all requests on the writer. No real
  gateway-plus-reader process test qualifies `readers` routing yet: keep
  `writer_only`.
* A record for a missing or deleted tenant, a duplicate tenant or the wrong
  shape for its kind is refused (`control_state_invalid`).

**Limits of this MVP.** A trusted backend has no admin directory and no admin
path: trusted authentication grants only scoped tenant roles, so admin routes
such as `GET /v1/admin/serve-status` and forced unpin are unavailable on it.
Without writer authority, to issue new tenants or credentials, stop the
backend, run the same database as a standalone server with the admin
directory, issue them, stop it, run `control-state update` and restart the
trusted backend. With writer authority enabled, this maintenance is
**unsupported** until a reviewed procedure exists: stopping a writer does not
release its authority, and a later claim is refused with
`writer_authority_claimed`. Do not drop the authority options, run the
database as a standalone writer to work around a claim, or assume that a
stopped process released authority; use only the manual
[writer failover](HA-ACA.md#planned-or-unplanned-writer-failover), which
restores the archived base. Revocation, suspension, resumption and directory
changes need no restart. Rate-limit shares assume at most `max_replicas`
gateways: more replicas, for example while a rollout overlaps old and new
replicas, let a credential exceed its limit in proportion, and this overshoot
is not qualified. A request body is held in memory up to `max_request_bytes`.
In trusted mode, `/v1/auth/config` and the capabilities still describe the
local token configuration.

## Managed tenant deployments (intent only, opt-in, off by default)

These admin routes record durable, idempotent tenant lifecycle intents in the
control-state store. They do **not** provision, deploy, route or stop any
Azure resource. There is no production ARM adapter, the control worker refuses before any claim or cloud call in production, and there is no
ACA-qualified behavior. The operator runbook and its local test evidence are
in [HA-ACA](HA-ACA.md#managed-tenant-deployments-local-intent-only-not-aca-qualified).

The routes are off by default. Without
`vlite-server <db> --tenant-deployments-file <json>`
(`{"target_file", "object", "shared_key_file"?}`, the `control-state` source
above) authenticated, well-formed requests answer 409
`tenant_deployments_not_configured`. Authentication and body validation can
reject a request before this check. Enabling them
needs `VLITE_ADMIN_DIR` and an admin token, and refuses `--insecure-local` and
`--durability none`. The server then holds the admin directory's lock, and
`vlite-server tenant-deployment ...` (same contracts, offline) answers
`admin_dir_busy`. Requests without admin credentials get
401 `admin_auth_required`.

| Route | Effect today |
|-------|--------------|
| `POST /v1/admin/tenant-deployments` | `create`: records the intent, creates unrouted tenant metadata and reserves the tenant (202, phase `reserved`). Cloud execution refuses (OD-002). |
| `POST /v1/admin/tenant-deployments/{tenant}/readers` | Records a reader change (202, phase `accepted`, `observed.reserved: false`). Nothing runs it. |
| `POST /v1/admin/tenant-deployments/{tenant}/retire` | Records a retirement naming `expected_receipt` (`data_disposition: "retain"` only). Nothing runs it. |
| `POST /v1/admin/tenant-deployments/{tenant}/migrations` | **Excluded.** Records an unreserved intent that nothing runs. Migration is not implemented. Do not use it. |
| `GET /v1/admin/tenant-deployment-operations` | Lists the redacted status of operations from the records (not live Azure state), ascending by operation ID: `limit` (1-100, default 50) and an exclusive `after` operation ID page it, and `next_after` names the next `after` (null on the last page). |
| `GET /v1/admin/tenant-deployment-operations/{operation}` | Status: `phase`, `next_step`, generation tokens, `desired` and `observed` from the records (not live Azure state). |
| `POST /v1/admin/tenant-deployment-operations/{operation}/{action}` | `resume` (no-op, never cloud execution) and `cancel`. Every other action is refused (below). |

Submission responses are 202 while the operation is not terminal; an
identical retry of an ended operation returns 200 with its recorded state.

**Submission body:** `{"idempotency_key", "request": {"kind": ...},
"contents": [...]}`, where `contents` holds the exact text of every
referenced profile. Retry through the **same surface and principal**:
idempotency is scoped to the principal and key, not the key alone. The CLI
uses `cli:tenant-deployment`; REST uses the authenticated admin principal.
Switching surfaces or admin principals is a new operation and can allocate
another unrouted tenant for `create`. Within the same principal's namespace,
a duplicate returns the same operation; the same key with different content
answers 409 `operation_request_conflict`. A path/body
mismatch answers 422 `operation_request_invalid`.

**Action body:** `{"idempotency_key", "expected_operation_generation",
"expected_lifecycle_generation", "expected_receipt"?, "ack_irreversible"?}`,
using tokens from the latest status. Results:

* Stale tokens: 409 `operation_generation_stale`. The same key with another
  request: 409 `action_request_conflict`. Unknown action: 404
  `tenant_deployment_action_unknown`. Unknown operation: 404
  `operation_not_found`.
* `resume` of a `present` reader change: 409 `pin_scope_unproven` (ITEM-008).
* `cancel`: seals and releases a reserved, unactivated operation. It answers
  409 `retirement_required` once activated and `operation_terminal` once
  ended. An accepted reader change or retirement that holds no reservation
  is withdrawn instead, in the same control-state compare-and-swap that
  would reserve it, then sealed `cancelled`. It can never reserve afterwards
  and the slot and route are unchanged. If its reservation won first, the
  cancel goes through that reservation instead (refused with
  `readers_in_progress`, `retirement_in_progress` or `retirement_required`
  once the change has progressed) and nothing is withdrawn. Withdrawals are
  never pruned; past 1,024 in the control state, cancel answers 409
  `withdrawal_capacity` and writes nothing. An unreserved `create` or
  `migrate` answers 409 `transition_unavailable`.
* `acknowledge-delivery`, `replace-credentials`, `rollback` and `release`:
  409 `transition_unavailable`. `release` without `ack_irreversible: true`
  answers 422 `irreversible_ack_required` first.

Responses never include the idempotency key, profile contents, secrets or
tokens. Audit events are `tenant_deployment_accepted` and
`tenant_deployment_action`.
