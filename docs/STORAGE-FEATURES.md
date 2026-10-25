# Storage features

Opt-in engine features: encryption at rest, point-in-time restore, change feed,
read replicas, disk-based ANN, GPU exact search and a WebAssembly build.
V5-only persisted extensions remain opt-in. Change feeds and sequence
restore also support V4. V1–V4 encodings are unchanged, and older binaries refuse
V5 files they do not understand (unknown header flags give `UnsupportedFeature`).
No operation here modifies a source file in place: each one writes a new file
through staged, fully revalidated publication, or reads a closed or copied file.

## Shared-lock read-only snapshots

`Database::open_read_only(path)` and
`open_read_only_encrypted(path, &key)` open existing files read-only under a
shared OS file lock. Multiple processes can read the same file; a writer still
requires exclusive ownership. Conflicting opens return `Error::Locked`, not a
wait. Handles see the committed state at open time; reopen to observe later
commits. They do not read alongside an active writer. Mutations return
`Error::ReadOnly` without poisoning, and incomplete tails are rejected rather
than repaired. `is_read_only()` reports the mode. In-memory ANN builds are
allowed without writing the datafile. These APIs add no new file format.

Python: `native.open(path, read_only=True)`; Node:
`native.open(path, {readOnly: true})`; C ABI Open: `"read_only": true`.
The C ABI serializes calls per process, so parallel embedded Python reads use
separate processes. The server instead keeps its exclusive file lock and uses
an RwLock for concurrent read routes and exclusive writes. See
[reference locking contracts](REFERENCE.md) and [MCP read-only access](MCP.md).

## Encryption at rest (#15)

```rust
let key = EncryptionKey::from_bytes([7; 32]);            // or from_passphrase(..)
let db = Database::create_with_options(path, 384, CreateOptions {
    encryption: Some(key), ..Default::default() })?;
let db = Database::open_encrypted(path, &key)?;
```

- **Cipher/KDF.** ChaCha20-Poly1305 (RustCrypto, pure Rust). A per-file key is
  derived from your key plus a random per-file salt. A passphrase goes through
  Argon2id (`KdfParams`); its salt and cost parameters are stored in the header.
- **What is encrypted.** Each frame body (vectors, payloads, metadata) is sealed
  as `nonce || ciphertext || tag`. The header carries a non-secret key ID, the
  salt and the KDF parameters.
- **What is not.** Frame prefixes stay plaintext so recovery can walk the log.
  They reveal operation type, collection ID, record ID, sequence number and
  sizes. The file size and timing also leak. Treat this as protection of record
  contents, not of access patterns.
