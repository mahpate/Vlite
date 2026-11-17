# HA reader-apply measurement protocol (ITEM-001, ITEM-002)

This is the exported-segment first cut of MEAS-001/002/007, **not an HA
qualification or a benchmark campaign**. It uses existing engine APIs only.
It does not implement a continuously serving reader, an archive consumer,
fencing or a gateway. The ITEM-001 `--smoke` mode does not prototype the reader
strategies; only the ITEM-002 `--measure` mode does, as local harness
prototypes rather than runtime reader implementations. It chooses
no strategy, staleness bound, enrollment limit or production capacity ceiling.
GATE-001 and GATE-002 remain open. The ITEM-001 text below describes the
`--smoke` mode; ITEM-002 added a separate `--measure` mode that prototypes
the three strategies on local synthetic fixtures (see
[ITEM-002 local strategy measurements](#item-002-local-strategy-measurements)).
The owner confirmed incremental apply with baseline-plus-tail recovery.
ITEM-003 still needs qualified MEAS-007 evidence; runtime refusal will be wired
into the later reader enrollment path (see the HA PRD ITEM-003 blocker note).

## Reproduce the bounded smoke

From the repository root in PowerShell:

```powershell
cargo test --locked --features json --example ha-reader-apply --test replica
cargo run --quiet --locked --features json --example ha-reader-apply -- --smoke
```

The second command prints one JSON document. Redirect stdout to a new artifact
path outside the repository to retain observations. Build diagnostics go to
stderr. The harness accepts exactly one argument, `--smoke` or (since ITEM-002)
`--measure`, and rejects anything else. It takes no database, endpoint,
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
| MEAS-001 strategy comparison (a)/(b)/(c) | **Measured locally by ITEM-002** on synthetic fixtures only (see below). Representative workloads and ACA qualification remain UNMEASURED. Detached index-build time is not evidence that any strategy is qualified. |
| MEAS-002 Blob upload/verified-head latency | **UNMEASURED**. Requires EPIC-PITR-001 baseline-first archive and actual durable-progress/consumer interfaces (Q-2/Q-4/Q-6); memory path Q-5 also remains open. Local segment save is not a substitute. |
| MEAS-002 ACA staleness distribution | **UNMEASURED**. Requires the archive, polling/scheduling, reader serving mode and representative load on the intended platform, with source-commit and visible-position correlation. No p50/p99 or staleness bound is derived from these three samples. ITEM-002 recorded a local single-process ack-to-exact-visible distribution for strategy (b) only; it is not an ACA staleness distribution. |
| MEAS-007 local/ACA capacity | **Local sampled file-length high-water marks and compaction measured by ITEM-002**; otherwise **UNMEASURED** beyond the enumerated fixture file lengths. Requires baseline download/staging lifetime, allocated-space peaks, compaction scratch, process/index memory, vCPU storage tier and workload-size qualification; do not infer a ceiling from smoke sizes. |
| Archive continuity and fencing | **UNMEASURED**. Exported segments have lineage and sequence, but no fenced writer epoch, durable archive head, retention pins or Blob fault outcomes. Q-1..Q-6 and GATE-001 remain open. |

ITEM-001 delivers this protocol and repeatable first-cut harness only. An
owner-approved future qualification must record the exact source, host and
filesystem, durability/format/index settings, workload and concurrency, every
cost boundary, raw results and correctness evidence. Only that later evidence
can support ITEM-002's strategy decision or ITEM-003's capacity/refusal work.

## ITEM-002 local strategy measurements

**Scope: SYNTHETIC_LOCAL_MEASUREMENT_NOT_ACA_SLO.** These numbers come from local
files on one shared developer host, using synthetic fixtures. They involve no Azure,
network, Blob or production data. They do not qualify any strategy for ACA,
and they set no staleness SLO or capacity ceiling. GATE-002 remains open.

### Command and source

```powershell
cargo run --quiet --release --locked --features json --example ha-reader-apply -- --measure > <path outside the repo>.json
```

- Artifact (outside the repo): `C:\Users\mahpate\.copilot\session-state\53d6b337-fd4c-4687-94a7-8e2a27d67b53\files\ha-item002-measure-20261117T051850Z.json`.
  Progress was written to stderr in `...051850Z.json.stderr.log`. Total harness elapsed time was 266.7 s.
- Source revision: `607f9b22590e55a72bc95933776a0411c842f438` (package 0.7.8).
  `working_tree_dirty=true`. The only dirty paths were the owner's three
  uncommitted `deploy/azure-container-apps/{README.md,deploy.ps1,test.ps1}` edits.
  The harness sources were clean at that revision.
- Host: Windows, x86_64, 16 logical CPUs, `rustc 1.99.1 (58cf8f9c3 2026-10-13)`,
  release profile. The temporary files were on `Q:\Temp`. The host was shared:
  other sessions and builds may have run concurrently, and the OS cache was warm and
  uncontrolled. No cold-cache claim is made.
- An earlier run at revision `3cc29131` exited nonzero partway through the
  100,000-record cells. The ANN verification query (ef_search 128) missed the exact
  top-1. No numbers from that run are used. `607f9b22` raised only the
  verification `query_ef` to 512. The HNSW build parameters did not change.

### Parameters

| Parameter | Value |
|---|---|
| Fixture | splitmix64 vectors in [0,1), 64 dimensions, 64-byte payload, one collection, V5 single-file, unencrypted |
| Baseline ingest | `write_batch` of 1,000 records per commit, Full durability |
| Sizes × deltas | {1,000, 10,000, 100,000} records × {100, 1,000} mutations |
| Delta mix | for k = i/3: update ID 2k, delete ID 2k+1, insert ID records+k. One Full-durability commit each, so a delta of 100 is 100 commits |
| HNSW | m 16, ef_construction 64, seed 42, squared L2, no quantization. Verification query k 1, ef 512 |
| Repetitions | 5 per cell. Strategy order rotated per rep: (a,b,c), (b,c,a), (c,a,b), ... |
| Segment cap | 256 MiB per `export_log` |
| MEAS-002 | 10,000-record baseline, 200 commits/s target, Full durability, 15 s per ship interval, intervals 250 ms and 1,000 ms |

The 100,000-record top size was kept because the whole run took 266.7 s, within
the roughly 15-minute budget.

### What each strategy times

- **(a) snapshot refresh.** The steps are:
  1. Source-side `maintenance::backup`.
  2. Reader-side copy, using `maintenance::backup` from the shipped snapshot.
  3. Ship cleanup.
  4. `Database::open_read_only`.
  5. `build_hnsw`.
  6. Swap: drop the old handle and delete the old file.

  `refresh_inclusive` covers all of these and excludes harness verification and
  byte sampling. Because `backup` takes an exclusive lock, the source writer is
  closed for the source backup. A live-writer baseline path is UNMEASURED.
- **(b) incremental apply.** `exact_visible_inclusive` covers `export_log` after
  the follower position, `LogSegment::save`, load/verify,
  `ReplicaFollower::apply` and segment cleanup. `index_ready_inclusive` adds the
  follower drop, a detached `open_read_only` and a **full** `build_hnsw`.
  `ReplicaFollower` drops in-memory indexes on apply and exposes only
  `&Database`, so **in-place incremental index maintenance is UNAVAILABLE** today.
  The follower is closed while the index is rebuilt.
- **(c) baseline replacement.** The steps are:
  1. Copy the latest baseline.
  2. Run `export_log` for the tail since that baseline, then `save`.
  3. `ReplicaFollower::open`, load/verify and `apply`.
  4. Cleanup and follower drop.
  5. `open_read_only` and `build_hnsw`.
  6. Swap.

  The tail length equals the delta, which is the mutation count since the baseline.
- **Restore stand-in (deviation).** `maintenance::restore` assigns a new lineage,
  and the follower in (c) would refuse the tail. The harness therefore uses
  `maintenance::backup`, which is byte-identical and keeps lineage, for the
  reader-side restore in (a) and (c).
- **Verification.** Every rep checks the reader against an in-memory model: ids,
  vectors, payloads, lineage and head sequence. It also checks that the approximate
  top-1 for the updated record equals the exact top-1. Any failure exits nonzero.
  This run verified every rep.
- **fsync and replay.** These run inside the single `apply` call, so
  `apply_fsync`/`apply_replay` are null (UNMEASURED).

### MEAS-001 results (ms, median with min–max over 5 reps; converted from raw ns in the artifact)

| Records | Delta | (a) refresh_inclusive | (a) build_hnsw | (b) exact_visible_inclusive | (b) apply_inclusive | (b) index_ready_inclusive | (c) refresh_inclusive | (c) apply_inclusive |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1,000 | 100 | 344.8 (335.4–357.2) | 328.4 (321.4–343.9) | 4.5 (4.3–11.2) | 2.1 (1.9–8.5) | 340.7 (323.7–345.2) | 334.5 (315.7–352.2) | 2.0 (1.9–2.1) |
| 1,000 | 1,000 | 367.8 (338.5–398.2) | 338.0 (305.8–364.9) | 12.9 (12.4–13.7) | 5.8 (5.6–6.5) | 350.2 (340.1–365.5) | 357.5 (343.3–367.2) | 6.0 (5.6–12.8) |
| 10,000 | 100 | 648.2 (622.2–673.3) | 569.8 (546.1–593.7) | 17.6 (16.6–19.2) | 11.4 (11.1–11.9) | 588.6 (570.2–627.2) | 634.4 (619.4–648.3) | 11.1 (10.5–11.6) |
| 10,000 | 1,000 | 666.8 (631.9–688.9) | 571.7 (538.9–598.7) | 24.1 (23.5–25.3) | 14.3 (13.9–15.2) | 592.8 (579.1–656.0) | 650.6 (626.6–665.2) | 14.4 (14.2–14.7) |
| 100,000 | 100 | 5,979.1 (5,696.1–6,347.2) | 5,275.1 (4,971.2–5,624.1) | 145.4 (143.4–159.4) | 107.7 (106.0–110.2) | 5,390.9 (5,259.8–5,493.6) | 5,892.2 (5,678.7–6,013.9) | 103.4 (101.4–104.8) |
| 100,000 | 1,000 | 6,067.0 (5,873.0–6,132.2) | 5,342.6 (5,169.9–5,374.9) | 160.1 (152.0–167.6) | 116.7 (109.2–124.2) | 5,533.0 (5,412.4–5,630.6) | 5,878.5 (5,704.0–6,028.3) | 109.0 (107.2–113.3) |

At 100,000 records, the copy phases took the following time (median ms):

| Strategy | Phase | 100 delta | 1,000 delta |
|---|---|---:|---:|
| (a) | `source_backup` | 298.9 | 309.0 |
| (a) | `reader_restore` | 298.2 | 298.1 |
| (c) | `baseline_restore` | 308.3 | 304.9 |

Segment sizes were 26,756 serialized bytes for 100 commits and 265,556 for
1,000 commits.

Observations. The ratios are derived from the medians above.

- In every cell, the full `build_hnsw` takes most of the index-ready time for every
  strategy. At 100,000/100, (a) `build_hnsw` is 5,275.1 ms of a 5,979.1 ms
  `refresh_inclusive`, about 88% (derived).
- (b) exact-visible cost grows with database size even for a fixed delta: 4.5,
  17.6 and 145.4 ms at a delta of 100. It stays far below a full refresh. At
  100,000/100, 5,979.1 / 145.4 ≈ 41× (derived), for (a) refresh versus (b) exact
  visible.
- Index-ready times for (a), (b) and (c) are within about 11% of each other in
  each cell. At 100,000/100 they are 5,979.1, 5,390.9 and 5,892.2 ms. All three
  are dominated by the same full HNSW build.

### MEAS-002 local staleness, strategy (b) only

The latency is measured from the writer's `put` returning, captured under the
writer lock, to `ReplicaFollower::apply` returning, captured with a monotonic clock.
Everything runs in one process on the local filesystem. It **excludes Blob upload,
the durable archive head, ACA scheduling and network**. It is not an SLO, and it
measures exact-read visibility, not index readiness.

| Ship interval | Achieved commits/s | Commits | Ships | p50 ms | p90 ms | p99 ms | max ms | min ms | Per-ship apply_inclusive median (min–max) ms |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 250 ms | 199.86 | 3,001 | 61 | 152.3 | 252.1 | 272.7 | 287.3 | 16.8 | 16.5 (10.6–24.6) |
| 1,000 ms | 199.82 | 3,001 | 16 | 528.7 | 928.8 | 1,017.9 | 1,025.9 | 17.4 | 16.7 (11.1–23.3) |

The final follower state verified equal to the source after each interval. In
both runs, per-ship `apply_inclusive` rose over the 15 s window (raw series in the
artifact) while the follower database grew; the
cause was not isolated.

Staleness for (a) and (c) is **UNMEASURED**. No distribution and no modeled
bound is given. The refresh costs in MEAS-001 are per-refresh costs, not
per-commit staleness.

**Erratum (post-review).** An earlier version of this section, and the saved
artifact's `unmeasured.staleness_for_a_and_c` annotation, stated that the
staleness of (a)/(c) was at least the "refresh interval + refresh cost", as a
derived lower bound. That is incorrect as a per-commit bound. For example, a
commit just before a snapshot cutoff waits almost no interval time.

That annotation is superseded. The harness annotation now reads UNMEASURED
without a bound. The artifact is preserved unchanged (SHA-256
`C285130DF3B807D5559E002C0BEAABA3BA5F217735D984816EF8FB63A6CE9D04`), and its
measured values remain valid.

### MEAS-007 local sampled bytes

These are logical file lengths sampled at phase boundaries. They are not allocated
blocks, RSS or transient staging between samples. The reader directory and the
ship (snapshot) directory are sampled separately. Source bytes are a test
colocation cost and are excluded. All 5 reps gave identical byte values.

| Records | Delta | Reader file bytes | (a) reader peak (ratio) | (a) ship peak | (b) reader peak (ratio) | (c) reader peak (ratio) |
|---:|---:|---:|---:|---:|---:|---:|
| 1,000 | 100 | 362,724 | 698,808 (1.927) | 362,724 | 389,480 (1.074) | 725,564 (2.000) |
| 1,000 | 1,000 | 601,524 | 937,608 (1.559) | 601,524 | 867,080 (1.441) | 1,203,164 (2.000) |
| 10,000 | 100 | 3,387,192 | 6,747,744 (1.992) | 3,387,192 | 3,413,948 (1.008) | 6,774,500 (2.000) |
| 10,000 | 1,000 | 3,625,992 | 6,986,544 (1.927) | 3,625,992 | 3,891,548 (1.073) | 7,252,100 (2.000) |
| 100,000 | 100 | 33,631,872 | 67,237,104 (1.999) | 33,631,872 | 33,658,628 (1.001) | 67,263,860 (2.000) |
| 100,000 | 1,000 | 33,870,672 | 67,475,904 (1.992) | 33,870,672 | 34,136,228 (1.008) | 67,741,460 (2.000) |

The ratio is the derived reader peak divided by the reader file bytes.

- The (a) reader peak occurs at `reader_restore`, when the old and new reader files
  coexist. At that phase the separately sampled ship snapshot is also present. For
  example, the 100,000/100 samples were reader 67,237,104 and ship 33,631,872.
  A colocated reader volume would therefore hold about 3× the reader file (derived).
- The (c) reader peak is the old reader plus the new baseline-plus-tail during
  `apply`.
- The (b) peak is the follower file plus the in-flight segment.

Compaction (`maintenance::compact` on the post-delta, reader-size database; one run per size, 1,000-mutation delta; peak from ~1 ms background polling):

| Records | Input bytes | Output bytes | Sampled peak bytes | Peak ÷ input (derived) | compact ms |
|---:|---:|---:|---:|---:|---:|
| 1,000 | 601,524 | 389,116 | 990,640 | 1.647 | 16.4 |
| 10,000 | 3,625,992 | 3,890,116 | 7,516,108 | 2.073 | 106.3 |
| 100,000 | 33,870,672 | 38,900,116 | 72,770,788 | 2.148 | 1,077.2 |

Output records were verified equal to the input. At 10,000 and 100,000 records the
compacted file is **larger** than its input. The harness does not investigate the
cause and records it as observed.

### Still UNMEASURED or unavailable after ITEM-002

- Blob upload latency and the durable or verified archive head. `ObjectStore` is
  server-internal, and no network or Blob use was authorized.
- The ACA staleness distribution. The ACA capacity, vCPU and storage tiers.
- Separate fsync and replay time inside `apply`. This would require engine
  instrumentation.
- Process peak memory. No existing dependency exposes it.
- In-place incremental HNSW maintenance on a follower. This is unavailable in the
  engine today.
- A live-writer (online) baseline. The (a) source backup required the writer to be
  closed.
- Allocated-block disk usage and staging between samples. Cold-cache behaviour.
  Representative (non-synthetic) workloads. Sizes above 100,000 records.
- The ITEM-003 capacity ceiling and enrollment refusal.
