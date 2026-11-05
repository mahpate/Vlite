# VLite 0.7.0 upgrade and rollback

VLite 0.7.0 adds a key-value store, agent memory, an opt-in directory store, graph expiry, bounded graph algorithms and optional managed embedding and ranking providers.

<!-- source: docs/STORAGE-FEATURES.md#key-value-store, docs/STORAGE-FEATURES.md#directory-store, docs/GRAPH.md#expiry-ttl, docs/GRPC.md, docs/API.md#agent-memory-rest, site/api.html -->

It follows [0.6.1](RELEASE-0.6.1.md). The additions are embedded Rust APIs, local shell and MCP tools, and REST and gRPC key-value and provider operations, plus agent memory over REST with clients, a command line and MCP tools. Existing files open as before and new files are written as before. New stores and application-facing operations need explicit use or setup. Exact SIMD scoring, filter-summary pruning and the fixes below apply automatically, and search hits and distances stay unchanged. Rust code that matches `GraphChange` exhaustively needs a new arm, and code that uses the generated `grpc` types has further source changes (see [Compatibility](#compatibility)). Measured speed results are in [Benchmarks](#benchmarks); this release makes no size claims. <!-- source: docs/STORAGE-FEATURES.md, docs/GRAPH.md, docs/GRPC.md, docs/API.md, src/exact_kernels.rs, tests/filter_summaries.rs, tests/graph_regression.rs, src/property_graph/change.rs, proto/vlite/v1/vlite.proto, benchmarks/RESULTS.md -->

> **Draft for release-candidate review.** The version number is provisional. Resolve or delete every `[TO CONFIRM AT RC]`, `[TO FILL AT RC]`, `[TO FILL AT FREEZE]` and `[VERIFY]` marker before publishing. A `<!-- source: ... -->` comment names the repository document or test that backs the statement before it.

## What is new

### Key-value store

Keep values under namespaced string keys, with no vector. <!-- source: docs/STORAGE-FEATURES.md#key-value-store -->

- **Embedded Rust API.** `Database::kv_store` and `Database::kv_view` (with
  `KvStore`, `KvView`, `KvEntry` and `KvPut` in `vlite::kv`) give you
  namespaced string keys with byte values, ordered prefix listing, optional
  expiry and compare-and-set on versions. An entry is an ordinary record in
  a one-dimension collection, so the file format does not change.
  `kv_store` creates a missing one-dimension collection; `kv_view` and every
  other surface need an existing one. The store needs a format 4 or newer
  file, and expiry needs format 5. Namespaces are 1 to 255 bytes, keys 1 to
  1,024 bytes and values up to 1,047,290 bytes, and one list call returns at
  most 10,000 entries. See
  [Key-value store](STORAGE-FEATURES.md#key-value-store).
  <!-- source: docs/STORAGE-FEATURES.md#key-value-store, src/kv.rs, tests/kv.rs -->
- **Shell and MCP.** `vlite-shell --db PATH` accepts `kv-get`, `kv-put`,
  `kv-delete` and `kv-list`. `vlite-mcp` adds `vlite_kv_get` and
  `vlite_kv_list`, and with `--writable` also `vlite_kv_put` and
  `vlite_kv_delete`. They work on a local file only (not through `--server`),
  need an existing one-dimension collection and never create one, and have no
  lease commands. A list page stops at 10,000 entries or 16 MiB of canonical
  JSON, as over REST. Use dedicated collections for key-value data, because
  vector, import and graph operations on the same collection can overwrite its
  entries. See [shell commands](TOOLS.md#local-key-value-shell-commands) and
  [MCP tools](MCP.md#local-key-value-tools).
  <!-- source: docs/TOOLS.md#local-key-value-shell-commands, docs/MCP.md#local-key-value-tools, tests/kv_tools.rs (list_page_exact_byte_cap_mcp_and_shell, list_limit_10000_peeks_for_continuation), tests/fixtures/kv/mcp-tool-additions.json; commit 689b04d8 -->
- **REST.** `vlite-server` serves four operations: `GET`, `PUT` and `DELETE`
  on `/v1/collections/{collection}/kv/entry`, and `GET` on
  `/v1/collections/{collection}/kv/entries` for prefix listing. They need an
  explicit, existing one-dimension collection and never create one. Get and
  list need read scope; put and delete need write scope. Values are padded
  standard base64, and versions and deadlines are uint64 decimal strings. Put
  and delete take an expected version (`any`, `absent` or a revision), and a
  put without `expires_at_ms` clears the entry's expiry. A list page defaults
  to 100 entries and stops at 10,000 entries or 16 MiB of canonical JSON, with
  `next_after` to continue and no snapshot across pages. Requests are strict:
  put takes no query parameters, unknown or duplicate query parameters are
  refused (400 `invalid_query`) as are unknown or duplicate JSON fields (422
  `invalid_json`), and query strings must be valid UTF-8 percent-encoding with
  form decoding, so `+` is a space and a literal plus is sent as `%2B`. The new
  error codes are `kv_not_found` (404), `not_kv_collection` (409) and
  `kv_slots_exhausted` (409). `GET /v1/capabilities` reports `features.kv`
  (true on format 5 files) and lists both routes. Put and delete are audited
  with the principal, collection, request id and outcome, and a put also with
  its expiry deadline, never with a key, namespace or value; a failed request
  records the collection as `(invalid)`. See
  [Key-value entries](API.md#key-value-entries) for compare-and-set, expiry,
  quotas and errors.
  <!-- source: tests/server_kv.rs (kv_contract_wire_goldens against tests/fixtures/kv/wire/ordinary.json, kv_put_expected_and_query_encoding_are_strict, kv_capabilities_advertise_supported_routes_and_format, kv_large_list_limits_preserve_order_and_continuation, kv_put_audit_records_safe_identity_and_expiry), src/server/admin.rs, src/server/kv.rs, src/openapi.json, docs/API.md#key-value-entries; commits 459a2ada and 3203986e -->
- **gRPC.** The new `vlite.v1.Kv` service has four unary RPCs: `GetKvEntry`,
  `PutKvEntry`, `DeleteKvEntry` and `ListKvEntries`. They carry raw bytes and
  uint64 versions and deadlines, and every address names its collection. The
  collection requirements, authorization, compare-and-set, expiry, quotas,
  audit and list page boundary (measured in canonical REST JSON) match REST.
  A collection name is checked against the 1 to 63 byte name grammar before any
  lookup or audit entry, so a rejected name is never written to the audit log.
  A value over 1,047,290 bytes is refused before it is converted, and a write
  whose commit outcome is unknown answers `UNKNOWN` with
  `commit_indeterminate`. The service has the same 8 MiB request and
  16 MiB response limits as `VLite` and is registered with the gRPC health
  service. A client needs a receive limit of at least 16 MiB + 1 KiB for
  `ListKvEntries` and a send limit above 1 MiB. Neither transport provides
  leases. See [Key-value entries](GRPC.md#key-value-entries).
  <!-- source: tests/server_grpc_kv.rs (grpc_kv_collection_validation_precedes_lookup_and_audit, grpc_kv_oversize_value_rejected_before_json_validation, grpc_kv_exact_page_byte_boundary, grpc_kv_health_and_transport_limits, grpc_kv_rest_parity_trace), src/server/grpc/kv.rs (grpc_kv_commit_indeterminate_is_unknown_for_put_and_delete), docs/GRPC.md#key-value-entries, proto/vlite/v1/vlite.proto; commit 80a6baf9 -->
- **HTTP clients.** The Python, JavaScript and .NET clients provide get, put
  (with compare-and-set), delete and prefix-list methods (`kv_get`, `kv_put`,
  `kv_delete` and `kv_list`; `kvGet` and so on; `KvGetAsync` and so on) for the
  REST routes above. Values are bytes, and versions and deadlines are lossless
  64-bit integers. Against a server without the routes, such as 0.6.1, the
  methods raise the client's unsupported error and do not fall back. These are
  HTTP clients, not the embedded bindings, and the Go client has no key-value
  methods.
  <!-- source: clients/README.md, clients/python/README.md, clients/javascript/README.md, clients/python/vlite_client/__init__.py, clients/javascript/index.js, clients/dotnet/VLite.Client/Client.cs -->
- **Durability and fencing.** Ordinary key-value writes follow the database's
  durability setting: `full` syncs before it acknowledges, `periodic` (the
  default) can lose the last flush interval on an OS crash or power loss, and
  `none` keeps nothing on disk. A successful write is acknowledged after
  commit, and no call forces a sync. A version is a commit sequence, not a
  durable, never-reused lease ledger: after such a loss a later write can
  receive a revision that was already acknowledged, and compaction, upgrade and
  restore renumber revisions. Use versions for compare-and-set on one open
  store, and read an entry again after any of those events. A dropped
  connection or a `commit_indeterminate` error does not prove that a write
  failed, so do not retry blindly. There are no lease operations.
  <!-- source: docs/API.md#key-value-entries, src/openapi.json, docs/STORAGE-FEATURES.md#key-value-store, docs/SECURITY.md, src/kv.rs; commit 3203986e -->

### Agent memory

Agent memory stores remembered entities and episodes in a graph and recalls them with bounded, ranked retrieval. It is off by default: the server needs a private bindings file, and the clients, the command line and the MCP tools below call it.
<!-- source: docs/projects/roadmap/next-major.md#decided-2026-11-04, docs/projects/roadmap/gap1-agent-memory.md, docs/API.md#agent-memory-rest -->

- **Clients.** The JavaScript, Python and .NET clients have a binding-local
  facade, `client.memory(binding)` (`client.Memory(binding)` in .NET), with
  nine operations: remember, recall, find facts, correct facts, consolidate,
  forget, get receipt, get erasure and resume erasure. Each sends one POST under
  `/v1/memory/{binding}/` and returns the full response; none retries, pages,
  splits or embeds for you. They need a server with a provisioned memory
  binding.
  <!-- source: clients/README.md, clients/javascript/README.md, clients/python/README.md, clients/javascript/index.js, clients/python/vlite_client/__init__.py, clients/dotnet/VLite.Client/MemoryClient.cs; commits 52e17469, f32e9448 and fb23d09b -->
- **Command line.** `vlite-memory-cli`, built with the `mcp-proxy` feature and
  included in the release archives (not in the Docker images), sends one
  request file to a binding with one of nine verbs: `remember`, `recall`,
  `facts`, `correct-facts`, `consolidate`, `forget`, `receipt`, `erasure` and
  `resume-erasure`. It prints one JSON line on success and one coded JSON error
  line on stderr, and requires HTTPS except for loopback hosts. See
  [Remote agent memory](TOOLS.md#remote-agent-memory).
  <!-- source: docs/TOOLS.md#remote-agent-memory, tests/memory_cli.rs, tests/fixtures/memory_cli, .github/workflows/release.yml, Dockerfile; commits 4ba4e2bb and 02853281 -->
- **MCP tools.** `vlite-mcp --server URL`, built with `mcp-proxy`, lists nine
  `vlite_memory_*` tools: remember, recall, facts, correct facts, consolidate,
  forget, receipt, erasure and resume erasure. Recall, facts, receipt and
  erasure are read-only tools; the other five need `--writable`. Each takes
  flat arguments plus a `binding`, forwards them unchanged, and the tools never
  appear with `--db`. Memory calls ignore ambient proxies, allow plain HTTP only
  for loopback hosts, and refuse a response that contains the bearer token or
  repeats a key. See
  [Server-backed agent memory](MCP.md#server-backed-agent-memory).
  <!-- source: docs/MCP.md#server-backed-agent-memory, tests/memory_mcp.rs, tests/fixtures/memory_tools; commits 2bf3ff0a and 4b9af2dc -->
- **Server.** Start `vlite-server` with `--memory-bindings-file PATH` (or
  `VLITE_MEMORY_BINDINGS_FILE`) to serve nine `POST` routes under
  `/v1/memory/{binding}/`. A binding names one graph and one evidence
  collection, the scopes (agent, user, session) each credential may use and a
  receipt window. `remember`, `recall`, `consolidate`, `forget` and
  `receipts/lookup` are served. `facts/query`, `facts/correct`,
  `erasures/get` and `erasures/resume` answer 409
  `memory_capability_unavailable` with `details.feature`. A mutation needs a
  body `idempotency_key`; a matching retry returns the original result with
  `replayed: true`. A request is limited to 8 MiB and a response has no server
  cap. Without the file there are no memory routes and the server behaves as in
  0.6.1. See [Agent memory (REST)](API.md#agent-memory-rest).
  <!-- source: docs/API.md#agent-memory-rest, docs/projects/roadmap/gap1-adapter-mapping.md, src/bin/vlite-server.rs, src/server/memory.rs, src/server/memory/routes.rs, tests/server_memory.rs -->
- **Reserved resources.** A binding's graph and evidence collection are
  reserved: the raw collection, record, search, graph, feed, export and
  attachment routes treat them as if they did not exist, unless the credential
  holds `raw_access` on that binding and the ordinary permissions. A create
  under a reserved name answers as for an occupied name. While any binding is
  configured, the gRPC listener, automatic index maintenance, imports,
  replication export and a few aggregate admin routes are disabled. See
  [Memory-bound resources](SECURITY.md#memory-bound-resources), including its
  known limits.
  <!-- source: docs/SECURITY.md#memory-bound-resources, tests/server_memory_guards.rs, src/server/reserved.rs -->
- **First-slice limits.** Facts, fact correction, erasure jobs and `forget` by
  series, entity, session or scope are not served yet. `consolidate` is C1 only
  (retain, commit only). `forget` deletes by ID and logically; it writes no
  deletion manifest, so its `erasure_notice` omits `manifest_id`, and it is not
  secure erasure. A receipt lookup of a `remember` receipt answers 409
  `memory_capability_unavailable` with `details.feature`
  `remember_receipt_lookup`; the original `remember` response carries the
  complete receipt. A 412 `precondition_failed` carries the code and message
  only. Recall is exact and bounded: `exact` and `mmr` selection work on a
  vector alone, and `mode: approximate` answers 422
  `memory_bounded_plan_unsupported`. Items cannot yet carry evidence
  references, metadata, assertions or a `confidence`. A binding's identity (its
  receipt window, generation and names) is read from the file at each start and
  is not stored in the database; do not change it once memory has been written,
  because after a restart with a changed receipt window a retry that is still
  inside its original window answers 409 `idempotency_conflict`. Every refusal
  and its `details.feature` is listed in
  [Agent memory (REST)](API.md#first-slice-limits). The clients surface a
  refusal unchanged, including `details.feature`; the command line prints its
  code and message.
  <!-- source: docs/projects/roadmap/gap1-adapter-mapping.md#1-first-rest-slice-inventory, tests/server_memory.rs, tests/fixtures/agent_memory/wire, clients/javascript/README.md, clients/python/README.md, docs/TOOLS.md -->

### Directory store (opt-in)

Keep a database as a directory, so that reopening loads a checkpoint image and
replays only the active log instead of the whole history.
<!-- source: docs/STORAGE-FEATURES.md#directory-store, docs/projects/roadmap/p2-directory-store.md -->

- **Create or convert.** `Database::create_directory(path, dimensions, options)`
  creates a format 5 database as a directory of write-ahead logs and an
  optional checkpoint image, listed by an atomic manifest, and
  `Database::is_directory` tells a handle's kind.
  `maintenance::convert_to_directory` (and `convert_to_directory_with_key` for
  an encrypted file) turns a closed format 5 file into one without changing the
  source. It writes a checkpoint image at once and keeps the lineage and every
  revision, so only change-feed cursors at or after the source's last commit
  stay valid. `maintenance::backup` packs a directory back into one file.
  Creating or converting a directory store is a Rust API. `Database::open`,
  `open_read_only`, the server and the maintenance tools accept the directory
  path unchanged. Commit timestamps and formats 3 and 4 are not supported.
  See [Directory store](STORAGE-FEATURES.md#directory-store).
  <!-- source: docs/STORAGE-FEATURES.md#directory-store, docs/projects/roadmap/p2-directory-store.md, tests/directory_store.rs, src/maintenance.rs -->
- **Checkpoints.** When the active log reaches the default 64 MiB
  (`DEFAULT_CHECKPOINT_BYTES`), a commit starts a background checkpoint that
  folds the logs into one image. `Database::set_checkpoint_threshold` changes
  or turns off that threshold (a single file ignores it), and
  `Database::checkpoint` runs one, waits and returns the floor sequence, or
  `None` when there was nothing to fold. The lineage, every record's revision
  (so compare-and-set versions keep matching), graphs and encryption are kept.
  <!-- source: docs/STORAGE-FEATURES.md#directory-store, src/directory.rs, src/lib.rs, tests/directory_store.rs, tests/directory_recovery_matrix.rs -->
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
  visibility only and is not time travel). Each graph write first reclaims
  what is due in the same atomic frame, using computed reclaim entries that do
  not count toward the 4,096-operation limit; a deadline that a batch sets at
  or before the write's clock is reclaimed for the rest of that batch.
  Compaction, `upgrade`, directory checkpoints and `convert_to_directory` drop
  what is due, and deadlines survive reopen and backup. Records attached to a
  node still cannot expire.
  See [Expiry](GRAPH.md#expiry-ttl).
  <!-- source: docs/GRAPH.md#expiry-ttl, docs/STORAGE-FEATURES.md#graph-enabled-files, tests/graph_ttl.rs -->
- **Change feed.** `Database::graph_changes_after`,
  `changes::read_log_graph_changes` and `changes::GraphFeedCursor` read an
  opt-in graph feed that reports deadline changes and expiry removals. Each
  operation carries a `GraphTtl` annotation: a deadline set or cleared, an
  explicit removal, or an expiry removal of the entity's own deadline or of its
  node. The default feed (`changes_after`) and its bytes are unchanged. A
  cursor is bound to one event set (`Empty` or `Ttl`), and a page can end
  inside a large expiry removal and resume there, so keep the whole cursor
  token (`to_token` and `from_token`): `position()` alone drops the place
  inside a commit. A checkpoint or compaction image omits entities that are
  already due without logging a delete, so the feed reports no removal for
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
memory and iterations are capped (defaults 100,000 vertex operations, 64 MiB,
20 iterations and 16 sampled sources; ceilings 1,000,000, 256 MiB, 100 and
64), edge reads, the deadline and cancellation stay bound by the existing
graph limits, and expiry is read at one clock. Results describe the local
scope, not the whole graph, echo the view sequence, read clock and normalized
request, report separately whether the scope and the computation completed,
and nothing is stored or written. REST, gRPC, MCP, query text, the graph CLI
and the bindings do not expose algorithms. See [Property graphs](GRAPH.md).
<!-- source: docs/GRAPH.md, site/api.html, src/property_graph/algorithms/tests.rs, tests/graph_algorithm_oracle.py -->

### Managed embedding providers and custom ranking (REST and gRPC)

Choose an embedding or ranking provider by alias, once an administrator has set
it up.
<!-- source: docs/GRPC.md, src/openapi.json -->

- **Off by default.** Without a providers file, the built-in hashing
  embedder, the existing opt-in HTTP embedder, and exact and MMR reranking
  behave as before, and the server makes no provider call. `embed` then answers
  as 0.6.1 did: the `hashing` provider returns the same response bytes, and an
  unknown, empty or malformed `provider` is 422 `invalid_input`, not 409
  `provider_not_configured`. A loaded providers file, even an empty one,
  switches on the alias behavior described below.
  <!-- source: tests/server_embed_legacy.rs (embed_matches_released_response_bytes against tests/fixtures/embed_legacy, raw bodies captured from the released 0.6.1 server; its README.md explains why the http provider has no success golden), src/server/providers/tests/serving.rs (rest_empty_registry_means_zero_egress), src/server/features.rs (embed_text, embed_builtin_in); commits c317aed1 and 48fa1307 -->
- **Administrator setup.** An administrator defines providers (endpoint,
  pinned model and version, dimensions, connect and total timeouts,
  concurrency, an endpoint allowlist, and an optional secret) and binds them
  to collections in a startup file kept outside the database, with at most 64
  aliases and 4,096 bindings in a file of at most 1 MiB. A secret is referenced
  by an environment variable, or on Unix by an absolute private file; Windows
  rejects file secret sources, so use an environment reference there. Callers
  cannot supply endpoints, models, headers or secrets. The file schema is in
  the [serving contract](projects/roadmap/serving-contract.md#gap-8-delivery-1-private-startup-registry-candidate).
  The startup file is named by `--providers-file` or `VLITE_PROVIDERS_FILE`.
  <!-- source: docs/projects/roadmap/serving-contract.md, src/server/providers.rs, src/bin/vlite-server.rs, tests/server_cli.rs -->
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
  `RERANK_MODE_CUSTOM` with `rerank_provider`. A shorter client `grpc-timeout`
  also bounds managed work, but a client that resets the stream does not cancel
  a provider call that is already running; the call stays bound by its total
  deadline and admission slot. gRPC reports `provider_snapshot_conflict` as
  `ABORTED`.
  <!-- source: docs/GRPC.md, proto/vlite/v1/vlite.proto, src/server/grpc.rs, src/server/providers/tests/grpc_serving.rs, tests/grpc_provider_wire.rs, docs/projects/roadmap/serving-contract.md -->
- **Safety bounds.** The caller needs search authorization before an alias is
  resolved. One embedding input is at most 16,384 UTF-8 bytes. Provider
  requests are at most 1 MiB and responses 4 MiB, with administrator-configured
  connect and total deadlines (at most 2 s and 10 s respectively) and bounded
  per-provider and per-tenant admission. A provider call is retried at most
  once, and only when the administrator declared the alias replay-safe. A
  failed provider is never replaced by a fallback. Only the top-level JSON
  payload fields that a binding names are sent to a provider, and metadata is
  never sent implicitly. The new error codes are `provider_not_configured`,
  `invalid_provider_input`, `provider_invalid_response`,
  `provider_unavailable`, `provider_timeout`, `provider_busy` and
  `provider_snapshot_conflict`.
  <!-- source: docs/GRPC.md, src/openapi.json, src/server/providers.rs, docs/projects/roadmap/serving-contract.md -->
- **Older peers.** The additions are optional, and existing messages keep their
  tags and meanings, so older clients keep working and ignore them. An older
  server has no `EmbedText` RPC or `Kv` service (`UNIMPLEMENTED`) and rejects
  custom rerank mode.
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
  Measured speed is in [Benchmarks](#benchmarks).
  <!-- source: docs/projects/roadmap/p3-compact-vectors.md#p3d-progress-exact-simd-kernels-reviewed-unreleased, src/exact_kernels.rs, .github/workflows/ci.yml -->
- **Filter block summaries.** Exact filtered search on a collection
  with declared metadata indexes keeps in-memory summaries of each block of
  1,024 records and skips blocks that provably hold no match. This applies
  automatically to eligible exact filtered scans, including the exact scan
  inside `search_auto`. Hits, distances and order are unchanged, and only
  `SearchStats::records_scanned` can be lower. Nothing is written to disk.
  <!-- source: docs/STORAGE-FEATURES.md#filter-block-summaries, tests/filter_summaries.rs, src/metadata_index.rs -->

### Approximate search: opt-in background rebuild (Rust)

`Database::set_background_hnsw_rebuild`, and the same method on
`CollectionMut`, lets a full mutation buffer start a background rebuild instead
of blocking the next query: searches keep using the current index plus an exact
scan of the changed records until the replacement is published. Before the
first matching index exists, ordinary approximate queries scan exactly, while
the ready-only APIs still return their missing-index error and never start
work. A slow rebuild can let the set of changed records grow past the usual
256-ID and 8 MiB limits, which costs memory and exact-scan time instead of
discarding the ready index, and errors are returned, never hidden by an inline
build. The option is off by default, applies to one handle and is not
persisted, so set it again after reopening. No REST setting or request field
was added. See
[Background HNSW rebuilds](REFERENCE.md#background-hnsw-rebuilds-rust).
<!-- source: docs/REFERENCE.md#background-hnsw-rebuilds-rust, site/api.html, src/rebuild.rs -->

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

The DO-005 campaign measured release candidate `30901be1`, the integration
head with a green full CI run, against v0.6.0 and v0.6.1. It used the unchanged
`graph_baseline.py` reference profile (five runs per workload and revision) on
identical Azure Standard_D16s_v5 VMs, and 90 of 90 slots passed the unchanged
background-CPU gate; none was discarded or retried. At 100,000 × 384
dimensions, against v0.6.0:

- Query p50 is 42.3% lower on the exact workload, 23.4% lower on the filtered
  workload and 42.6% lower on the exact scan of the HNSW workload. The
  approximate HNSW query and the index build are within 2%.
- Recall and file bytes are identical.
- Ingest, reopen and peak memory are within about 3%.

The commits after `30901be1` add server adapters, tools, clients and
documentation, restore two 0.6.1 default behaviors (the number text of
`embed` responses, and `browse` observing flusher health once), and change
only the filter-summaries path of collections that declare a metadata index.
No other file on the paths the campaign measured changed: the single-file
store, exact and HNSW search, ingest and reopen. The additions are outside
those embedded workloads. The supplemental figures for indexed filters below
come from revision `58b18194`, which contains the filter-summary change. This
is not a new benchmark of the release revision.
<!-- source: git diff 30901be1..HEAD over src and crates (only src/compact/stats.rs, src/records.rs, the key-value and memory modules and src/openapi.json differ outside the server and tools), src/compact/stats.rs, src/records.rs; commits c317aed and 58b1819 -->

A supplemental run on the same VM configuration measured what that profile
does not exercise (435 measured slots; 432 passed the same unchanged gate and
3 failed it, so they are kept out of the medians). Medians of five runs, on the
release candidate:

- **Directory store.** Reopening 100,000 × 384 vectors takes 170 ms from a
  checkpointed directory against 238 ms from one file (−28.5%); at
  400,000 × 96 it takes 243 ms against 341 ms and at 1,000,000 × 32 it takes
  512 ms against 576 ms. Reopen still grows with the data, so the 50 ms goal is
  not met. An explicit checkpoint takes 2.7–4.0 s. A directory that checkpoints
  during ingest ingests at about half the single-file rate (117,000 against
  223,000 vectors/s); the log-only layout ingests and reopens like one file.
- **Exact search at 768 dimensions** (80,000 vectors): p50 4.9 ms against
  10.6 ms in 0.6.0 and 10.5 ms in 0.6.1, with the same ingest and reopen.
- **Declared metadata indexes** (100,000 × 384, equality filter on a boolean
  field): an `eq` or `eq-range` index
  cuts the p50 of a filter that matches 1% of the records by 87–89% and of one
  that matches 10% in whole 1,024-record blocks by 39–52%; with the 10% spread
  over all blocks the gain is 13–27%. A `range` index orders integers and
  floats only; on a boolean field it adds just the block summaries, which help
  when the matches fill whole blocks (24–44% at 10%, 85–88% at 1%) and
  otherwise cost the same as no index.
- **Queries during writes** (20,000 × 384, 16 writes and 4 deletes before each
  query): a lazy rebuild blocks one query for about 1.65 s, twice per run. The
  background rebuild keeps requests short, but queries sent back to back find a
  ready index only 40% of the time, and paced 250 ms apart the worst request is
  14.6 ms. The 10 ms goal is not met.

[Full tables, per-run CPU and method](../benchmarks/results/graph-reference-0.7.0-rc-vs-v0.6.0-azure.md) ·
[raw runs](../benchmarks/results/graph-reference-0.7.0-rc-vs-v0.6.0-azure.json) ·
[supplemental tables](../benchmarks/results/supplement-0.7.0-azure.md) ·
[supplemental raw slots](../benchmarks/results/supplement-0.7.0-azure.json) ·
[DO-005 baseline](projects/feature-agent-graph/baseline.md) ·
[published results](../benchmarks/RESULTS.md). These are measured values on one
host type, not performance guarantees.
<!-- source: benchmarks/results/graph-reference-0.7.0-rc-vs-v0.6.0-azure.md, benchmarks/results/graph-reference-0.7.0-rc-vs-v0.6.0-azure.json, benchmarks/results/supplement-0.7.0-azure.md, benchmarks/results/supplement-0.7.0-azure.json, docs/projects/feature-agent-graph/baseline.md, benchmarks/RESULTS.md, docs/projects/roadmap/next-major.md, src/compact/stats.rs -->

## Upgrade

Back up and verify checksums first. Replace the binary or image with 0.7.0 and
restart. No file upgrade, setting or data migration is needed: existing files
open as before and are never converted in place. Follow the usual
single-writer rule: stop the old process (or scale the old revision to zero
replicas) before the new one opens the file.
<!-- source: docs/RELEASE-0.6.1.md, docs/RELEASE-0.6.0.md, docs/STORAGE-FEATURES.md -->

These application-facing operations and storage modes need explicit use or
setup, and change nothing until you use them:

- the key-value store, through the Rust API, the local shell and MCP tools, the
  REST or gRPC operations, or the HTTP client methods;
- the directory store, when you create or convert one;
- graph expiry and the TTL-aware feed, when you set a deadline or read that
  feed;
- graph algorithms, when you call `GraphRef::algorithm`;
- managed providers, when an administrator supplies a startup file;
- agent memory, when an administrator supplies a bindings file
  (`--memory-bindings-file`; see
  [Start the server with memory](API.md#start-the-server-with-memory));
- background index rebuilds, when you call `set_background_hnsw_rebuild`.
<!-- source: docs/STORAGE-FEATURES.md#key-value-store, docs/STORAGE-FEATURES.md#directory-store, docs/GRAPH.md#expiry-ttl, docs/GRPC.md, docs/API.md#key-value-entries, docs/REFERENCE.md#background-hnsw-rebuilds-rust, tests/server_cli.rs -->

Exact SIMD scoring and filter-summary pruning apply automatically and leave
search hits and distances unchanged; summaries can lower
`SearchStats::records_scanned`. The fixes above apply automatically too. Rust
code that matches `GraphChange` exhaustively needs a new arm, and code that
uses the generated `grpc` types has further source changes (see
[Compatibility](#compatibility)).
<!-- source: src/exact_kernels.rs, src/metadata_index.rs, tests/filter_summaries.rs, src/property_graph/change.rs, proto/vlite/v1/vlite.proto -->

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
- **Directory store.** 0.6.1 cannot open a directory path at all, even one
  that has never been checkpointed, and a backup packed from a checkpointed
  directory carries a history flag that 0.6.1 refuses. Keep the single file you
  converted from; it excludes everything written to the directory since. To
  copy the current state out, use 0.7.0 `backup` and then `compact` into a
  separate file: compaction drops the history flag, starts a new lineage and
  renumbers revisions from one. Compaction of such an image is tested for plain
  and encrypted stores, but the whole chain from a checkpointed directory
  through `backup` and `compact` to the 0.6.1 binary is not tested end to end,
  so open the result with 0.6.1 before you rely on it. A copy that still holds
  graph deadlines is refused as `Corrupt`; use `strip-graph` on it as described
  above.
  <!-- source: docs/STORAGE-FEATURES.md#directory-store, docs/projects/roadmap/p2-directory-store.md, src/maintenance.rs, src/format.rs -->

If you configured managed providers, also remove `--providers-file` or
`VLITE_PROVIDERS_FILE` from the startup arguments and environment, because
0.6.1 has no providers file. Stop clients from calling `EmbedText`, custom
reranking and the key-value operations as well, because 0.6.1 does not serve
them. Key-value entries are ordinary records, so 0.6.1 still opens the file and
reads them as records of a one-dimension collection.
<!-- source: src/bin/vlite-server.rs, src/kv.rs, proto/vlite/v1/vlite.proto -->

If you configured agent memory, also remove `--memory-bindings-file` or
`VLITE_MEMORY_BINDINGS_FILE` and stop clients from calling the memory routes,
because 0.6.1 does not serve them. Memory data is an ordinary graph and
evidence collection, which 0.6.1 opens without any reservation: every
credential with ordinary access can then read them. Move or delete that data
first if that matters.
<!-- source: docs/SECURITY.md#memory-bound-resources, src/server/memory.rs -->

## Compatibility

- **Defaults are unchanged.** A file created with default options is written as
  in 0.6.1 (format 5 with graphs). File handles and `vlite-server` keep
  periodic durability with a 60 s interval. The default Cargo features and the
  minimum Rust version (1.89) are unchanged.
  <!-- source: tests/graph_regression.rs, docs/RELEASE-0.6.0.md, docs/STORAGE-FEATURES.md#periodic-lazy-durability, Cargo.toml -->
- **Files.** The new stores and features add nothing to a file unless you use
  them, and key-value entries are ordinary records. A graph file that has used
  expiry cannot be opened by older releases, and neither can a directory store
  or a backup packed from a checkpointed one (see [Rollback](#rollback)). A
  file that never uses expiry is written exactly as before. A file whose header
  carries the history or compact flag without the matching structure is now
  reported as `Corrupt`, where 0.6.1 reported `UnsupportedFeature`; no valid
  file written by 0.6.1 is affected.
  <!-- source: docs/GRAPH.md#expiry-ttl, docs/STORAGE-FEATURES.md#directory-store, tests/graph_ttl.rs, src/kv.rs, src/format.rs -->
- **Invalid input is judged more strictly.** A file written by 0.6.1 is
  unaffected. A damaged one differs in two places. Reading the change feed or
  exporting the log now checks each graph frame in full, as inspection already
  did, so a frame with an altered count, reserved ID or body whose checksum was
  repaired is refused where 0.6.1's export (and sometimes its feed) accepted
  it, and the reason can differ. A cursor whose version byte is 2, passed to
  `GraphRef::find_nodes`, answers `InvalidQuery("malformed cursor")` where
  0.6.1 answered `InvalidQuery("unsupported cursor version")`; the error kind is
  the same.
  <!-- source: src/property_graph/frame.rs, src/logops.rs, src/changes.rs, src/format.rs, src/property_graph/engine/cursor.rs -->
- **Memory and logging.** A collection that declares a metadata index can log a
  `vlite::filter` warning, event `filter_summary_budget`, when building its
  block summaries exceeds a fixed budget; that scan is then exact without them.
  Graph nodes keep their relationships and properties in sorted vectors
  instead of per-property trees, which changes the memory use and speed of large
  graphs; ordering, files and the wire are unchanged.
  <!-- source: src/compact/stats.rs, src/property_graph/engine/state.rs, src/property_graph/engine/model.rs -->
- **Wire.** REST and gRPC changes are additive: the four key-value REST
  operations (`GET /v1/capabilities` lists their routes and reports
  `features.kv`), five new RPCs (the four-RPC `vlite.v1.Kv` service and
  `VLite.EmbedText`), optional provider and ranking fields, and one new rerank
  mode, with existing tags and meanings unchanged. With a bindings file, nine
  `POST` routes under `/v1/memory/{binding}/` are also served; without one they
  do not exist. The OpenAPI document now
  also says that `rerank.lambda` is allowed only with `mode` `mmr`; the server
  already refused any other combination with 422 `invalid_input` in 0.6.1, so
  no request changes meaning. The `tools/list` of
  `vlite-mcp` lists four more tools: `vlite_kv_get` and `vlite_kv_list` in
  read-only mode, plus `vlite_kv_put` and `vlite_kv_delete` with `--writable`;
  with `--server URL` it also lists nine `vlite_memory_*` tools.
  <!-- source: src/openapi.json, proto/vlite/v1/vlite.proto, tests/grpc_provider_wire.rs, tests/server_grpc_kv.rs, tests/server_kv.rs (kv_capabilities_advertise_supported_routes_and_format), tests/graph_surface_parity.rs, tests/fixtures/kv/mcp-tool-additions.json, src/server.rs -->
- **Errors.** `cursor_before_history` (Rust `Error::CursorBeforeHistory`, REST
  409, a C ABI error string) applies to history below a directory store's
  checkpoint floor. The Rust `Error` enum, which is `#[non_exhaustive]`, also
  gains `InvalidKvKey`, `KvSlotsExhausted` and `NotKvCollection`. Over REST and
  gRPC, `kv_not_found`, `not_kv_collection`, `kv_slots_exhausted` and the seven
  provider codes are new. The agent-memory routes add their own codes, listed
  in [Agent memory (REST)](API.md#agent-memory-rest).
  <!-- source: src/lib.rs, src/server.rs, src/server/kv.rs, bindings/c/src/lib.rs, docs/API.md -->
- **Rust source.** `GraphChange` (`vlite::property_graph::GraphChange`, also
  `vlite::changes::GraphChange`) gains a `Temporal(TemporalChange)` variant and
  is not `#[non_exhaustive]`, so a `match` that lists every variant stops
  compiling: add an arm or a wildcard. Production builds never produce the
  variant. `NodeView` and `EdgeView`, both `#[non_exhaustive]`, gain
  `expires_at_ms`. With the `grpc` feature, the generated public Rust types
  change too: an exhaustive `match` on `RerankMode` needs the new `Custom` arm,
  `SearchRequest`, `Hit` and `SearchResponse` gain fields (so a struct literal
  needs `..Default::default()`), and code that implements the generated `VLite`
  server trait must add `embed_text`. These are source changes, separate from
  the additive wire changes above.
  <!-- source: src/property_graph/change.rs, src/property_graph/read.rs, src/server/grpc.rs, build.rs, proto/vlite/v1/vlite.proto -->
- **Build.** The library now depends on `log` 0.4 (one warning when a reader
  skips derived blocks of a compact image) and, outside WebAssembly, on
  `memmap2` 0.9.
  <!-- source: Cargo.toml, src/compact/mod.rs -->

## Not included in this release

These are private, test-only, gated, parked or design-only. They are not
available, and you should not rely on them. Size-driven work is parked for
0.7.0 as best effort: the release does not wait for it and makes no size
claims.
<!-- source: docs/projects/roadmap/next-major.md, docs/projects/performance/storage-goals.md -->

- **Compact and float16 vector storage (parked).** Float32 stays the only
  storage precision, and release builds refuse stores that carry the precision
  flag. Release builds validate the envelope of a compact checkpoint image,
  skipping authenticated derived blocks with one `log` warning, but no
  production writer creates one.
  <!-- source: docs/projects/roadmap/next-major.md, docs/projects/performance/storage-goals.md, src/compact/mod.rs -->
- **Compact graph storage (parked).** It is test-only, and production graph
  storage is unchanged.
  <!-- source: docs/GRAPH.md, docs/projects/performance/storage-goals.md, src/compact/mod.rs -->
- **Temporal graph** (validity intervals and as-of reads). Production builds
  refuse temporal operations and temporal files with `UnsupportedFeature(64)`.
  The public `GraphChange::Temporal`, `GraphOp::Temporal`, `TemporalOp`,
  `TemporalSegment`, `TemporalChange` and `Database::temporal_changes_after`
  items exist, but never carry temporal data in a production build.
  <!-- source: docs/GRAPH.md, site/api.html, src/property_graph/change.rs -->
- **Cypher-style query subset.** The parser, planner and read executor are
  crate-private, with no public surface.
  <!-- source: docs/projects/roadmap/next-major.md, docs/projects/roadmap/gap6-cypher-subset.md -->
- **Document-to-graph ingestion.** Only crate-private preparation code and test
  references exist.
  <!-- source: docs/projects/roadmap/next-major.md, docs/projects/roadmap/gap7-graphrag-ingestion.md -->
- **Leases and a fencing-token ledger.** This is a design only and is not
  implemented. Key-value versions are not such a ledger (see
  [Key-value store](#key-value-store)).
  <!-- source: docs/projects/roadmap/next-major.md, docs/projects/roadmap/gap2-lease-ledger.md -->
- **Filter-aware approximate search (parked).** It is private and gated.
  <!-- source: docs/projects/roadmap/next-major.md, docs/projects/performance/storage-goals.md, docs/projects/roadmap/p5-filters-and-serving.md -->
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