- **Wrong key.** Opening with a wrong key gives `EncryptionKeyMismatch` (it
  reports the file's key ID). A missing key gives `EncryptionKeyRequired`, and a
  key for an unencrypted file gives `NotEncrypted`. A damaged frame fails its
  authentication tag, so garbage is never returned.
- **Rotation.** `maintenance::reencrypt(src, dest, old_key, new_key)` copies to a
  new file under the new key with a new salt, then reloads and validates it. Retire
  the old file and its backups yourself after verifying the new one.
- **Keyed maintenance.** `inspect_with_key`, `verify_with_key`, `backup_with_key`,
  `restore_with_key` and `compact_with_key` work on encrypted files. The keyless
  functions return `EncryptionKeyRequired`. Backups of encrypted files are
  byte-identical copies and stay encrypted.
- **Sidecars.** ANN snapshots and disk-ANN files hold plaintext vectors, so they
  are refused for encrypted databases (`SidecarUnavailable`). Both are
  rebuildable, so rebuild HNSW in memory after open instead.
- **Not covered.** Key storage and distribution, in-memory data while the
  process runs, swap and core dumps, and the server's own sidecar files.

## Point-in-time restore (#14)

Create the database with `CreateOptions { commit_timestamps: true, .. }` to
record a wall-clock time per commit (a small anchor frame). Then:

```rust
maintenance::restore_to(Some(base_backup), log_copy, dest,
    RestoreTarget::Sequence(42) /* or TimestampMs(ms) */, key)?;
```

- The log is a copy of the full committed log (the closed primary file, a
  backup, or a follower's file). The optional base must have the same lineage
  and be an exact byte prefix of the log, which is verified.
- The destination is a new, validated file that gets a **new lineage**. The
  sources are never modified. A target before the base, or beyond the log, is
  `InvalidRestoreTarget`. A timestamp restore on a file without timestamps is
  `TimestampsUnavailable`.
- `compact` rewrites the log, drops the timestamp flag and assigns a new
  lineage, so history before a compaction cannot be restored by time. Keep the
  pre-compaction file or backup for that.
- Sequence restore needs no timestamps and works on any V4/V5 log.

## Change feed (#5)

```rust
let mut cursor = db.feed_cursor()?;                 // or FeedCursor::beginning(lineage)
let batch = db.changes_after(&cursor, 500)?;        // ChangeBatch { changes, next, has_more, head }
cursor = batch.next;
cursor.save(path)?;                                 // durable (tmp + fsync + rename)
```

- Events: put (with record and expiry), delete, metadata update, collection
  created/dropped, settings updated, vectors updated. A batch commit is never
  split across feed batches, so a batch may exceed `max_events`.
- **Expiry** is not an event. TTLs appear as `expires_at_ms` on the put, and
  consumers decide when a record expired.
- Cursors carry the lineage and sequence and a text token with a CRC.
  `CursorCompacted` means the file was compacted or restored since the cursor was
  taken, so history is gone and the consumer must resync. `CursorAhead` means the
  cursor is past the head, and `InvalidCursor` means the token is damaged.
- Needs V4+ (`ChangeFeedRequiresUpgrade` otherwise). Encrypted files are read
  with the database's key. Each call scans the log from the start, so cost is
  O(log size). Keep batches large and poll sparingly.
- `changes::read_log_changes(path, key, cursor, max)` reads a closed or copied
  log without opening a database.

## Read replicas (#4)

```rust
let segment = primary.export_log(follower_sequence, 1 << 20)?;   // LogSegment
let mut follower = ReplicaFollower::create(path, &segment, key)?; // first segment
follower.apply(&next_segment)?;
let lag = follower.status()?;   // applied_sequence, primary_head, lag
follower.db().search(..)?;      // read-only access
```

- Segments are whole commits cut from the primary log, authenticated with CRCs
  (and AEAD tags for encrypted files), and must continue the follower's last
  sequence and lineage. Gaps, replays and foreign lineages are rejected
  (`ReplicaDiverged`) and leave the follower unchanged (the file is truncated
  back).
- `put`, `delete` and `write_batch` on a follower return `ReadOnlyReplica`.
  `promote()` closes the follower and returns a normal writable `Database`.
- File shipping: `LogSegment::save`/`load`, `apply_file`, and `sync_from_log(path,
  max_bytes)` for a primary file that is closed or copied. On Windows a live
  primary holds an exclusive lock, so ship `Database::export_log` segments
  instead.
- **Cost.** Each apply replays the follower's whole file (O(database size)) and
  drops any in-memory ANN. Batch your applies. A crashed apply's incomplete tail
  is discarded when the follower reopens. A network transport is not provided.

## Disk-based ANN (#9)

```rust
let source = db.build_disk_ann("default", sidecar, HnswConfig::default(), DistanceMetric::SquaredL2)?;
let index = DiskAnnIndex::open(sidecar, DiskAnnOptions { cache_bytes: 64 << 20 })?;
let hits = db.search_disk_ann(&index, &query, 10, /*ef*/ 100)?;
```

- A rebuildable sidecar (128-byte header, fixed-stride node records with their
  own CRC, the upper graph layers, and a footer). It holds the HNSW graph and
  the f32 vectors, and it is bound to the database's lineage and sequence.
  Any later commit makes it `StaleIndex`, so rebuild it. It is never trusted
  after damage (`verify()` checks every node).
- **Query memory** is bounded: the header and upper layers stay resident and
  nodes are read by `pread` into an LRU cache capped at `cache_bytes`.
  `memory()` reports resident bytes, cache use, hits and disk reads.
- **Build memory** is that of an in-memory HNSW build, so collections larger
  than RAM must be built on a larger machine or in pieces and then queried on a
  small one. This is documented rather than hidden.
- Recall matches the in-memory graph at the same `ef` (the same graph is
  walked). The tests assert recall@10 ≥ 0.9 and within 0.05 of in-memory HNSW,
  and exact results at exhaustive `ef`. It indexes records as stored (no
  metadata filters or expiry) and supports squared L2 and cosine.
- Replacing a sidecar that a `DiskAnnIndex` still has open fails on Windows;
  drop the old index first. Not available for encrypted databases.

## GPU exact search (#18)

Enable cargo feature `gpu` (wgpu, MSRV-compatible, pure compute shaders).

```rust
let gpu = GpuExactSearcher::new();                          // None without an adapter
let hits = GpuExactSearcher::search_or_cpu(gpu.as_ref(), &db, &query, 10)?;
```

- Squared-L2 over the default collection. The GPU scores in f32. The best
  `4k + 32` candidates are re-scored in f64 on the CPU, so reported distances
  match `Database::search`. Near-ties closer than f32 error may reorder at the
  boundary of the top-k.
- With no adapter, or on GPU failure (`GpuUnavailable`), `search_or_cpu` uses the
  CPU path. Software adapters (WARP, llvmpipe) work but are slow.
- Not for filtered, cosine or non-default-collection search. Use the CPU path.

## WebAssembly (#19)

The engine compiles for `wasm32-unknown-unknown` with `--no-default-features`.
`Database::create_in_memory`, `open_from_bytes` and `to_bytes` use a byte-buffer
backend with no file locking, and every format feature (encryption, change feed,
timestamps) works against it. The host persists the image. `set_wasm_clock_ms`
supplies time because there is no system clock. Background HNSW rebuilds need
threads and return an error. `bindings/wasm` is a wasm-bindgen package with a
Node smoke test (see its README). Path-based APIs compile but fail at runtime
there. The in-memory backend works natively too, which is how it is unit-tested.

## Graph-enabled files (experimental, `graph` feature)

The non-default Cargo feature `graph` (not shipped in release artifacts) stores
named property graphs in the same log as vectors. `Database::create_with_graph`
creates a V5 file with header flag `FLAG_GRAPH = 4`. Encryption and commit
timestamps work as usual. Ordinary files never become graph-enabled in place
(`maintenance::enable_graph` writes a graph-enabled copy).
`create_graph` and `drop_graph` (empty graphs only) each commit one op-12 frame;
`list_graphs` reads the directory without a commit. The frame goes through the
same writer, sequence, lock,
checksums, sealing and timestamp anchors as vector commits. Vector frames in a
graph file are byte-identical to an ordinary V5 file.

Format rules:

- Names follow the collection-name rule. At most 64 graphs can exist at once;
  the internal ID of a dropped graph can be reused.
- Op 12 is valid only in V5 files carrying the flag.
- The frame prefix is plaintext: collection 0, record ID 0, 1..=4,096 entries,
  body ≤ 16 MiB. Graph names, keys, labels and properties exist only in the
  body, which is encrypted in encrypted files.
- On open, each graph body is checksum-verified, decrypted, fully decoded and
  validated, then applied atomically. A malformed or inconsistent graph frame
  fails the whole open with `Corrupt`; there is no partial graph state.
- A graph body may interleave graph entries with vector-mutation entries (one
  canonical op-3 batch entry each, decoded with the target collection's
  dimensions) and node attachments. Vector entries apply in body order at the
  frame's sequence; every attachment the body touches must end on a live
  record without expiry.

Compatibility:

- Builds without `graph` and older releases reject graph-enabled files with
  `UnsupportedFeature(4)`.
- In graph builds, maintenance carries graph files: `inspect`,
  `inspect_detail`, `verify`, `backup`, `restore`, `compact*`, `reencrypt`,
  `restore_to`, `drill` and log-segment export accept them, and read replicas
  replay their graph frames. `compact*` rewrites each graph (IDs kept) and
  records its node and edge high-water marks, so node and edge IDs are never
  reused; like vector compaction it restarts sequences and drops commit
  timestamps. `maintenance::inspect_graph` and `verify_graph` add the graph
  flag and graph, node, edge and attachment counts. `maintenance::enable_graph`
  copies a v4 or v5 file out as a graph-enabled file, and
  `maintenance::strip_graph` copies the vector data of a graph-enabled file out
  as a v5 file without graphs (it needs `confirm_graph_data_loss = true`).
  `vlite-maintain` gains `enable-graph` and `strip-graph
  --confirm-graph-data-loss` in graph builds.
- `maintenance::upgrade` rejects graph-enabled files with
  `GraphUnsupportedOperation`. Change feeds (`Database::feed_cursor`,
  `changes_after` and `changes::read_log_changes`) reject them with
  `GraphChangeFeedUnsupported`. The server answers `/v1/changes` with 501 and
  gRPC with `UNIMPLEMENTED`, refuses webhook subscriptions on such files, and
  maps the other graph errors to 409 (state conflicts) or 422 (invalid input).
- `graph_mut(name).write_batch(&[GraphOp])` writes nodes, edges, attachments
  and vector mutations of any collection as one atomic op-12 frame (at most
  4,096 operations; nothing is written on any error). Attaching requires a
  live record without expiry. While a record is attached, ordinary delete,
  conditional delete, batch delete and import deletes fail with
  `GraphAttachedRecord` and setting an expiry fails with
  `GraphAttachedRecordExpiry`, without writing; overwrites and metadata
  updates are allowed and keep the attachment. Detach first to delete.
- `graph(name)` provides node/edge gets, paged `find_nodes` and `neighbors`,
  breadth-first `traverse`, canonical `shortest_path` and `explain`. Read results
  carry `view_sequence`, `complete`, truncation reasons and work statistics.
  `ReadOptions` bounds work and output and supports cancellation; exceeding a
  hard ceiling is an error, not a silent clamp. A truncated shortest-path search
  reports `Exhausted`, never an uncertified no-path answer.
- Paged cursors are bound to the query and graph sequence. Graph writes and
  updates of attached records invalidate them; unrelated vector writes do not.
  A read-only database retains its open-time view. Schema/property indexes,
  hybrid graph retrieval, and graph-specific server/MCP/SDK APIs are later work.

## Durability and security caveats

- The in-memory backend is durable only as far as you persist `to_bytes()`.
- Encryption protects contents, not metadata in frame prefixes. Losing the key
  loses the data; there is no recovery.
- A passphrase is only as strong as its entropy; tune `KdfParams` for your
  hardware.
- Restore, rotation and replica creation need free space for a full copy.
- REST/gRPC expose a subset of these APIs; see [API.md](API.md) and
  [GRPC.md](GRPC.md). Replica apply/follower ownership, GPU search and WASM
  persistence remain application-managed, not network-follower services.
