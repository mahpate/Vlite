# Committed read snapshots

`Database::read_snapshot()` captures the default collection;
`CollectionRef::read_snapshot()` captures a named collection. Both return an
owned `ReadSnapshot`. Capture while holding your normal database-owner lock,
then release that lock before executing exact searches or browsing:

```rust
# fn example(db: &mut vlite::Database) -> vlite::Result<()> {
let before = db.read_snapshot()?;
db.put(10, &[1.0, 2.0], b"new value")?;
// Reads the committed generation captured before that write.
let hits = before.search(&[1.0, 2.0], 10)?;
# Ok(())
# }
```

The caller still serializes mutation and capture through the database owner.
Snapshots are `Send + Sync`, retain no file handle, and can outlive the database.
`sequence()` and `lineage()` identify the captured committed state. No snapshot
can be created from a poisoned owner or an internal, not-yet-synced durability
group. A previously captured committed snapshot remains usable after a later
write failure.

## Visibility and lifetime

* Exact distances still accumulate in `f64`, with ascending ID breaking ties.
* Record vectors, payloads and metadata are immutable within a capture.
  Atomic batches appear entirely before or entirely after capture.
* Default `get`, `browse`, and search TTL visibility uses the capture's clock.
  Their `_at` variants accept an explicit Unix-millisecond clock.
* `len()` counts physical records, including records hidden by TTL.
* Capture is O(1). The first subsequent mutation copies the ID tree, sharing
  unchanged immutable records; only modified record bodies require copying.
  A metadata-only update can therefore copy that record's vector/payload while
  a snapshot or background rebuild retains the previous generation.
* There is no retained-history registry. Old bodies are released when the last
  snapshot/rebuild referencing them is dropped.

The per-database budget is eight distinct captures, each limited to 100,000
records and 128 MiB of logical record bodies. Clones share one capture's budget;
dropping only some clones does not release it. These bounds exclude allocator
overhead, metadata allocation overhead and the current live database.
Capture returns `InvalidQuery` when limits are exceeded. Applications can
explicitly fall back to their ordinary lock-held reads.

`Database::snapshot_retention_stats()` reports `active_captures` and
`retained_logical_bytes` for monitoring. The byte count adds the logical body
size of every active capture at its capture time; it intentionally counts
shared bodies once per distinct capture, just like the per-capture retention
budget. It is not an allocation estimate, a unique-live-body count, or RSS.
Snapshot clones do not add to either counter, and the counters return to zero
when the final clone of each capture is dropped. A rejected capture does not
change them. The two counters are sampled independently, so concurrent capture
or reclamation can briefly expose values from adjacent states.

### Record allocation and locality trade-off

The shared representation is used even when no snapshot is active. Instead
of storing each `Record` inline in a B-tree leaf, each leaf stores one pointer
to a separately allocated `Arc<Record>`. This introduces an extra pointer
dereference and allocation per live record. Vectors and payloads were already
separate allocations; this does **not** pack vectors contiguously or improve
their scan locality. Small-vector or metadata-heavy workloads can be more
sensitive to this change than the measured 512-dimensional workload.

On a typical 64-bit build, the Arc allocation adds two pointer-sized reference
counts and the tree stores an additional pointer: a nominal 24 bytes per live
record compared with inline records, before allocator metadata, alignment and
B-tree capacity effects. Actual resident memory is not bounded by this
estimate and has not been measured here. Startup also now allocates one Arc
per reconstructed record. The observed reopen improvement comes from CRC
validation and replay-buffer changes, not a claim that Arc reconstruction is
faster or smaller.

In return, captured generations avoid copying vector, payload and metadata
bodies. A first write with a retained generation duplicates the ID tree and
increments record references, rather than copying every body. Snapshots and
rebuilds can retain superseded bodies until their last reference disappears;
the logical retention limits above are deliberately not RSS limits.

Snapshots currently support `get`, paginated `browse`, and exact filtered
vector search. They do **not** retain HNSW, metadata, text, sparse, disk or GPU
indexes. Index-backed operations should continue using ordinary collection
methods. REST exact unindexed searches without reranking use this mechanism;
other server paths retain their existing owner locking.

## Background HNSW generations

Unquantized background builds share the same immutable record generation
instead of copying every vector synchronously. Limits still account for
logical f32 vector bytes; additionally the complete retained record bodies
(including payloads/metadata) must not exceed 256 MiB. Quantized builds still
train and encode their bounded u8 snapshot synchronously.

Every committed record mutation advances the collection's vector version and
the job's bounded catch-up version. Publication verifies that these versions
agree. Cancellation, explicit supersession, catch-up overflow, worker failure
or an uncovered version cannot publish a stale worker graph. There is still
one background job per database and no automatic retries.

## Startup

The datafile remains the sole source of truth. Startup still validates and
replays the complete log, then reconstructs derived indexes; there is no new
checkpoint or format version. IEEE CRC-32 validation uses a portable
slicing-by-eight table with the same polynomial, streaming behavior and
on-disk checksums. The component profile and end-to-end example are available:

```text
cargo test --release --lib replay_component_profile -- --ignored --nocapture
cargo run --release --example engine_snapshot_perf -- candidate
```

These are bounded synthetic, warm-cache measurements, not throughput or
production restart guarantees.

## Controlled mixed read/write evidence

`examples/engine_mixed_perf.rs` compares lock-held and snapshot exact scans
against the **same** engine binary. One reader begins a stable-generation
scan before one writer attempts a durable mutation. It verifies result IDs
and f64 distance bits against an independently mirrored pre-write state,
then verifies the committed post-write state after every round. Three
alternating-order paired trials use 8,192 records, 128 dimensions, one
reader/writer, one Rayon thread and 24 rounds per mode.

On the recorded native Windows run, median writer lock wait changed from
0.710 ms to 0.0002 ms, and median durable write total from 1.221 ms to
0.828 ms. This was **not a universal latency improvement**: median reader
total increased from 0.729 ms to 0.903 ms; writer mutation-plus-sync work
increased from 0.466 ms to 0.828 ms. The snapshot write maximum was
14.523 ms, versus 2.838 ms lock-held. Snapshot writers completed before the
read returned in 41/72 rounds, compared with 0/72 lock-held rounds.

Separate warmed in-memory pairs exclude fsync and expose first-write tree
copying: median first write was 0.002 ms without a retained capture and
0.124 ms with one. Read totals include generation release, whereas scan
timings exclude it. Concurrent COW/refcount traffic and reclamation can
therefore make reads slower despite removing scan-duration writer blocking.

This is a deliberately reader-first contention schedule with warm caches
on a shared development host, **not** a production arrival-rate model,
throughput guarantee, historical pre-Arc comparison or memory measurement.
All raw samples, p95/max values, exact commands and binary/source digests
are preserved in
[`benchmarks/results/engine-mixed-windows.json`](../benchmarks/results/engine-mixed-windows.json).

```powershell
$env:CARGO_TARGET_DIR = 'target\perf2-engine-measure'
$env:RAYON_NUM_THREADS = '1'
cargo run --locked --release --example engine_mixed_perf -- mixed-final
```
