# VLite 0.3.0 upgrade and rollback

This experimental release extends VLite's single-owner, single-file engine with
opt-in format V5, additional retrieval modes, transport security, offline tools
and client integrations. It includes all changes since `v0.2.0`. The live dataset
still resides in memory and the log is replayed on open; this is not a distributed
database or a production-hardening guarantee. See [capability contracts](CAPABILITIES.md),
[storage features](STORAGE-FEATURES.md), [REST](API.md) and [gRPC](GRPC.md).

## What's new since 0.2.0

### Transports, security and operations

- Native ARM CI runs the full Rust suite on Linux arm64, including a release
  gRPC smoke test. Release automation packages Linux arm64 command-line binaries
  alongside the existing native archives and multi-platform container images.
  This does not add Linux arm64 Python wheels or Node native packages.
- Streaming gRPC adds ascending-ID `ExportVectors` and resumable `StreamImport`,
  plus gRPC request, failure and stream-message metrics. REST and gRPC share the
  same owner, credentials, bounded workers and rate accounting. Extended RPCs
  expose V5 record/vector queries, disk ANN and change-feed watching.
- Native REST HTTPS and gRPC TLS use rustls. Optional gRPC mTLS requires client
  certificates **and** bearer authentication. Bootstrap-admin TLS reload validates
  every replacement identity/CA before publishing; existing sessions keep their
  old configuration. REST mTLS still requires a proxy.
- Optional `otel` builds export allowlisted request spans over OTLP/gRPC. No
  endpoint means no exporter; vectors, payloads, metadata values and tokens are
  not trace attributes. Streaming spans cover admission, not the entire stream.
- Persisted scoped credentials support shared REST/gRPC request-rate buckets and
  stored-vector quotas. Tenants provide namespace isolation and aggregate quotas
  within one database, not separate processes, encryption domains or files.
- The dashboard exposes V5 records/retrieval, change feeds, webhook management,
  disk ANN and PITR workflows. Timing-sensitive credential/log tests and
  maintenance crash/recovery coverage were hardened for slow CI runners.

See [server security](SECURITY.md) for flags, quota behavior and tenant limits.

### Format V5 and retrieval

- Opt-in V5 supports record TTL/absolute expiry, metadata-only updates, GeoPoint
  metadata, named dense vectors, sparse vectors and dense+sparse hybrid fusion.
  Model-version history and resumable re-embedding let applications stage a new
  model while reads continue using the old active version, then switch atomically.
- Grouped search, positive/negative-example recommendations, facets, numeric
  statistics and geographic filtering/ranking extend collection retrieval.
- A bounded, parsed **read-only** query language supports vector/text search,
  typed predicates, grouping, facets and ordering; it is not SQL or a scripting
  engine. Dense/sparse and lexical hybrid retrieval use weighted reciprocal-rank
  fusion over bounded candidate sets.
- Pluggable embedder/reranker traits, deterministic hashing embeddings, optional
  `http-embedder` OpenAI-compatible HTTP embeddings, exact rescoring and MMR add
  application-controlled retrieval pipelines. Hashing is not a semantic model.
  Provider calls, re-embedding scheduling and retry/backoff remain explicit.
- Encryption at rest seals frame bodies with ChaCha20-Poly1305; passphrases use
  Argon2id. Optional commit timestamps support timestamp-targeted PITR. Both are
  explicit creation options, not automatic changes to existing files.

See [vector extensions](VECTORS.md) and [capabilities](CAPABILITIES.md) for exact
bounds and format requirements; not every retrieval operation requires V5.

### Storage, recovery and acceleration

- A durable, lineage-bound change feed returns whole-commit batches. REST and
  gRPC expose polling, and gRPC adds `WatchChanges`. HMAC-SHA256 webhooks dispatch
  asynchronously with durable cursor progress and at-least-once delivery.
