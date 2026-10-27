# REST API reference

The server bundles its current machine-readable contract at `/openapi.json`.
Protected routes take a bearer credential and apply the deny-by-default
scoped-credential policy; the dashboard sends its session cookie instead.
Collection routes keep their collection-bound authorization checks. Errors use
the `{"error":{"code":"...","message":"..."}}` envelope.

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
past the head of the log answers 409 `cursor_ahead`. The change feed and
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
