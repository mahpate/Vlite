# Graph measurements (NFR-002 / NFR-003, ITEM-021)

Companion to [`feature-agent-graph.prd.md`](feature-agent-graph.prd.md), [`decisions.md`](decisions.md) and [`baseline.md`](baseline.md). These are measurements, not guarantees. The NFR-003 numbers are proposed targets for review at GATE-004.

## Status: reference measurement collected; GATE-004 review pending

Five full-scale, paired runs (latency benchmark and memory split) were collected on 2026-10-24 and are summarized below as medians with min–max ranges. The raw report, including every per-run value, is [`measurements/epic005-reference-0.4.1.json`](measurements/epic005-reference-0.4.1.json).

Read these figures with four limits in mind:

1. **The host was not idle.** It is a VM that never became quiet: a 3-minute passive survey measured 4.99% mean busy time (17% of half-second samples above 10%, worst 45.1%). Collection therefore used a calibrated background-load gate (see [Collection protocol](#collection-protocol)) instead of an idle-machine requirement. The numbers are reference measurements of a shared, normally busy VM, not idle-hardware numbers.
2. **The base commit does not contain the measured code.** The graph is still uncommitted, so the report records `d6b4757` with `working_tree_dirty: true`. Identity comes from the SHA-256 of both executables and of a 97-entry source manifest, not from the commit.
3. **This is not the DO-005 graph-off performance record.** That record is still [`baseline.md`](baseline.md) at `81ff901`; remeasuring graph-off performance at the release commit remains a separate REG-010/GATE-008 obligation.
4. **This is an input to review, not an approval.** Nothing here closes AC-022 or GATE-004.

Still open:

1. Hybrid retrieve p50/p99. `hybrid_retrieve` is `"not_available"` in the report; it arrives with the hybrid-retrieval epic (ITEM-039/EPIC-010).
2. Property-index memory. The report records `property_indexes` as 0 bytes with status `not_implemented_epic_008`, because property indexes do not exist yet (EPIC-008).
3. Re-collection once the graph is committed, if a gate needs a commit-identified record.
4. The GATE-004 decision on whether the in-memory cost described under [Memory](#memory-nfr-002-in-memory-split) is acceptable. This is a review input and RISK-005 stays open.

## Reference run identity

| Field | Value |
|---|---|
| Collected | 2026-10-24, 23:19:35–23:26:59 UTC (five paired runs) |
| Base commit | `d6b47573af54b75c218b09d149e3e14a4c207519`, `working_tree_dirty: true` (graph uncommitted) |
| Toolchain | `rustc 1.99.1 (58cf8f9c3 2026-10-13)`, LLVM 23.1.1, `x86_64-pc-windows-msvc`; release profile, `--locked`, `--features graph` |
| Host | Intel Xeon Platinum 8370C @ 2.80 GHz, 16 logical CPUs, 64 GiB RAM (68,665,831,424 B), Windows 11 Enterprise build 10.0.26300; a VM |
| Storage | `Q:` on ReFS, backed by a "Msft Virtual Disk" on a SAS controller; physical backing unknown; Defender real-time scanning active and not excludable |
| `graph_bench.exe` SHA-256 | `43589abf471f90205853e195a48c4cf783622ddacc56078181043e15d2fec3fd` |
| `graph_memory.exe` SHA-256 | `9e7031435b98295916a10bf4ece4f6c43e3c3943fa1edd6bdcb6a4a57c5fdce7` |
| Source identity | 97-entry `source_sha256` manifest in the report (`Cargo.toml`, `Cargo.lock`, `build.rs`, every `.rs` and `Cargo.toml` under `src`, `examples` and `crates`, and the runner). The runner re-checks it after each repetition, and it was recomputed once more after collection with zero differences |

The hardware matches the DO-005 description in [`baseline.md`](baseline.md). The report's `reference_identity_checks` (CPU model, 16 logical CPUs, RAM size, OS build, `Q:` ReFS volume, virtual SAS disk) are all true and `active_builds` is empty; the runner refused to start otherwise. Since 2026-10-27 the runner checks the Azure DO-005 baseline host instead (see [`baseline.md`](baseline.md)), so this report's host is retired and its numbers are not comparable with a new run.

## Collection protocol

- **Runner.** `scripts\graph_reference.py` performs five repetitions. Each one takes an idle probe, then runs the latency executable, then the memory executable. It samples Windows guest CPU every 0.5 s, records the peak working set of each process, and writes one JSON report. It never builds, changes Git state or stops other processes. `tests\graph_reference_runner.py` (8 tests, run in CI) covers the gate logic.
- **Workload for every run.** 100,000 nodes, 1,000,000 edges, 1,000 queries per read, seed 1, batches of 4,096.
- **Background-load gate.** Before each repetition the runner takes a 5 s idle probe and goes ahead only if the guest CPU used by everything except the benchmark process had a mean of at most 10% and no half-second sample above 50%. A failing probe is retried, up to 12 attempts, and the rejected probes stay in the report under `preflight_rejected`. Each benchmark run must meet the same gate; if one does not, or a process fails, or the source files change, the runner stops instead of discarding and repeating the run. Whether a repetition is kept therefore depends only on background CPU and process success, never on the measured values.
- **Why the gate is not stricter.** The first attempt used a 2% mean / 10% sample gate and was refused after 6 s (probe mean 13.4%, worst sample 26%). The partial report is not published. The survey above showed that only 13% of 5 s windows met that gate and none of the 50 s or 100 s windows did, about the length of a benchmark run, so the gate was recalibrated to the figures above (roughly twice the measured mean, and the burst size seen). See `decisions.md`, "Reference collection protocol".
- **Result of collection.** All five repetitions passed on the first probe, with no rejected probes, every process exit code 0 and memory `ok: true`.

Background CPU during the accepted runs (fraction of 16 logical CPUs, excluding the benchmark process; mean / worst half-second sample):

| Pair | Probe | Latency run | Memory run |
|---|---|---|---|
| 1 | 4.3% / 17.4% | 4.3% / 21.9% | 4.4% / 24.8% |
| 2 | 7.0% / 13.9% | 4.0% / 16.5% | 5.3% / 25.3% |
| 3 | 3.8% / 16.0% | 6.0% / 21.9% | 4.6% / 17.2% |
| 4 | 5.5% / 11.7% | 5.6% / 26.7% | 2.9% / 12.3% |
| 5 | 6.4% / 16.4% | 7.2% / 29.8% | 2.5% / 8.1% |

Benchmark-run means were 2.5–7.2% (median 4.5%); worst samples were 8.1–29.8% (median 21.9%). Guest CPU accounting does not show hypervisor or physical-disk activity, so these figures bound only what the guest can see.

## Benchmarks

Two release executables are measured. They are separate programs so the allocator counting used for memory never affects the latency numbers.

`examples\graph_bench.rs` (`[[example]]`, `required-features = ["graph"]`) measures time and log bytes:

```
cargo run --release --locked --features graph --example graph_bench -- [--nodes 100000] [--edges 1000000] [--queries 1000] [--seed 1] [--dir PATH]
```

`examples\graph_memory.rs` (`[[example]]`, `required-features = ["graph"]`) builds the same workload, then measures retained and peak allocator bytes per NFR-002 component. It uses a counting global allocator (`examples\graph_support\counting_alloc.rs`); both examples share one generator (`examples\graph_support\workload.rs`). It prints a JSON document and exits non-zero if the components do not add up to the retained total, if memory is not released back to the starting baseline, or if a read leaves a different live count than it found.

The collection runner builds nothing; build once and pass both executables:

```
cargo build --release --locked --features graph --example graph_memory --example graph_bench
python scripts\graph_reference.py --latency-exe <graph_bench.exe> --memory-exe <graph_memory.exe> --machine-json <machine.json> --work-dir <scratch dir> --output <report.json>
```

- **Workload.** Nodes `n0..n{N-1}`, alternating labels `Doc`/`Person`, one integer property `w`. Edges are keyless, with uniformly random source and destination (self-loops and parallel edges are possible), one of four relationship types and an integer `w`. Everything is written in atomic graph batches of 4,096 operations into a fresh file with two-dimensional vectors and no records, attachments or vectors on graph elements. The generator is SplitMix64 with `--seed`.
- **`graph_bench` metrics.**
  - Build time.
  - Log bytes per node and per edge: file growth over the node and edge phases.
  - Bytes per batch and per operation: the write amplification of graph batches.
  - Read-only open/replay time: the median of three opens.
  - p50/p99 of three reads, each over `--queries` seeded starting nodes and recorded with its outcomes: `neighbors` (outgoing, limit 100, default limits); `traverse` (2 hops, default limits); and `shortest_path` between random pairs (default limits, 2 hops).
- **Timing.** Each read is timed around the public `GraphRef` call on a warm, read-only handle, one call at a time.
- **`graph_memory` metrics.** The memory split of the state rebuilt by replaying the file, the peak allocator bytes while opening it, and the per-call peak and retained output bytes of the same three reads. `graph_memory --smoke` runs a reduced 10,000-node / 100,000-edge / 100-query version for development; it is not reference evidence.

## Results

Medians of the five accepted repetitions, with the minimum–maximum in parentheses, for the 100,000-node / 1,000,000-edge workload. Per-repetition values are in the raw report.

### Build, log size and replay (`graph_bench`)

| Metric | Median (min–max) |
|---|---|
| Build, first batch to database close | 18.075 s (17.887–18.804) |
| of which node batches | 0.813 s (0.808–0.828) |
| of which edge batches | 16.476 s (16.273–17.206) |
| of which closing the database after the last batch | about 0.78 s (0.778–0.786), derived by subtraction |
| Graph batches | 270 (identical in every repetition) |
| File size | 65,552,022 B = 102 B empty graph + 5,040,290 B node batches + 60,511,630 B edge batches (identical) |
| Log bytes per node / per edge | 50.4 / 60.5 (identical) |
| Bytes per batch / per operation | 242,785 / 59.6 (identical) |
| Read-only open and replay (median of 3 opens, per repetition) | 3.518 s (3.466–3.610) |
| Replay per million edges | 3.518 s (3.466–3.610); the file holds exactly 1,000,000 edges and 100,000 nodes |

The closing time is `drop(db)` after the last batch. It includes releasing the in-memory state and was not separated from any final flush.

### Read latency (`graph_bench`)

Serial calls on a warm read-only handle, default limits except where stated, 1,000 seeded calls per repetition.

| Read | p50 | p99 | Outcomes (identical in every repetition) |
|---|---|---|---|
| `neighbors`, outgoing, limit 100 | 34.8 µs (33.0–36.1) | 69.5 µs (65.1–75.3) | 0 incomplete |
| `traverse`, 2 hops | 376.3 µs (372.6–398.5) | 677.2 µs (676.5–749.8) | 570 of 1,000 incomplete (`OutputLimit`); mean 90.0 nodes returned |
| `shortest_path`, 2 hops | 60.2 µs (56.4–70.2) | 122.0 µs (116.4–168.1) | 0 found, 1,000 `NoPathWithinHops`, 0 exhausted |

### Memory (NFR-002 in-memory split)

Requested allocator bytes and live allocator blocks retained after replaying the log's 271 verified frames (270 batches plus the graph creation) into the in-memory graph state (`graph_memory`). The memory report is identical in all five repetitions, and the components add up to the retained total exactly (0 B and 0 blocks unattributed; memory returned to the starting baseline).

| Component | Bytes | Blocks | Share | Per unit |
|---|---:|---:|---:|---|
| Edge records | 760,780,054 | 3,166,664 | 74.9% | 760.8 B and 3.17 blocks per edge (about 240 B per block) |
| Node records | 101,633,322 | 516,666 | 10.0% | 1,016.3 B and 5.17 blocks per node |
| Outgoing adjacency | 72,633,462 | 1,174,134 | 7.1% | |
| Incoming adjacency | 72,497,462 | 1,173,802 | 7.1% | |
| Adjacency together | 145,130,924 | 2,347,936 | 14.3% | 145.1 B per edge |
| Node key index | 6,483,322 | 115,460 | 0.6% | |
| Label index | 1,961,897 | 16,667 | 0.2% | |
| Key and label indexes together | 8,445,219 | 132,127 | 0.8% | 84.5 B per node |
| Graph overhead (name index 309 B, graph table 2,768 B, graph name 5 B) | 3,082 | 4 | < 0.01% | |
| Edge key index, attachments, attachment owners | 0 | 0 | 0% | zero by construction here: keyless edges, no attachments or vectors |
| Property indexes | 0 | 0 | 0% | not implemented (EPIC-008) |
| **Total retained** | **1,015,992,601** | **6,163,397** | 100% | about 1,016 B per edge for the whole graph; 15.5 times the 65,552,022 B log |

- Replay peaked at 1,018,602,633 B above its starting live count, only 2,610,032 B above what it retains.
- Opening the file read-only retains 1,015,999,992 B in 6,163,403 blocks, which is 7,391 B and 6 blocks more than the replayed state alone. Its peak above baseline is 1,019,009,052 B. The example's replay and the library's open therefore agree to within 0.001% on retained bytes.

Temporary memory of one read, on the open read-only handle (bytes; the `unbalanced_calls` count was 0 for every read, meaning each call returned the live count to where it started once its result was dropped):

| Read | Peak above baseline: p50 / p99 / max | Retained output: p50 / p99 / max |
|---|---|---|
| `neighbors`, limit 100 | 18,412 / 33,455 / 36,244 | 17,819 / 32,862 / 35,651 |
| `traverse`, 2 hops | 187,588 / 269,305 / 278,092 | 176,038 / 247,259 / 247,342 |
| `shortest_path`, 2 hops | 1,588 / 3,204 / 3,396 | 8 / 8 / 8 |

### Process peak working set

Peak working set of the whole benchmark process, as reported by Windows. It includes the build phase and, for the memory executable, the planned frames, and is not comparable with the allocator split above.

| Executable | Median (min–max) |
|---|---|
| `graph_bench` | 1,143,640,064 B, about 1.065 GiB (1,143,635,968–1,144,152,064) |
| `graph_memory` | 1,219,743,744 B, about 1.136 GiB (1,209,356,288–1,219,743,744) |

Elapsed time per run was 35.82 s (35.29–36.31) for `graph_bench` and 47.95 s (47.39–48.18) for `graph_memory`. The instrumented `graph_memory` timings are not reported as latencies; use the uninstrumented `graph_bench` figures.

## What the figures mean

- **Memory unit.** Requested allocator bytes (`Layout::size`) and live block counts from a counting global allocator. This is not RSS, committed memory, allocator headers, padding or size-class rounding, and it misses memory that bypasses Rust's global allocator (the operating system, C libraries, memory maps), stacks and static data. Resident memory is higher; only the process peak working set above is a resident-size figure.
- **Measurement windows.** The counters are process-wide, and each measurement window runs on the main thread while no other thread allocates. `peak_above_baseline_bytes` is the highest live byte count inside the window minus the live count when the window started. It is not a process peak: for replay and open, the planned frames (about the size of the log) are live at the start and are excluded. A `realloc` is accounted as an allocation of the new block, a copy and a release of the old one, so a growing buffer's peak counts both blocks.
- **What the split describes.** The state rebuilt by replaying the file's verified frames. The example checks that the frames it replays equal the frames in the file, byte for byte except for checksums. A writer that also planned and rolled back batches might leave a slightly different tree shape. The example compiles its own copy of the engine source (`#[path]`) to attribute components; the 0.001% agreement with the library's open above is the cross-check.
- **Workload scope.** Uniform random keyless edges, four relationship types, two labels alternating `Doc` and `Person`, an integer `w` on every node and edge, batches of 4,096, and no attachments or vectors on graph elements. Per-node and per-edge figures do not transfer to graphs with keyed edges, attachments or large property maps. One graph size was measured, so scaling with size is not characterized here.
- **Reads.** Serial calls on a warm handle at default limits. Output is capped at 100 nodes and 200 edges, but exploration is not capped: with about 10 outgoing edges per node (1,000,000 edges over 100,000 nodes), a full 2-hop expansion visits about 110 nodes, so the latency covers the whole expansion even when the output is cut off at the cap (`OutputLimit`, 57% of `traverse` calls).
- **Shortest paths.** At the default 2 hops, random pairs never connected in this workload (0 of 1,000 in every repetition), so the measured cost is the bounded search that certifies `NoPathWithinHops`, not path construction. A workload whose paths are found has not been measured.
- **Spread.** Across repetitions, (max − min) / median is 4–6% for build and replay time, 7–15% for `neighbors` and `traverse` p50 and p99, and 23% (p50) and 42% (p99) for `shortest_path`. Guest CPU accounting does not show hypervisor or physical-disk activity, and Defender real-time scanning was active.
- **Determinism.** The memory report was identical in all five repetitions. It carries no commit or toolchain; the runner report does.

## Against the proposed targets (NFR-003, NFR-004, RISK-005)

These are inputs for the GATE-004 review. This report does not approve a gate.

- **NFR-003.** `neighbors` p99 is 69.5 µs at the median and 75.3 µs in the worst repetition, against a proposed target of at most 1 ms: a margin of at least 13 times. `traverse` p99 is 677.2 µs at the median and 749.8 µs in the worst repetition, against at most 25 ms: a margin of at least 33 times. Both margins are far larger than the spread between repetitions, so the conclusion does not depend on the background-load tolerance. The caveats above (serial warm-handle calls, output caps) apply. `shortest_path` has no proposed target.
- **NFR-004.** At the default limits, the worst of 1,000 `traverse` calls peaked at 278,092 B above its starting live count (p99 269,305 B), against 36,244 B for `neighbors` and 3,396 B for `shortest_path`. These are measurements at one set of limits on one graph; they do not show how the bound scales with `max_visited_nodes` and `max_edges_examined`.
- **RISK-005 and the in-memory cost.** The in-memory graph holds about 1.0 GB for 100,000 nodes and 1,000,000 edges, about 1,016 B per edge for the whole graph and 15.5 times the 65.6 MB log. Edge records are 75% of it (3.17 allocator blocks, about 240 B each, per edge). Replay takes 3.5 s per million edges and the process peak working set was 1.07–1.14 GiB. Whether this is acceptable, or whether a more compact representation is warranted, is a decision for the GATE-004 review. The "~40 bytes/edge" estimate remains withdrawn.

NFR-002 component coverage:

| NFR-002 component | Reported as |
|---|---|
| Log records | 50.4 B per node, 60.5 B per edge |
| In-memory records | 1,016.3 B per node, 760.8 B per edge |
| Adjacency | 145.1 B per edge |
| Key and label indexes | 84.5 B per node (no edge keys in this workload) |
| Property indexes | not implemented: 0 B, status `not_implemented_epic_008` |
| Temporary traversal memory | per-read peak and retained output (table above) |
| Replay time per million edges | 3.518 s |
| `neighbors`, `traverse`, `shortest_path` p50 and p99 | read latency table |
| Hybrid retrieve p50 and p99 | not available until ITEM-039 (EPIC-010) |
| Write amplification per graph batch | 242,785 B per batch, 59.6 B per operation |

## Superseded provisional figures

An earlier single run on a machine whose load was not recorded (build 20.946 s, replay 3.873 s, `neighbors` p50 and p99 35.5 and 69.5 µs, `traverse` 402.5 and 793.2 µs, `shortest_path` 69.7 and 138.4 µs) and a 10,000-node smoke run are superseded by the figures above and were never published as evidence.

## Raw data

[`measurements/epic005-reference-0.4.1.json`](measurements/epic005-reference-0.4.1.json) holds, for every repetition, the complete output of both executables, the CPU samples, the process peak working set, and the machine, toolchain, hash and protocol records. `completed` is `true`.

## In-memory adjacency compaction (local development, not DO-005)

The in-memory adjacency indexes now use per-node sorted vectors instead of
tree sets. Relationship names are interned across both directions using thin
shared pointers: an adjacency entry is one pointer plus one `EdgeId` (16 B
on 64-bit targets), with vector capacity and per-node map overhead in addition.
Ordering remains lexical relationship type followed by edge ID, not symbol
insertion order. Typed scans and cursors binary-search the vectors, and merging
both directions still emits self-loops once. Symbols are removed when their
last edge is removed, including during validation and failed-batch rollback.
The write log and frame codecs are unchanged.

This is an adjacency improvement, not a claim that S4's whole-graph target has
been reached. Authoritative node and edge records still retain their existing
string and property-map representations to preserve reference-returning engine
interfaces. The counting allocator attributes the intern table and shared
relationship allocations to `in_adjacency`, which is released after both
directions; component totals still include every owned allocation.

The development environment refused Cargo execution without interactive
approval. Consequently no local before/after release memory or latency result
is available, and formatting, Clippy, workspace tests and graph doctests could
not run. The reference values above (1,015.99 B/edge total, 145.13 B/edge
adjacency; neighbors p50/p99 34.8/69.5 us and traverse 376.3/677.2 us) are
historical, not a local baseline. Re-run `graph_memory` and `graph_bench` in
release mode at 100,000 nodes, 1,000,000 edges and 1,000 queries before making
a memory-reduction or latency-nonregression claim. No DO-005 campaign was run.