- PITR writes a newly validated destination from a retained log and optional
  exact-prefix backup. Sequence targets work on V4/V5; timestamp targets require
  timestamp-enabled logs. The active database is never overwritten.
- Read-replica log shipping provides `export_log` / `ReplicaFollower::apply`,
  file save/load and promotion. REST can export segments; delivery and follower
  management belong to the application. **There is no network follower service.**
- Disk ANN uses a rebuildable, lineage/sequence-bound HNSW sidecar with bounded
  query cache memory. Build memory still requires an in-memory HNSW build.
- Optional Cargo feature `gpu` adds default-collection squared-L2 exact GPU
  scoring through wgpu, CPU f64 candidate rescoring and CPU fallback on absence
  or failure of an adapter. It is not GPU ANN or a general GPU query backend.
- `bindings/wasm` supplies a wasm-bindgen in-memory package with byte-image
  import/export. Hosts own persistence and supply the clock. Build it from the
  tagged source; generated browser/Node WASM bundles are not release assets.

### Tools and ecosystem

- `vlite-shell`, `vlite-import`, `vlite-tune` and `vlite-quality` join the native
  archives. `tools` enables JSONL/CSV/pgvector/COPY/flat-FAISS tooling;
  `import-formats` additionally enables Parquet and Arrow IPC. The shell supports
  scripted/read-only use; tuning and quality tools compare HNSW configurations
  against exact baselines without persisting index settings.
- First-party Go and .NET REST clients join Python and JavaScript clients.
  Python includes LangChain and LlamaIndex adapters with optional framework
  dependencies. Native bindings and REST/gRPC surfaces expose the new engine
  operations where documented; feature parity is not implied.
- A Helm chart deploys a single-owner StatefulSet with persistent storage and
  secret-backed configuration. Go/.NET clients, Helm and WASM wrapper sources
  are included in native desktop/x64 archives. No NuGet, Go module registry,
  npm/PyPI or Helm repository publication is performed automatically.

See [tools](TOOLS.md), [clients](../clients/README.md),
[WASM build instructions](../bindings/wasm/README.md) and
[Helm operations](../deploy/helm/vlite/README.md).

## Compatibility

**V5 is opt-in. Older binaries refuse V5 files. There is no automatic migration.**
V1–V4 encodings and existing behavior remain unchanged. `Database::create` and
ordinary server creation still default to V3. Explicitly select
`FormatVersion::V5`, `CreateOptions` for encryption/timestamps, or server
`--create-format 5` when V5 functionality is required. Encrypted creation uses V5.
Do not assume that a binary version bump changes a database's format.

V4/V5 collections, conditional writes and lineage rules still apply. Copy-out
compaction/restore changes lineage: reacquire ETags, reissue lineage-bound grants,
resync change consumers and rebuild derived indexes when selecting a new copy.
Existing V1–V4 files do not acquire V5 mutations merely by being opened. Trial
new-format creation on separate files and re-ingest through supported APIs when
needed; there is no implicit V1–V4-to-V5 conversion.

## Known limitations

- All tenants and transports share one database owner and storage failure
  domain. A live file still needs RAM for its records; disk ANN only bounds
  graph-query cache memory. Tenants support 1,024 lifetime IDs; tenant-local
  collection names are limited to 28 characters.
- Named-vector state is a maximum 16 MiB collection snapshot rewritten per
  mutation. At most 32 dense names and 64 previous model versions are supported.
  Named graphs are not persisted; base record replacement clears extra vectors.
  Query execution can materialize matching candidates; hybrid facets/groups
  describe the bounded fusion set, not necessarily the full collection.
- Encryption does not conceal frame prefixes, IDs, operation types, file sizes
  or timing, and does not protect process memory/swap or server admin sidecars.
  Losing the key loses the data. ANN snapshots and disk ANN are refused for
  encrypted files because their vectors would be plaintext.
