# HA reader-apply measurement protocol (ITEM-001)

This is the exported-segment first cut of MEAS-001/002/007, **not an HA
qualification or a benchmark campaign**. It uses existing engine APIs only.
It does not implement a continuously serving reader, an archive consumer,
fencing, a gateway, or any of the three proposed reader strategies. It chooses
no strategy, staleness bound, enrollment limit or production capacity ceiling.
GATE-001 and GATE-002 remain open; ITEM-002/003 remain Not Started.

## Reproduce the bounded smoke

From the repository root in PowerShell:

```powershell
cargo test --locked --features json --example ha-reader-apply --test replica
cargo run --quiet --locked --features json --example ha-reader-apply -- --smoke
```

The second command prints one JSON document. Redirect stdout to a new artifact
path outside the repository to retain observations. Build diagnostics go to
stderr. The harness accepts **only** `--smoke`; it takes no database, endpoint,
credential or production input. It creates fresh temporary files on the local
temporary filesystem and removes them on success or error. Run from this
checkout so the reported Git revision/dirty flag identifies the harness source.
Record the command and retain that source revision/diff alongside the output.
The reported `rustc` is the compiler on PATH at execution time; reproduce using
the same build toolchain. A dirty-source capture is not a released-build result.

Two deterministic fixtures contain 8 and 32 baseline records, 8 dimensions,
64-byte payloads, and three rounds of update/delete/insert (three independently
acknowledged Full-durability commits per round). The source is an unencrypted,
single-file V5 log with one collection, no timestamp anchors, no graph, memory
bindings, admin state or ANN snapshot sidecars. Vectors use the modular formula
in `vector`; HNSW uses m=4, construction ef=16, seed=42, squared L2, no
quantization. A ready-only ANN query uses k=1, ef=64, and must match the exact
answer. This tiny query check is not a recall study.

The fixture sizes are correctness probes, not a scaling curve. No concurrent
load, repetitions for statistical significance, timing thresholds, warmup
policy or cold-cache control is provided. Debug and release numbers must not be
compared. Host contention, filesystem, hardware and OS caches affect all times.
No Azure resources, network operations or production copies are used.

## Procedure and boundaries

1. Ingest and close the synthetic source. Use `maintenance::backup` to publish a
   validated byte-identical baseline, then the same API to restore a follower
   copy. Open it through `ReplicaFollower`. Record each inclusive operation.
2. Update ID 0, timestamp its successful return with a monotonic local clock,
   then delete an existing ID and insert a new ID. Export a delta after the
   follower position with a 1 MiB frame target. The smoke asserts all three
   commits fit this segment; the engine target is not a hard encoded-size cap.
3. Save the `VLSEG001` file, load/verify it, and apply it to the follower.
   Verify all fixture IDs, vectors, payloads, metadata, revisions and counts,
   as well as lineage, applied sequence and zero observed lag. Record the time
   from the oldest acknowledgement to this verified exact-read visibility.
4. Drop the follower, open the same file with `Database::open_read_only`, build
   HNSW explicitly, and check a ready-only query. Record reopen and index-build
   durations separately, and the inclusive oldest-ack-to-detached-index-ready
   interval. Drop this handle and reopen `ReplicaFollower`; verify the data and
   applied position again before the next delta.
5. Outside the acknowledgement intervals, refuse duplicate, gap and foreign
   lineage applies; reject corrupt and truncated serialized files at the
   decoder. Verify rejected operations leave the follower file unchanged.
   Resume from the reopened position in the next round.
6. Sample file lengths; retain the baseline and all deltas through the case.
   Verify empty-tail apply is a no-op, close the source and follower, check
   byte-for-byte log equality, and independently verify the follower file.

All durations are raw monotonic nanoseconds, not distributions or promises.
Failed verification exits nonzero; no success JSON is printed on a case failure.

