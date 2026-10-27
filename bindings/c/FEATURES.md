# Additive embedded feature JSON contract (ABI 1)

No new native symbols or allocators are required. Send UTF-8 JSON to
`vlite_execute(handle, input, length)`:

```json
{"op":"features","request":{"op":"put_with_expiry","id":"1","vector":[1,0],"expires_at_ms":"2000000000000","metadata":{}}}
```

The response and ownership rules are unchanged: `{"ok":true,"result":...}` or
`{"ok":false,"error":{"code":"...","message":"..."}}`. Free each non-null
response exactly once with `vlite_response_free`, from the same loaded library.
`vlite_abi_version()` remains 1. All old operations and default format-3 creation
remain unchanged. The new `features` discriminator requires a matching new
library; old libraries reject it, never silently ignore it.

## Types and limits

IDs, revisions, feed heads, lineages, key IDs, timestamps and restore targets
are **canonical decimal u64 strings** (no signs or leading zeros). Counts,
indices, distances and dimensions are JSON numbers. Sparse vectors are sorted
unique `[u32_index, finite_f32_weight]` pairs. Dense vectors are f32 arrays.
Metadata uses the existing `{field:{"type":"string","value":"x"}}` shape;
integer values are i64 decimal strings, and geo values are
`{"type":"geopoint","value":{"latitude":0,"longitude":0}}`. Filters remain
AND arrays of `eq`, `range`, and `georadius` conditions (`radius_meters`).
Unknown request fields and enum values are errors. Requests and feature responses
are bounded to 8 MiB including JSON/base64 overhead. Large byte images cannot
cross this ABI; no streaming image API is provided. Feed commits are never split,
so a large batch can exceed the requested event count (or the response bound).

All collection-specific operations accept optional `collection` (default
`"default"`). All new retrieval methods return result arrays directly, not a
`hits` envelope, except `query`, which returns `{hits,groups,facets}`. Hits use
`{id,distance}` with lower distance better; dense/sparse fusion returns existing
hybrid hits with descending RRF scores. Default metrics use collection settings
except `configure_vector`, which defaults to squared L2.

## Handle-zero operations

- `create`: `dimensions`, optional `path` (null/omitted for memory), `options`:
  `{format?:3|4|5, credentials?:{key_base64?:string,passphrase?:string}, commit_timestamps?:bool, durability?:"periodic"|"full"|"none", flush_interval_ms?:number}`.
  Returns `{handle}`. A raw key is exactly 32 bytes, padded base64. Key and
  passphrase are mutually exclusive. Encryption/timestamps imply V5 unless an
  incompatible explicit format is provided. Default remains V3.
  File databases default to periodic durability every 60 s (`flush_interval_ms`
  1–60000): an OS crash or power loss can lose acknowledged writes from roughly
  the last interval. `"full"` syncs every commit; `"none"` is the only mode for
  in-memory databases. Use the base `{"op":"sync"}` request to force a sync; see
  `bindings/README.md`.
- `open`: `path`, optional `credentials`, `durability`, `flush_interval_ms`; returns `{handle}`.
- `open_from_bytes`: `bytes_base64`, optional `credentials`; returns `{handle}`.
- `restore_to`: `log_path`, new `destination`, optional `base_backup`, exactly
  one of `sequence` / `timestamp_ms`, optional `credentials`. Returns
  `{restored_sequence,source_head,restored_at_ms}`. Sources must be closed/copied;
  no overwrite or implicit repair. The base must be an exact log prefix with the
  same lineage. Timestamp restore needs commit timestamps. Restores get a new
  lineage. Source files are never changed.

Use ordinary `close` on any returned database handle.

## Database-handle operations

