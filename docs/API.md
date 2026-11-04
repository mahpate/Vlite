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
412 `precondition_failed`, including a missing/due key. An unconditional or
absence-conditional delete of a missing key succeeds with `deleted:false`.

Versions and deadlines are canonical uint64 **decimal strings**, without signs
or leading zeroes except `"0"` (maximum `"18446744073709551615"`). Expected
revision `"0"` is valid but cannot match a live entry. Optional
`expires_at_ms` is an absolute Unix-millisecond string or null; omitted/null
clears expiry. Already-due puts succeed, then read absent. Expiry needs v5.
Namespaces are 1..255 UTF-8 bytes and keys 1..1,024; strings are not normalized.
Values are canonical padded standard base64, at most **1,047,290 decoded bytes**
(1,396,388 encoded characters); an empty value is valid. Unknown/duplicate JSON
fields and wrong types are rejected with `invalid_json`; parsed bound or
canonical-encoding violations return 422 `invalid_input`. Unknown/duplicate
query parameters return 400 `invalid_query`. The existing 8 MiB body cap applies.

List `prefix` defaults to empty (at most 1,024 UTF-8 bytes), `after` is omitted
or a nonempty key, and `limit` is 1..10,000 (default 100). Keys sort by unsigned
UTF-8 bytes; `after` is exclusive. A page is the largest prefix fitting both
`limit` and **16,777,216 bytes of compact canonical JSON**, including the
`{"items":[...],"next_after":...}` envelope. `next_after` is the last returned
key only when another live matching entry exists at that page's clock;
otherwise it is null, including empty pages. One owner guard and expiry clock
cover a page; no snapshot spans pages.

Missing/due reads return 404 `kv_not_found`; a collection with other dimensions
returns 409 `not_kv_collection`. Sixteen occupied collision-probe slots return
409 `kv_slots_exhausted`; expiry before v5 returns 409 `upgrade_required`.
Operation scope, tenant and immutable collection bindings apply before lookup.
Ordinary record quotas also count KV entries. Mutations are audited without
keys, namespaces or values. Read-only handles refuse mutations.

Ordinary KV follows configured durability: `full` syncs before acknowledgment,
`periodic` can lose up to a flush interval on OS crash/power loss, and `none` is
memory-only. There is no per-call forced sync, automatic retry or lease
guarantee; disconnects and `commit_indeterminate` do not prove a write failed.
gRPC, MCP, CLI, client helpers and lease operations are not part of this REST
surface.

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