| Output | Includes / excludes |
|---|---|
| `baseline.backup_inclusive_ns`, `restore_backup_inclusive_ns` | Existing backup API: validation, staging, sync, publication and inspection. Not an isolated copy or Blob baseline-download measurement. Windows directory-entry power-loss qualification is not implied. |
| `baseline.follower_open_ns`, `follower_reopen_ns` | Follower open/full replay/residency work. Not apply, transport, or index building. |
| `export_ns` | `Database::export_log`, including scanning/validation and segment construction. Not serialization or Blob upload. |
| `save_ns` | Serialization, local staged write, sync and rename via `LogSegment::save`. Not network or a durable archive head commit. |
| `load_verify_ns` | Local read, parse and SHA-256 verification. Not apply. |
| `apply_inclusive_ns` | Append, fsync and full database replay as one engine operation. **Do not add separately estimated fsync/replay costs.** `apply_fsync_ns` and `apply_replay_ns` are null (UNMEASURED). |
| `read_only_reopen_ns`, `detached_index_build_ns` | Separately reopened read-only database and explicit in-memory HNSW build. The follower has no mutable database view, so this is not an in-place follower rebuild or an implemented runtime reader strategy. |
| `oldest_ack_to_exact_visible_ns` | First successful write return through the remaining writes, export, local shipping, apply and exhaustive data verification. Not commit-start-to-visible age, per-record staleness or archive staleness. |
| `oldest_ack_to_detached_index_ready_ns` | The preceding interval plus dropping the follower, separate read-only reopen, index build and exact/ready-query check. Includes harness work; **overlaps** the previous interval and operation timings. Not additive or a serving SLO. |
| `frame_bytes`, `serialized_segment_bytes` | Raw frames versus actual serialized segment file length (including envelope, header and digest). |
| `files.retained_staging_bytes` | Baseline plus all saved delta file lengths. Excludes the source and active follower. |
| `files.sampled_total_bytes` | Source + follower + retained staging, cross-checked against the case directory. Logical lengths sampled after each round, **not a high-water mark**, allocated disk blocks, memory use, transient `.part` files, compaction headroom or ACA capacity. The source is a test-only colocation cost. |

`ReplicaFollower` replays on apply and drops its in-memory indexes, but exposes
only `&Database`. This experiment deliberately avoids `promote` or engine changes
to obtain a mutable index-building handle. It observes index construction on
the identical local state after a separate read-only reopen. It does not measure
index loss on an indexed continuously serving follower. Reopening also resets
the follower's last-observed primary head to its applied sequence; the smoke
reaches the exported head before each reopen and makes no live lag claim.

## Concrete gates before extending these observations

| Measurement / dependency | Status and required evidence |
|---|---|
| MEAS-001 separate fsync/replay costs | **UNMEASURED**. Requires individually instrumented engine phases; this slice measures only inclusive apply. |
| MEAS-001 strategy comparison (a)/(b)/(c) | **UNMEASURED**. Requires implemented strategies and owner-approved representative non-production workloads. Detached index-build time is not evidence that any strategy is qualified. |
| MEAS-002 Blob upload/verified-head latency | **UNMEASURED**. Requires EPIC-PITR-001 baseline-first archive and actual durable-progress/consumer interfaces (Q-2/Q-4/Q-6); memory path Q-5 also remains open. Local segment save is not a substitute. |
| MEAS-002 ACA staleness distribution | **UNMEASURED**. Requires the archive, polling/scheduling, reader serving mode and representative load on the intended platform, with source-commit and visible-position correlation. No p50/p99 or staleness bound is derived from these three samples. |
| MEAS-007 local/ACA capacity | **UNMEASURED** beyond the enumerated fixture file lengths. Requires baseline download/staging lifetime, allocated-space peaks, compaction scratch, process/index memory, vCPU storage tier and workload-size qualification; do not infer a ceiling from smoke sizes. |
| Archive continuity and fencing | **UNMEASURED**. Exported segments have lineage and sequence, but no fenced writer epoch, durable archive head, retention pins or Blob fault outcomes. Q-1..Q-6 and GATE-001 remain open. |

ITEM-001 delivers this protocol and repeatable first-cut harness only. An
owner-approved future qualification must record the exact source, host and
filesystem, durability/format/index settings, workload and concurrency, every
cost boundary, raw results and correctness evidence. Only that later evidence
can support ITEM-002's strategy decision or ITEM-003's capacity/refusal work.
