# VLite 0.7.0 upgrade and rollback

VLite 0.7.0 adds a key-value store, an opt-in directory store, graph expiry, bounded graph algorithms and optional managed embedding and ranking providers.

<!-- source: docs/STORAGE-FEATURES.md#key-value-store, docs/STORAGE-FEATURES.md#directory-store, docs/GRAPH.md#expiry-ttl, docs/GRPC.md, site/api.html -->

It follows [0.6.1](RELEASE-0.6.1.md). Most additions are embedded Rust APIs. Existing files open as before, new files are written as before, and every new capability is opt-in. One Rust source-level change affects code that matches `GraphChange` exhaustively (see [Compatibility](#compatibility)). This release makes no new performance claims. <!-- source: docs/STORAGE-FEATURES.md, docs/GRAPH.md, docs/GRPC.md, tests/graph_regression.rs, src/property_graph/change.rs -->

> **Draft for release-candidate review.** The version number is provisional. Resolve or delete every `[TO CONFIRM AT RC]`, `[TO FILL AT RC]` and `[VERIFY]` marker before publishing. A `<!-- source: ... -->` comment names the repository document or test that backs the statement before it.

## What is new

### Key-value store

Keep values under namespaced string keys, with no vector. <!-- source: docs/STORAGE-FEATURES.md#key-value-store -->

- **Embedded Rust API.** `Database::kv_store` and `Database::kv_view` give you
  namespaced string keys with byte values, ordered prefix listing, optional
  expiry, compare-and-set and fencing tokens. An entry is an ordinary record in
  a one-dimension collection, so the file format does not change. The store
  needs a format 4 or newer file, and expiry needs format 5. Namespaces are up
  to 255 bytes, keys up to 1,024 bytes and values just under 1 MiB, and a list
  page holds up to 10,000 keys. A version is a commit sequence, so read an
  entry again after compaction, upgrade or restore before you use its version.
  See [Key-value store](STORAGE-FEATURES.md#key-value-store).
  <!-- source: docs/STORAGE-FEATURES.md#key-value-store, src/kv.rs, tests/kv.rs -->
- **Shell and MCP.** `vlite-shell --db PATH` accepts `kv-get`, `kv-put`,
  `kv-delete` and `kv-list`. `vlite-mcp` adds `vlite_kv_get` and
  `vlite_kv_list`, and with `--writable` also `vlite_kv_put` and
  `vlite_kv_delete`. They work on a local file only (not through `--server`),
  need an existing one-dimension collection and never create one, and have no
  lease commands. Use dedicated collections for key-value data, because vector,
  import and graph operations on the same collection can overwrite its
  entries. See [shell commands](TOOLS.md#local-key-value-shell-commands) and
  [MCP tools](MCP.md#local-key-value-tools).
  <!-- source: docs/TOOLS.md#local-key-value-shell-commands, docs/MCP.md#local-key-value-tools, tests/kv_tools.rs, tests/fixtures/kv/mcp-tool-additions.json -->
- **HTTP clients.** [VERIFY] The Python, JavaScript and .NET clients include
  key-value methods (`kv_get`, `kv_put`, `kv_delete` and `kv_list`; `kvGet` and
  so on; `KvGetAsync` and so on). They call routes under
  `/v1/collections/{collection}/kv/`, and the `vlite-server` in this draft
  serves none, so the methods cannot succeed against it yet. Keep this item
  only if the REST routes below are confirmed.
  <!-- source: clients/README.md, clients/python/README.md, clients/python/vlite_client/__init__.py (no kv route exists in src/openapi.json, src/server.rs or proto/vlite/v1/vlite.proto) -->
- **REST.** [TO CONFIRM AT RC] `vlite-server` serves the key-value store over
  REST under `/v1/collections/{collection}/kv/`.
  <!-- source: docs/projects/roadmap/serving-kv-ttl-contract.md (proposed schema, section 4; not implemented at this draft) -->
- **gRPC.** [TO CONFIRM AT RC] The gRPC service exposes the key-value store.
  <!-- source: docs/projects/roadmap/serving-kv-ttl-contract.md (proposed proto additions, section 7; not implemented at this draft) -->

### Directory store (opt-in)

Keep a database as a directory, so that reopening loads a checkpoint image and
replays only the active log instead of the whole history.
<!-- source: docs/STORAGE-FEATURES.md#directory-store, docs/projects/roadmap/p2-directory-store.md -->

- **Create or convert.** `Database::create_directory(path, dimensions, options)`
  creates a format 5 database as a directory of write-ahead logs and an
  optional checkpoint image, listed by an atomic manifest.
  `maintenance::convert_to_directory` (and `convert_to_directory_with_key` for
  an encrypted file) turns a closed format 5 file into one, and
  `maintenance::backup` packs a directory back into one file. Creating a
  directory store is a Rust API. `Database::open`, `open_read_only`, the server
  and the maintenance tools accept the directory path unchanged. Commit
  timestamps and formats 3 and 4 are not supported.
  See [Directory store](STORAGE-FEATURES.md#directory-store).
  <!-- source: docs/STORAGE-FEATURES.md#directory-store, docs/projects/roadmap/p2-directory-store.md, tests/directory_store.rs -->
- **Checkpoints.** Once the active log passes 64 MiB, a commit starts a
  background checkpoint that folds the logs into one image.
  `Database::set_checkpoint_threshold` changes or turns off that threshold, and
  `Database::checkpoint` runs one and waits. The lineage, every record's
  revision (so compare-and-set and fencing tokens keep matching), graphs and
  encryption are kept.
  <!-- source: docs/STORAGE-FEATURES.md#directory-store, tests/directory_store.rs, tests/directory_recovery_matrix.rs -->
- **History below a checkpoint is gone.** A change-feed or log-segment request
  from a cursor below the checkpoint floor returns `CursorBeforeHistory` (REST
  409 `cursor_before_history`). Take a snapshot and resume from the floor. A
  new replica of a checkpointed store starts from a backup.
  <!-- source: docs/STORAGE-FEATURES.md#directory-store, docs/API.md, tests/directory_store.rs -->

### Graph expiry (TTL) and change feeds (embedded Rust API)

- **Expiry.** `GraphMut::set_node_expiry` and `set_edge_expiry` (or
  `GraphOp::SetNodeExpiry` and `SetEdgeExpiry` in a batch) set or clear a
  deadline in milliseconds since the Unix epoch. From its deadline a node is
  hidden together with every edge of it, and an edge's own deadline hides only
  that edge, so no dangling edge is ever visible or stored. Reads hide what is
  due at the read clock (`GraphRef::with_clock` fixes it; it decides
  visibility only and is not time travel). Each graph write first removes what
  is due, in the same atomic frame and as one computed entry, so the
  4,096-operation limit is unaffected. Compaction, `upgrade`, directory
  checkpoints and `convert_to_directory` drop what is due, and deadlines
  survive reopen and backup. Records attached to a node still cannot expire.
  See [Expiry](GRAPH.md#expiry-ttl).
  <!-- source: docs/GRAPH.md#expiry-ttl, docs/STORAGE-FEATURES.md#graph-enabled-files, tests/graph_ttl.rs -->
- **Change feed.** `Database::graph_changes_after`,
  `changes::read_log_graph_changes` and `changes::GraphFeedCursor` read an
  opt-in graph feed that reports deadline changes and expiry removals. The
  default feed (`changes_after`) and its bytes are unchanged. A cursor is bound
  to one event set (`Empty` or `Ttl`), and a page can end inside a large expiry
  removal and resume there. A checkpoint or compaction image omits entities that
  are already due without logging a delete, so the feed reports no removal for
  them; reads already hide them.
  <!-- source: docs/projects/roadmap/gap3-ttl-feed.md, site/api.html, tests/graph_ttl_feed.rs, tests/graph_ttl_paths.rs -->
- **Where it is available.** Rust only. REST, gRPC, MCP, the graph CLI and the
  bindings do not expose expiry or the TTL-aware feed.
  <!-- source: docs/GRAPH.md#expiry-ttl, docs/projects/roadmap/gap3-ttl-feed.md -->

### Bounded graph algorithms (embedded Rust API)

`GraphRef::algorithm` runs one algorithm on one explicit local scope: degree
counts, hop-constrained weighted shortest paths, PageRank, label propagation
or sampled betweenness. The scope is a bounded set of seed nodes with a
direction, relationship types, labels and edge conditions. Work, scratch
memory and iterations are capped (defaults 100,000 operations, 64 MiB, 20
iterations and 16 sampled sources; ceilings 1,000,000, 256 MiB, 100 and 64),
and expiry is read at one clock. Results describe the local scope, not the
whole graph, report whether they are complete, and nothing is stored or
written. REST, gRPC, MCP, query text, the graph CLI and the bindings do not
expose algorithms. See [Property graphs](GRAPH.md).
<!-- source: docs/GRAPH.md, site/api.html, src/property_graph/algorithms/tests.rs, tests/graph_algorithm_oracle.py -->

### Managed embedding providers and custom ranking (REST and gRPC)

Choose an embedding or ranking provider by alias, once an administrator has set
it up.
<!-- source: docs/GRPC.md, src/openapi.json -->

- **Off by default.** Without provider configuration, the built-in hashing
  embedder, the existing opt-in HTTP embedder, and exact and MMR reranking
  behave as before, and the server makes no provider call.
  <!-- source: docs/GRPC.md, src/openapi.json, docs/projects/roadmap/serving-contract.md -->
- **Administrator setup.** An administrator defines providers (endpoint,
  pinned model and version, dimensions, timeouts, concurrency, endpoint
  allowlist, and a secret taken from an environment variable or a private
  file) and binds them to collections in a startup file kept outside the
  database. Callers cannot supply endpoints, models, headers or secrets.
  [TO CONFIRM AT RC] The startup names `--providers-file` and
  `VLITE_PROVIDERS_FILE` await ratification. [VERIFY] The file format is
  documented only in the serving contract so far.
  <!-- source: docs/projects/roadmap/serving-contract.md, src/bin/vlite-server.rs, tests/server_cli.rs -->
- **REST.** `POST /v1/collections/{collection}/embed` takes `provider` as
  `hashing`, `http` or a bound alias and returns a typed
  `{vector, dimensions, provider}`. Search accepts
  `rerank: {mode: "custom", provider, candidates}` to rank candidates with a
  bound alias (`1 <= k <= candidates <= 1000`, default `max(k, 50)`). A hit can
  carry `rerank_score`, the negated provider cost, next to its unchanged metric
  `distance`, and the response can carry `ranking: {provider, order}`. The
  Python client has typed shapes for these fields.
  <!-- source: src/openapi.json, clients/python/README.md, docs/projects/roadmap/serving-contract.md -->
- **gRPC.** The `EmbedText` RPC returns the same shape, and `Search` accepts
  `RERANK_MODE_CUSTOM` with `rerank_provider`. [TO CONFIRM AT RC] gRPC reports
  `provider_snapshot_conflict` as `ABORTED`, a provisional mapping.
  <!-- source: docs/GRPC.md, proto/vlite/v1/vlite.proto, tests/grpc_provider_wire.rs, docs/projects/roadmap/serving-contract.md -->
- **Safety bounds.** The caller needs search authorization before an alias is
  resolved. One embedding input is at most 16,384 UTF-8 bytes. Provider
  requests are at most 1 MiB and responses 4 MiB, with 2 s connect and 10 s
  total deadlines and bounded per-provider and per-tenant admission. A provider
  call is retried at most once, and only when the administrator declared the
  alias replay-safe. A failed provider is never replaced by a fallback. The new
  error codes are `provider_not_configured`, `invalid_provider_input`,
  `provider_invalid_response`, `provider_unavailable`, `provider_timeout`,
  `provider_busy` and `provider_snapshot_conflict`.
  <!-- source: docs/GRPC.md, src/openapi.json, docs/projects/roadmap/serving-contract.md -->
- **Older peers.** The additions are optional, and existing messages keep their
  tags and meanings, so older clients keep working and ignore them. An older
  server has no `EmbedText` RPC (`UNIMPLEMENTED`) and rejects custom rerank
  mode.
  <!-- source: docs/GRPC.md, tests/grpc_provider_wire.rs -->

### Exact search kernels (results unchanged)

- **SIMD scoring.** Exact search scores several rows per pass with SIMD
  instructions where the CPU supports them: AVX2 on x86_64 (detected at run
  time), NEON on aarch64, and a portable kernel everywhere else, including
  WebAssembly. Each row keeps the same f64 operations in the same order as
  before, and no kernel uses fused multiply-add, so IDs, distances (bit for
  bit), order, ties and search statistics are identical. No API, setting,
  error, file format or wire field changes, and there is no switch to turn the
  kernels off. They also score filtered and parallel exact scans, hybrid and
  planned search, HNSW exact re-scoring, `ExactReranker`, named-vector exact
  search, disk ANN neighbor scoring, MMR pairwise scoring and the GPU
  searcher's CPU re-score. The NEON kernel was validated on native aarch64 CI.
  No performance claim is made here (see [Benchmarks](#benchmarks)).
  <!-- source: docs/projects/roadmap/p3-compact-vectors.md#p3d-progress-exact-simd-kernels-reviewed-unreleased, src/exact_kernels.rs, .github/workflows/ci.yml -->
- **[VERIFY] Filter block summaries.** Exact filtered search on a collection
  with declared metadata indexes keeps in-memory summaries of each block of
  1,024 records and skips blocks that provably hold no match. Hits, distances
  and order are unchanged, and only `SearchStats::records_scanned` can be
  lower. Nothing is written to disk. Repository docs still call this
  unreleased, so confirm that it belongs in these notes.
  <!-- source: docs/STORAGE-FEATURES.md#filter-block-summaries-unreleased, tests/filter_summaries.rs -->

### Approximate search: opt-in background rebuild (Rust)

`Database::set_background_hnsw_rebuild`, and the same method on
`CollectionMut`, lets a full mutation buffer start a background rebuild instead
of blocking the next query: searches keep using the current index plus an exact
scan of the changed records until the replacement is published. The option is
off by default, applies to one handle and is not persisted, so set it again
after reopening. No REST setting or request field was added. See
[Background HNSW rebuilds](REFERENCE.md#background-hnsw-rebuilds-rust).
<!-- source: docs/REFERENCE.md#background-hnsw-rebuilds-rust, site/api.html -->

## Fixed

- **Concurrent change-feed and replication reads.** Reads of the commit log
  that share one file cursor are now serialized, so a `GET /v1/changes`
  request, webhook delivery and a replication export that run at the same time
  can no longer interleave their seek and read steps. A concurrent test covers
  plain and encrypted files, and file, in-memory and directory backings.
  <!-- source: tests/graph_change_feed.rs, src/lib.rs -->
- **Publishing maintenance output where file locking is unavailable.**
  `backup`, `restore`, `compact`, `upgrade`, re-encryption, point-in-time
  restore and the `enable-graph` and `strip-graph` copies now take an
  exclusive lock on the new file before its name appears, so nothing opens a
  store while its publisher is still finishing. Where the filesystem reports
  that locking is unsupported, publication proceeds without the lock. A lock
  held by another process still fails with `Locked`, and any other lock error
  fails with an I/O error. In both cases nothing is published and the source
  is unchanged.
  <!-- source: src/maintenance.rs -->
- **Resident writes after a failed periodic flush.** A write that has already
  been appended to the log is always applied in memory, even when the periodic
  flusher fails right after the append. Memory therefore matches what reopening
  the file replays, the call returns success, and the handle refuses its next
  operation with `Poisoned` and appends nothing more. Put, put with expiry,
  delete, batch, import-chunk and graph batches that carry vector writes are
  covered by tests, and read-only handles still refuse before appending. This
  is the behavior 0.6.1 already had, so it needs no action on upgrade; the
  tests keep the reworked write path from changing it.
  <!-- source: src/lib.rs (tests post_append_put, post_append_put_with_expiry, post_append_delete, post_append_write_batch, post_append_import, post_append_graph_vectors, resident_writes_refuse_read_only_before_append and resident_apply_preserves_graph_rebuild_and_accounting_after_append; Database::apply has no error return at the v0.6.1 tag) -->

## Benchmarks

[TO FILL AT RC] Link this release's results in the DO-005 record ([baseline](projects/feature-agent-graph/baseline.md), [results](../benchmarks/RESULTS.md)) or state that none were run; this draft quotes no benchmark numbers.
<!-- source: docs/projects/feature-agent-graph/baseline.md, benchmarks/RESULTS.md, docs/projects/roadmap/next-major.md -->

## Upgrade

Back up and verify checksums first. Replace the binary or image with 0.7.0 and
restart. No file upgrade, setting or data migration is needed: existing files
open as before and are never converted in place. Follow the usual
single-writer rule: stop the old process (or scale the old revision to zero
replicas) before the new one opens the file.
<!-- source: docs/RELEASE-0.6.1.md, docs/RELEASE-0.6.0.md, docs/STORAGE-FEATURES.md -->

Every new capability is opt-in and changes nothing until you use it:

- the key-value store, when you call `kv_store` or the local key-value tools;
- the directory store, when you create or convert one;
- graph expiry and the TTL-aware feed, when you set a deadline or read that
  feed;
- graph algorithms, when you call `GraphRef::algorithm`;
- managed providers, when an administrator supplies a startup file;
- background index rebuilds, when you call `set_background_hnsw_rebuild`.
<!-- source: docs/STORAGE-FEATURES.md#key-value-store, docs/STORAGE-FEATURES.md#directory-store, docs/GRAPH.md#expiry-ttl, docs/GRPC.md, docs/REFERENCE.md#background-hnsw-rebuilds-rust, tests/server_cli.rs -->

Exact search uses the SIMD kernels automatically, with identical results.
Rust code that matches `GraphChange` exhaustively needs one new arm (see
[Compatibility](#compatibility)).
<!-- source: docs/projects/roadmap/p3-compact-vectors.md#p3d-progress-exact-simd-kernels-reviewed-unreleased, src/exact_kernels.rs, src/property_graph/change.rs -->

## Rollback

Restart with the 0.6.1 binary or image. 0.7.0 writes nothing that 0.6.1 cannot
read unless you used one of these:
<!-- source: docs/RELEASE-0.6.1.md, docs/GRAPH.md#expiry-ttl, docs/STORAGE-FEATURES.md#directory-store -->

- **Graph expiry.** A graph file that has used the expiry operations is
  refused by older builds as `Corrupt`. Go back to a copy made before the first
  deadline was set, or copy the file with `vlite-maintain strip-graph SOURCE
  DESTINATION --confirm-graph-data-loss`, which keeps the vectors and drops the
  graphs.
  <!-- source: docs/GRAPH.md#expiry-ttl, docs/GRAPH.md#compatibility-and-rollback -->
- **Directory store checkpoints.** Older releases cannot open a directory that
  has been checkpointed, or a backup packed from one. Keep the single file you
  converted from. [VERIFY] To leave a checkpointed directory, pack it with
  `backup` and compact the packed file (compaction drops the checkpoint flag
  and renumbers from one), then open the result with the older binary before
  you rely on it. No test covers this path.
  <!-- source: docs/STORAGE-FEATURES.md#directory-store, docs/projects/roadmap/p2-directory-store.md -->

If you configured managed providers, also remove `--providers-file` or
`VLITE_PROVIDERS_FILE` from the startup arguments and environment, because
0.6.1 has no providers file.
<!-- source: src/bin/vlite-server.rs -->

## Compatibility

- **Defaults are unchanged.** A file created with default options is written as
  in 0.6.1 (format 5 with graphs). File handles and `vlite-server` keep
  periodic durability with a 60 s interval. The default Cargo features and the
  minimum Rust version (1.89) are unchanged.
  <!-- source: tests/graph_regression.rs, docs/RELEASE-0.6.0.md, docs/STORAGE-FEATURES.md#periodic-lazy-durability, Cargo.toml -->
- **Files.** The new stores and features add nothing to a file unless you use
  them, and key-value entries are ordinary records. A graph file that has used
  expiry, and a checkpointed directory store or a backup packed from one,
  cannot be opened by older releases (see [Rollback](#rollback)). A file that
  never uses expiry is written exactly as before.
  <!-- source: docs/GRAPH.md#expiry-ttl, docs/STORAGE-FEATURES.md#directory-store, tests/graph_ttl.rs, src/kv.rs -->
- **Wire.** REST and gRPC changes are additive: optional fields, one new RPC
  and one new rerank mode, with existing tags and meanings unchanged. The
  `tools/list` of `vlite-mcp` lists four more tools: `vlite_kv_get` and
  `vlite_kv_list` in read-only mode, plus `vlite_kv_put` and `vlite_kv_delete`
  with `--writable`.
  <!-- source: src/openapi.json, proto/vlite/v1/vlite.proto, tests/grpc_provider_wire.rs, tests/graph_surface_parity.rs, tests/fixtures/kv/mcp-tool-additions.json -->
- **Errors.** `cursor_before_history` (Rust `Error::CursorBeforeHistory`, REST
  409, a C ABI error string) applies to history below a directory store's
  checkpoint floor. The Rust `Error` enum, which is `#[non_exhaustive]`, also
  gains `InvalidKvKey`, `KvSlotsExhausted` and `NotKvCollection`.
  <!-- source: src/lib.rs, src/server.rs, bindings/c/src/lib.rs, docs/API.md -->
- **Rust source.** `GraphChange` (`vlite::property_graph::GraphChange`, also
  `vlite::changes::GraphChange`) gains a `Temporal(TemporalChange)` variant and
  is not `#[non_exhaustive]`, so a `match` that lists every variant stops
  compiling: add an arm or a wildcard. Production builds never produce the
  variant. `NodeView` and `EdgeView`, both `#[non_exhaustive]`, gain
  `expires_at_ms`.
  <!-- source: src/property_graph/change.rs, src/property_graph/read.rs, src/server/grpc.rs -->
- **Build.** The library now depends on `log` and, outside WebAssembly, on
  `memmap2`.
  <!-- source: Cargo.toml -->

## Not included in this release

These are private, test-only, gated or design-only. They are not available, and
you should not rely on them:
<!-- source: docs/projects/roadmap/next-major.md -->

- **Compact and float16 vector storage.** Float32 stays the only storage
  precision, and release builds refuse stores that carry the precision flag.
  <!-- source: docs/projects/roadmap/next-major.md, src/compact/mod.rs -->
- **Compact graph storage.** It is test-only, and production graph storage is
  unchanged.
  <!-- source: docs/GRAPH.md, src/compact/mod.rs -->
- **Agent memory** (remember, recall, consolidate, forget). The code is
  crate-private and inactive.
  <!-- source: docs/projects/roadmap/next-major.md, docs/projects/roadmap/gap1-agent-memory.md, src/lib.rs -->
- **Temporal graph** (validity intervals and as-of reads). Production builds
  refuse temporal operations and temporal files with `UnsupportedFeature(64)`.
  The public `GraphChange::Temporal`, `GraphOp::Temporal` and
  `Database::temporal_changes_after` items exist, but never carry temporal
  data in a production build.
  <!-- source: docs/GRAPH.md, site/api.html, src/property_graph/change.rs -->
- **Cypher-style query subset.** The parser, planner and read executor are
  crate-private, with no public surface.
  <!-- source: docs/projects/roadmap/next-major.md, docs/projects/roadmap/gap6-cypher-subset.md -->
- **Document-to-graph ingestion.** Only crate-private preparation code and test
  references exist.
  <!-- source: docs/projects/roadmap/next-major.md, docs/projects/roadmap/gap7-graphrag-ingestion.md -->
- **Leases and a fencing-token ledger.** This is a design only and is not
  implemented.
  <!-- source: docs/projects/roadmap/next-major.md, docs/projects/roadmap/gap2-lease-ledger.md -->
- **Filter-aware approximate search.** It is private and gated.
  <!-- source: docs/projects/roadmap/next-major.md, docs/projects/roadmap/p5-filters-and-serving.md -->
- **Access beyond Rust to graph expiry, the TTL-aware feed and graph
  algorithms.** REST, gRPC, MCP, the graph CLI and the bindings do not expose
  them.
  <!-- source: docs/GRAPH.md, docs/projects/roadmap/gap3-ttl-feed.md -->
- **Creating a directory store outside Rust.** The shell, the import tool, the
  server and the bindings cannot create one.
  <!-- source: docs/STORAGE-FEATURES.md#directory-store, src/bin/vlite-server.rs -->
- **Scale-out** (network replication, object storage, branching). The work is
  backlogged.
  <!-- source: docs/projects/roadmap/next-major.md, docs/projects/roadmap/scale-out.md -->