- PITR needs retained history and space for a full destination. Compaction drops
  commit timestamps/history and changes lineage; retain a pre-compaction backup.
  Replica apply replays the whole follower file and discards in-memory ANN.
- Change-feed polls scan the log from the beginning. Expiry is not an event.
  Compacted/restored cursors require resync. Webhooks share one cursor; an
  unavailable subscriber can stall progress, successful receivers may get
  duplicates, and there is no per-subscriber cursor or dead-letter queue.
- Disk ANN becomes stale after any commit, has no metadata/expiry filtering,
  supports L2/cosine only, and requires the old open sidecar to be dropped before
  replacement on Windows. GPU search does not support filters, cosine or named
  collections; f32 boundary near-ties can reorder despite CPU rescoring.
- WASM has no filesystem locks, path operations, background threads, sidecars
  or GPU backend. Durability is only as strong as host persistence of `toBytes()`.
- Offline tools require the other owner/server to stop. Import parsing holds
  the full input in memory; default atomic imports are limited to 4,095 rows and
  frame size. Chunked imports can leave earlier chunks committed after failure.
  Flat FAISS import is deliberately narrow; tuning samples are not production
  recall guarantees.
- gRPC export snapshots each page, not the whole stream. Failed streaming
  imports retain earlier committed chunks. Messages are limited to 8 MiB and
  responses to 16 MiB; there is no reflection API or replica transport/status RPC.
  Never blindly retry a commit-indeterminate write. Provider configuration and
  administrative coverage differ between REST, gRPC and embedded consumers.

The linked feature documents are authoritative for additional limits and error
semantics. Defaults remain conservative; enable only what the deployment needs.

## Safe upgrade sequence

1. Stop writes or schedule a maintenance window. Make a consistent serialized
   backup, verify it, and preserve an independent copy outside the database volume.
   Never copy a live, actively written file with a generic filesystem command.
2. Record API/dashboard image digests, binary versions, selected database path,
   admin-directory state and encryption-key custody. Retain compatible binaries
   and protected keys/configuration independently of the new deployment.
3. Deploy matching 0.3.0 API/dashboard components against the **unchanged** existing
   file. Check health, authenticated capabilities, format, counts and read-only
   exact queries. Existing V1–V4 workloads need no format conversion.
4. Trial V5 in a separate newly created file. Explicitly choose V5/encryption/
   timestamps as required and populate it through supported application APIs.
   Test expiry, vector history, retrieval, backup/restore, credentials and consumer
   resync using representative data before deciding to select it for production.
5. Verify the trial offline with `vlite-maintain verify` (use keyed maintenance
   for encryption). Test restore and keep the original recovery copy. Selecting
   a new file is an explicit operator action, never an automatic migration.

## Rollback

Stop new writes and preserve current database/admin state before rollback.
Restore recorded compatible API/dashboard images or binaries without removing
volumes (`compose down -v` is destructive). Do not disturb unrelated services.

If the file remains V1–V4, a binary supporting that format can reopen it. Never
point a pre-V5 binary at a V5 file. To roll back after selecting V5, restore a
validated pre-upgrade copy to a **separate path** and explicitly select it offline;
plan how to reconcile later writes. Binary rollback does not undo committed data.
Keep encryption keys and the original logs/backups needed by consumers and PITR.

## Release verification

Verify downloads against `SHA256SUMS` and check `release-manifest.json` for source
identity. Checksums detect corruption; they are not signatures. Use matching
API/dashboard versions or immutable image digests. Automation gates release
assets on native tests, installed SDK checks, gRPC smoke tests, multi-platform
image checks, HTTPS and browser verification. PyPI publication is handled by a
separate trusted-publisher workflow; the Python 0.3.0 packages are available as
[vlite-embedded](https://pypi.org/project/vlite-embedded/) and
[vlite-client](https://pypi.org/project/vlite-client/). See
[installation and platform limits](INSTALL.md) before choosing an asset.