| Operation | Fields | Result |
|---|---|---|
| `put_with_expiry` | `id`, `vector`, optional `payload_base64`, `metadata`, `expires_at_ms` | `{revision}` |
| `update_metadata` | `id`, `metadata` | `{revision}` |
| `search_grouped` | `vector`, `groups`, `hits_per_group`, `field`, optional `filter`, `metric` | `[{value,hits}]` |
| `recommend` | `k`, optional `positive_ids`, `negative_ids`, `positive_vectors`, `negative_vectors`, `filter`, `metric` | hits |
| `facets` | `field`, optional `filter` | `[{value,count}]` |
| `numeric_stats` | `field`, optional `filter` | `{count,min,max,sum,average}` |
| `search_geo` | `vector`, `k`, `field`, `latitude`, `longitude`, `radius_meters`, optional `filter`, `metric`, `sort` (`vector_distance`/`geo_distance`) | `[{id,vector_distance,geo_distance_meters}]` |
| `configure_vector` | `name`, `dimensions`, `active_version`, optional `metric`, `history` | null |
| `vector_specs` | none | name to `{dimensions,metric,active_version,history}` map |
| `record_vectors` | `id` | `{dense,sparse}` or null |
| `put_vectors` | `id`, optional `dense`, `sparse` | null |
| `put_vector_version` | `id`, `name`, `version`, `vector` | null |
| `prune_vector_history` | `name`, `history` | null |
| `search_named` | `name`, `vector`, `k`, optional `version` | hits |
| `search_sparse` | `name`, sparse `vector`, `k` | hits |
| `search_dense_sparse` | `dense_name`, `dense`, `sparse_name`, `sparse`, `k`, optional `version`, `fusion` | hybrid hits |
| `rerank` | `vector`, `candidates:[{id,distance?}]`, `limit`, optional `method` (`exact`/`mmr`), `metric`, `lambda` | hits |
| `query` | `query`, optional vector `parameters` map | `{hits,groups,facets}` |
| `feed_cursor` | none | opaque checksummed token |
| `changes_after` | optional `cursor` (null/omitted = beginning), `max_events` (default 1000) | `{changes,next,has_more,head}`; graph files add `graph_created`, `graph_dropped` and `graph_batch` kinds ([GRAPH.md](../../docs/GRAPH.md#change-feed-and-webhooks)) |
| `encryption_info` | none | `{is_encrypted,key_id}` |
| `build_disk_ann` | `path`, optional `collection`, `metric` | `{lineage,sequence,collection_id,live_records}` |
| `search_disk_ann` | `path`, `vector`, `k`, optional `ef_search` (100), `cache_bytes` (8 MiB) | hits |
| `save_ann_snapshot` | `path` | `{indexes,bytes}`; refused for encrypted databases |
| `load_ann_snapshot` | `path` | `{status,indexes}`; `status` is `loaded`, `missing` or a rejection code, and only `loaded` installs indexes |
| `to_bytes` | none | `{bytes_base64}` (memory databases only) |

`put_with_expiry` and `update_metadata` require V5. Expiry is absolute Unix
milliseconds; null/omitted means no expiry. Expiry writes intentionally do not
accept revision preconditions. Metadata updates replace metadata only, retaining
base/named vectors, payload and expiry. Expired records are excluded from normal
retrieval. Group counts and hits per group are capped at 1000 each.

Named/sparse vectors require V5 and a live base record. `dense` maps names to
`[{version,vector}]`, `sparse` maps names to sparse vectors. `put_vectors` replaces
**all** additional vectors on a record; `put_vector_version` changes one version.
`history` retains 0..64 old versions beyond the active version. Historical
search uses `version`; omitted means active. Base replacement/deletion clears
additional vectors; metadata-only replacement retains them. Sparse distance is
negative dot product. `fusion` accepts the existing snake_case fields
`candidates`, `rrf_k`, `vector_weight`, `text_weight`.

Rerank resolves up to 4096 current live base records from candidate IDs, then
uses full precision. Exact rerank supports all metrics. MMR always uses cosine,
requires `lambda` in [0,1] (default .5), and rejects `metric` to avoid silently
ignoring it. Exact mode rejects `lambda`. No provider callbacks are exposed.
The read-only query grammar is documented in `docs/VECTORS.md`; syntax failures
return `query_parse_error` with a byte position in the message.

Change events contain `sequence`, `ordinal`, `collection`, `committed_at_ms` and
`kind`. Put events include `id`, full `record`, `expires_at_ms`; delete includes
`id`; metadata update includes `id`, `metadata`; collection-created includes
`dimensions`, `settings`; settings-updated includes `settings`. Dropped and
vectors-updated events have no body. Expiry itself is not an event. Feed reads
scan the log; callers persist `next` and resync on lineage changes.

Disk ANN opens/closes the sidecar per search (cache is per call, not retained).
It refuses stale indices after any commit, and refuses encrypted databases
because sidecars contain plaintext vectors. It does **not** filter metadata or
expiry. Build uses ordinary in-memory HNSW resources. No replica, GPU,
re-embedding-worker or named-ANN binding is provided.

## Failure/security behavior

Engine failures retain existing codes and add `requires_format_v5`,
`encryption_key_required`, `encryption_key_mismatch`, `not_encrypted`,
`invalid_encryption_options`, `change_feed_requires_upgrade`, `invalid_cursor`,
`cursor_ahead`, `cursor_compacted`, `timestamps_unavailable`,
`invalid_restore_target`, `stale_index`, `sidecar_unavailable`,
`publication_indeterminate`, `query_parse_error` and `invalid_durability`.
`response_too_large` means the result cannot cross the JSON bound; use smaller
result sets. Never automatically retry indeterminate mutations.

Credentials are passed in process, never over HTTP. Encryption protects frame
contents, not plaintext frame prefixes, timing/file sizes, swap, crash dumps or
live process memory. JSON/worker/Python credential copies cannot be reliably
wiped; protect process access and do not log requests. Keys are not returned;
key IDs are non-secret. Passphrases use engine Argon2id defaults and stored KDF
parameters. Losing keys loses data. In-memory databases are durable only after
the host saves `to_bytes()` output.

## Property graph

`{"op":"capabilities"}` (any handle, no other fields) returns `{"graph": bool}`.
Graph commands are carried as `{"op":"graph","request":{"op":..., ...}}` and are
compiled into every library since v0.5.1 (the `graph` feature is a no-op
alias). Libraries from v0.5.0 built without it return `unsupported` for every
graph request (libraries predating this envelope return `invalid_input` for the
unknown `graph` op). The JSON mirrors the
`vlite-graph-cli`: node/edge/record IDs, revisions and sequences are decimal
strings (JSON integers are also accepted on input), unknown fields are rejected,
and node/edge selectors are `{"id":"…"}` or `{"key":"…"}`.

| `op` | Handle | Fields → result |
|---|---|---|
| `create` | 0 | `path`, `dimensions`, optional `options` (as feature `create`: `format`, `credentials`, `commit_timestamps`) → `{handle}`; creates a NEW V5 graph-enabled file, never converts |
| `is_graph_enabled` | db | → `{graph_enabled}` |
| `list_graphs` | db | → `{graphs:[{name,node_count,edge_count}]}` |
| `create_graph` / `drop_graph` | db | `graph` → `{graph:{…}}` / `{dropped}`; drop requires an empty graph |
| `batch` | db | `graph`, `ops` → `{graph, revision, outcomes}`; one atomic commit |
| `get_node` / `get_edge` | db | `graph`, `node` / `edge` |
| `find_nodes` | db | `graph`, optional `label`, `conditions`, `limit`, `cursor` |
| `neighbors` | db | `graph`, `node`, optional `direction` (`out`/`in`/`both`), `types`, `edge_conditions`, `limit`, `cursor` |
| `traverse` | db | `graph`, `start` (selectors), optional `direction`, `types`, `expand_filter`, `return_filter` (`{labels, conditions}`), `edge_filter`, `include_paths` |
| `shortest_path` | db | `graph`, `source`, `destination`, optional `direction`, `types` |
| `explain` | db | `query` (one read request above, with its `op`) → `{graph, explain:{query, limits, projection, indexes}}` |

Every read also accepts `limits` (`max_hops`, `max_visited_nodes`,
`max_edges_examined`, `max_edges_per_node`, `deadline_ms`, `max_output_nodes`,
`max_output_edges`, `max_output_paths`; unset fields use engine defaults, values
above the ceilings fail) and `projection` (`"all"`, `"none"`, `{"only":[…]}`).
Reads return `graph`, `view_sequence`, `complete`, `truncation`, `stats` and a
lowercase-hex `cursor` (or null) plus `node`, `edge`, `nodes`, `edges`, `paths`
or `shortest_path` (`outcome`: `found`, `no_path_within_hops`, `exhausted`).
Batch `ops` are `create_node`, `upsert_node`, `patch_node`, `delete_node`,
`create_edge`, `patch_edge`, `delete_edge`, `attach`, `detach`, `put_vector`
(`collection`, `id`, `vector`, optional `payload_base64`, `metadata`) and
`delete_vector`; `expected` is `"any"`, `"absent"` or `{"revision":"…"}`.

Graph errors use the REST names: `graphs_not_enabled`, `graphs_require_v5`,
`invalid_graph_name`, `graph_exists`, `graph_not_found`, `graph_not_empty`,
`too_many_graphs`, `graph_key_exists`, `graph_key_conflict`,
`graph_node_not_found`, `graph_edge_not_found`, `graph_node_has_edges`,
`graph_attachment_conflict`, `graph_attachment_not_found`,
`graph_record_not_attachable`, `graph_attached_record`,
`graph_attached_record_expiry`, `graph_limit_exceeds_ceiling`,
`graph_cursor_stale`, `graph_change_feed_unsupported`,
`graph_unsupported_operation` and `graph_invalid_options`. Other failures keep
the existing codes (`read_only`, `locked`, `invalid_input`, …); responses over
8 MiB return `response_too_large`. A library without the feature opening a
graph-enabled file keeps the existing `invalid_input` rejection.

## Optional float32 input entry points

`vlite_search_f32` and `vlite_batch_put_f32` are additive, optional ABI-1 exports
declared in `vlite.h`. They do not replace `vlite_execute`; detect their presence
with the platform's dynamic symbol lookup before calling against an older
library. Existing published 0.3.0 binaries lack these symbols. The source Python
wrapper detects them and offers JSON fallback or explicit `unsupported` errors.

Both borrow immutable JSON options and an aligned native-endian IEEE float32
buffer until return, with the same owned JSON envelope and
`vlite_response_free` allocator. Invalid non-null/inaccessible pointers remain
caller undefined behavior: length checks cannot establish pointer validity.
Null/misaligned float pointers, excessive counts, nonfinite values, invalid JSON
and mismatched bulk dimensions/counts are rejected as `invalid_input` before
mutation. A null float pointer is permitted only for count zero. JSON options
are independently limited to 8 MiB and vectors to 2097152 float32 values (8 MiB).

Search accepts an existing `{"op":"search",...}` or
`{"op":"search_approximate_ready",...}` JSON request with `vector` omitted and
inserts the borrowed vector into the existing execution path. Supplying both a
JSON vector and a typed vector is an error. Exact, auto, hybrid and ready-only
approximate engine validation and option/response rules are unchanged; a JSON
`search_approximate_ready` request without `vector` remains `invalid_input`.
Libraries built before ready-only typed search reject that request as
`invalid_input`.

Bulk options have this shape (no `op` or vector fields):

```json
{"dimensions":2,"operations":[{"id":"1"},{"id":"2","payload_base64":"AP8="}],"collection":"default"}
```

Each row accepts only `id`, optional `payload_base64` and optional `metadata`.
Floats are row-major, dimensions must be positive, and the count must equal
`operations.length * dimensions` without overflow, with at most 4096 rows.
An empty row list accepts a zero-length buffer. Puts go through the existing
atomic batch implementation, including validation, read-only protection,
serialization and the handle's durability mode. Input vectors are copied into
owned operations before execution; neither call retains caller pointers.
No owner locks, file locks or durability policies are relaxed.
