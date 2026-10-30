# Storage features

Opt-in engine features. Encryption, commit timestamps
and graphs need v5 files; change feeds and sequence restore work on v4 and v5.
Older binaries refuse v5 files with flags they do not understand
(`UnsupportedFeature`). No operation modifies its source file: each writes a
new, fully revalidated file or reads a closed or copied one.

## Shared-lock read-only snapshots

`Database::open_read_only(path)` and `open_read_only_encrypted(path, &key)`
take a shared OS lock, so several processes can read one file. A writer needs
exclusive ownership; conflicting opens return `Error::Locked` without waiting.
A handle sees the commits that existed when it opened. Mutations return
`Error::ReadOnly` without poisoning the handle, incomplete tails are rejected
rather than repaired, and `is_read_only()` reports the mode. In-memory ANN
builds still work.

Python: `native.open(path, read_only=True)`. Node:
`native.open(path, {readOnly: true})`. C ABI Open: `"read_only": true`. The C
ABI serializes calls per process, so parallel Python reads need separate
processes. The server keeps its exclusive lock and serves concurrent reads
itself; see [MCP read-only access](MCP.md).

## Encryption at rest

```rust
let key = EncryptionKey::from_bytes([7; 32]);            // or from_passphrase(..)
let db = Database::create_with_options(path, 384, CreateOptions {
    encryption: Some(key), ..Default::default() })?;
let db = Database::open_encrypted(path, &key)?;
```

- **Cipher.** ChaCha20-Poly1305 (pure Rust) with a per-file key derived from
  yours and a random salt. Passphrases use Argon2id (`KdfParams`). The header
  stores the salt, cost parameters and a non-secret key ID.
- **Scope.** Frame bodies (vectors, payloads, metadata) are sealed as
  `nonce || ciphertext || tag`. Frame prefixes stay plaintext for recovery and
  reveal operation type, collection ID, record ID, sequence and sizes; file
  size and timing also leak.
- **Errors.** `EncryptionKeyMismatch` (wrong key; reports the file's key ID),
  `EncryptionKeyRequired` (missing key) and `NotEncrypted` (key for a plain
  file). A damaged frame fails authentication, so garbage is never returned.
- **Maintenance.** `maintenance::reencrypt(src, dest, old_key, new_key)`
  rotates keys into a validated copy with a new salt; retire the old file and
  its backups yourself. `inspect_with_key`, `verify_with_key`,
  `backup_with_key`, `restore_with_key` and `compact_with_key` handle
  encrypted files, and the keyless versions return `EncryptionKeyRequired`.
  Backups stay encrypted.
- **Sidecars.** ANN snapshots and disk-ANN files are refused
  (`SidecarUnavailable`) because they would hold plaintext vectors; rebuild
  HNSW in memory instead.
- **Not covered.** Key storage and distribution, process memory, swap, core
  dumps and the server's own sidecar files. A lost key means lost data.

## Point-in-time restore

Create the database with `CreateOptions { commit_timestamps: true, .. }` to
record a wall-clock time per commit, then:

```rust
maintenance::restore_to(Some(base_backup), log_copy, dest,
    RestoreTarget::Sequence(42) /* or TimestampMs(ms) */, key)?;
```

- The log is a copy of the full committed log: a closed primary, a backup or a
  follower's file. An optional base must share its lineage and be an exact
  byte prefix of the log; both are verified.
- The output is a new, validated file with a **new lineage**. A target before
  the base or past the log gives `InvalidRestoreTarget`, and a timestamp
  target on a file without timestamps gives `TimestampsUnavailable`.
- `compact` drops timestamps and assigns a new lineage, so keep
  pre-compaction backups to restore earlier history by time. Sequence restore
  needs no timestamps.

## Change feed

```rust
let mut cursor = db.feed_cursor()?;                 // or FeedCursor::beginning(lineage)
let batch = db.changes_after(&cursor, 500)?;        // ChangeBatch { changes, next, has_more, head }
cursor = batch.next;
cursor.save(path)?;                                 // durable (tmp + fsync + rename)
```

- Events: put (with record and expiry), delete, metadata update, collection
  created or dropped, settings updated and vectors updated. Batch commits are
  never split, so a feed batch may exceed `max_events`. Expiry is not an
  event; consumers read `expires_at_ms` on the put.
- Cursors hold lineage, sequence and a CRC-checked token. Errors:
  `CursorCompacted` (file compacted or restored; resync), `CursorAhead` (past
  the head) and `InvalidCursor` (damaged token).
- Needs v4+ (`ChangeFeedRequiresUpgrade` otherwise). Each call scans the log
  from the start, O(log size), so use large batches and poll sparingly.
  Encrypted files are read with the database key.
- `changes::read_log_changes(path, key, cursor, max)` reads a closed or copied
  log without opening a database.

## Read replicas

```rust
let segment = primary.export_log(follower_sequence, 1 << 20)?;   // LogSegment
let mut follower = ReplicaFollower::create(path, &segment, key)?; // first segment
follower.apply(&next_segment)?;
let lag = follower.status()?;   // applied_sequence, primary_head, lag
follower.db().search(..)?;      // read-only access
```

- Segments are whole commits authenticated by CRCs (and AEAD tags when
  encrypted). Gaps, replays and foreign lineages fail with `ReplicaDiverged`
  and leave the follower unchanged.
- Writes on a follower return `ReadOnlyReplica`; `promote()` closes it and
  returns a writable `Database`.
- For file shipping, use `LogSegment::save`/`load`, `apply_file`, or
  `sync_from_log(path, max_bytes)` on a closed or copied primary. A live
  primary on Windows holds an exclusive lock, so ship `Database::export_log`
  segments instead.
- Each apply replays the whole follower file (O(database size)) and drops its
  in-memory ANN, so batch applies. A crashed apply's tail is discarded on
  reopen. There is no network transport.

## Disk-based ANN

```rust
let source = db.build_disk_ann("default", sidecar, HnswConfig::default(), DistanceMetric::SquaredL2)?;
let index = DiskAnnIndex::open(sidecar, DiskAnnOptions { cache_bytes: 64 << 20 })?;
let hits = db.search_disk_ann(&index, &query, 10, /*ef*/ 100)?;
```

- The index is a rebuildable sidecar holding the HNSW graph and f32 vectors in
  CRC-checked node records. It is bound to the database's lineage and
  sequence, so any later commit makes it `StaleIndex`; rebuild it. `verify()`
  checks every node, and a damaged sidecar is never trusted.
- **Query memory is bounded.** The header and upper layers stay resident;
  nodes are read with `pread` into an LRU cache capped at `cache_bytes`.
  `memory()` reports resident bytes, cache use, hits and disk reads.
- **Build memory is not.** A build needs as much memory as an in-memory HNSW
  build, so build larger-than-RAM collections on a bigger machine, or in
  pieces, and query them on a small one.
- Recall matches in-memory HNSW at the same `ef` because the same graph is
  walked. Tests assert recall@10 ≥ 0.9, within 0.05 of in-memory HNSW, and
  exact results at exhaustive `ef`. Records are indexed as stored (no
  metadata filters or expiry); squared L2 and cosine are supported.
- Not available for encrypted databases. On Windows, drop an open
  `DiskAnnIndex` before replacing its file.

## GPU exact search

Enable the cargo feature `gpu` (wgpu compute shaders).

```rust
let gpu = GpuExactSearcher::new();                          // None without an adapter
let hits = GpuExactSearcher::search_or_cpu(gpu.as_ref(), &db, &query, 10)?;
```

- Squared L2 over the default collection only; use the CPU path for filtered,
  cosine or other-collection search. The GPU scores in f32 and the best
  `4k + 32` candidates are re-scored in f64 on the CPU, so distances match
  `Database::search`. Near-ties within f32 error may reorder at the top-k
  boundary.
- With no adapter, or on GPU failure (`GpuUnavailable`), `search_or_cpu` uses
  the CPU. Software adapters (WARP, llvmpipe) work but are slow.

## WebAssembly

The engine builds for `wasm32-unknown-unknown` with `--no-default-features`.
`Database::create_in_memory`, `open_from_bytes` and `to_bytes` use a
byte-buffer backend without file locking. Encryption, change feeds and
timestamps work, and the host must persist the image for durability.
`set_wasm_clock_ms` supplies the time. Background HNSW rebuilds need threads
and return an error, and path-based APIs fail at runtime. `bindings/wasm` is a
wasm-bindgen package with a Node smoke test. The in-memory backend also works
natively.

## Graph-enabled files

Every build since v0.5.1 can store named property graphs in the same log as
vectors; there is no `graph` Cargo feature. Every new file is
graph-enabled: `Database::create` writes a v5 file with header flag
`FLAG_GRAPH = 4` (an explicit v3/v4 create stays plain);
`maintenance::enable_graph` writes a graph-enabled copy of a v4 or v5 file.
Graph commits are op-12 frames on the same commit path as vectors, so locking,
sequences, checksums, encryption and commit timestamps work as usual.

- **Limits.** Graph names follow the collection-name rule. At most 64 graphs
  exist at once, and a dropped graph's ID can be reused. A frame holds
  1..=4,096 entries and a body of at most 16 MiB. Names, keys, labels and
  properties live only in the body, which is encrypted in encrypted files.
- **Open.** Each graph frame is verified, decrypted, decoded, validated and
  applied atomically. A malformed frame fails the open with `Corrupt`; there
  is no partial graph state.
- **Writes.** `create_graph` and `drop_graph` (empty graphs only) commit one
  frame each. `graph_mut(name).write_batch(&[GraphOp])` applies up to 4,096
  node, edge, attachment and vector operations, in any collection, in order as
  one atomic frame; nothing is written on error. Attached records must be live
  and without expiry when the frame commits. While a record is attached,
  plain, conditional, batch and import deletes fail with
  `GraphAttachedRecord`, and setting an expiry fails with
  `GraphAttachedRecordExpiry`. Overwrites and metadata updates keep the
  attachment. Detach first to delete.
- **Reads.** `graph(name)` offers node and edge gets, paged `find_nodes` and
  `neighbors`, breadth-first `traverse`, canonical `shortest_path` and
  `explain`. `ReadOptions` bounds work and output and supports cancellation;
  exceeding a hard ceiling is an error. Results report `view_sequence`,
  `complete`, truncation reasons and work statistics, and a truncated
  shortest-path search returns `Exhausted`, never an unproven "no path". Paged
  cursors are bound to the query and graph sequence: graph writes and
  attached-record updates invalidate them, unrelated vector writes do not.
- **Maintenance.** `inspect`, `inspect_detail`, `verify`,
  `backup`, `restore`, `compact*`, `reencrypt`, `restore_to`, `drill`,
  `upgrade`, log export and read replicas handle graph files. Graph files are
  already the latest format, so `upgrade` makes a validated copy that keeps the
  graph flag and every graph frame. `compact*` keeps graph IDs and
  never reuses node or edge IDs; like vector compaction, it restarts sequences
  and drops commit timestamps. `maintenance::inspect_graph` and `verify_graph`
  report graph, node, edge and attachment counts. `maintenance::strip_graph`
  copies the vector data to a v5 file without graphs and needs
  `confirm_graph_data_loss = true`. `vlite-maintain` gains `enable-graph` and
  `strip-graph --confirm-graph-data-loss`.
- **Change feed.** Since v0.6.0 `feed_cursor`, `changes_after` and
  `read_log_changes` report graph commits as `GraphCreated`, `GraphDropped`
  and `GraphBatch` events, and `/v1/changes`, gRPC `Changes`/`WatchChanges`
  and webhooks serve them; see
  [Change feed and webhooks](GRAPH.md#change-feed-and-webhooks).
- **Unsupported.** Releases before v0.5.0, and v0.5.0 builds without `graph`, reject graph
  files with `UnsupportedFeature(4)`. The server maps graph errors to 409
  (state conflicts) or 422 (invalid input). Schemas, hybrid
  retrieval, query text and the server, MCP and SDK surfaces are in the
  [property graph guide](GRAPH.md).
- **CLI.** `vlite-graph-cli` runs these reads and write batches with JSON; see
  [Graph operations](TOOLS.md#graph-operations).

## Periodic (lazy) durability

Since v0.5.1 file handles default to `Durability::Periodic` with a 60 s
interval (1 ms–60 s allowed): commits reach the OS immediately and a background
flusher syncs them, for vector and graph writes alike. A process crash loses
nothing acknowledged; an **OS crash or power loss can lose up to about the last
interval (~60 s by default)**. `Durability::Full` syncs every commit before
acknowledgement. `Durability::None` is the in-memory mode: nothing touches
disk and all data is lost on restart. Use `sync()` for an explicit durable
point; `close()` syncs and a failed sync poisons the handle. The mode is not
persisted. Server: `--durability periodic|full|none [--flush-interval-ms N]`;
C ABI, Python and Node: `durability` and `flush_interval_ms` plus `sync`. See
[periodic durability](REFERENCE.md#periodic-lazy-durability) for reopen cases.

## Key-value store

`Database::kv_store(collection)` (Rust only, v4+) stores namespaced string keys
with byte values, no vector needed. It uses an ordinary one-dimension
collection, created on first use, so the file format is unchanged.

```rust
use vlite::{Expected, kv::KvPut};
let mut kv = db.kv_store("agent-memory")?;
let token = kv.put("pool", "claim/42", b"worker-a", KvPut {
    expected: Expected::Absent,        // create only
    expires_at_ms: Some(lease_end_ms), // v5 files only
})?;
kv.delete("pool", "claim/42", Expected::Revision(token))?;
```

- **Versions** are commit sequences. Use them for compare-and-set
  (`Expected::Revision`) and as fencing tokens. They change after compaction,
  upgrade or restore, so re-read the entry after any of those.
- **Expiry** hides an entry at its deadline. `Expected::Absent` then
  succeeds. Expiry on a v4 file returns `ExpiryRequiresUpgrade`.
- **Listing** with `view().list(namespace, prefix, after, limit)` returns keys
  in order, up to 10,000 per page. It scans the whole collection.
- **Limits:** namespaces up to 255 bytes, keys up to 1,024 bytes, values just
  under 1 MiB.
- **Hash collisions:** keys hash to 16 consecutive record IDs. If all 16 hold
  other keys, `KvSlotsExhausted` is returned. Other records in the collection
  are never overwritten.
- REST, gRPC and bindings come after the OpenAPI review.

## Directory store

`Database::create_directory(path, dimensions, options)` (Rust only, v5)
stores a database as a directory instead of one file. Every other API,
including `Database::open`, `open_read_only`, the server and the maintenance
tools, accepts the directory path unchanged.

```text
store/
  LOCK            held while open (exclusive, or shared for read-only)
  MANIFEST        ordered list of the live files, replaced atomically
  image-N.vlite   checkpoint image, optional
  wal-N.log       write-ahead logs; only the last one is appended
```

- **Checkpoints** fold the logs into one image on a background thread, so
  reopen loads the image plus a short log instead of the whole history.
  One starts after a commit once the active log passes 64 MiB. Change it with
  `set_checkpoint_threshold(Some(bytes))`, turn it off with `None`, or run one
  and wait with `checkpoint()`, which returns the floor sequence.
- **Reopen** maps the image and sealed logs read-only and loads the image's
  records and graphs in bulk, in parallel, instead of replaying them; only
  the active log is replayed. Earlier releases cannot open a directory that
  this release has checkpointed, or a backup packed from one.
- **Kept across checkpoints:** the lineage, every record's revision
  (compare-and-set and fencing tokens still match), graphs and encryption.
- **History below the floor is gone.** Change-feed and log-segment requests
  for an earlier cursor return `CursorBeforeHistory` (REST 409
  `cursor_before_history`). Take a snapshot, then resume from the floor. A
  new replica of a checkpointed store starts from a backup.
- **Converters:** `maintenance::convert_to_directory(file, dir)` keeps the
  lineage, revisions and cursors. `maintenance::backup(dir, file)` packs a
  directory back into one file.
- **Not supported:** commit timestamps and v3 or v4 formats. Crash recovery
  trims a torn tail of the active log only; damage elsewhere is reported as
  corruption.

## Limitations

- A passphrase is only as strong as its entropy; tune `KdfParams` for your
  hardware.
- Restore, rotation and replica creation need free space for a full copy.
- REST and gRPC expose a subset of these APIs; see [API.md](API.md) and
  [GRPC.md](GRPC.md). Your application manages replica shipping, GPU search
  and WASM persistence.
