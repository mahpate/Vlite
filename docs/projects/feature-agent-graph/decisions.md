# feature-agent-graph — Decision outputs DO-001..DO-006 (EPIC-002)

Companion to [`feature-agent-graph.prd.md`](feature-agent-graph.prd.md) v1.2.10, Section 4.12. This records EPIC-002 decisions and supporting evidence; implementation now exists for the graph feature, op tag 12, adapter and public API, but no release gate is approved by this document. The DO-001/DO-002 compatibility probes ran against commit `81ff901c54d674d1ad519dd4fa722e937db46c4b` (HEAD when EPIC-001/EPIC-002 started) and release tags `v0.1.0`, `v0.1.1`, `v0.1.2`, `v0.2.0`, `v0.3.0`, all available locally; every revision was extracted with `git archive` into `%TEMP%` and built offline into its own target directory. That evidence is historical and is not regenerated. The REG-001..REG-008 regression and surface-parity baseline was re-pinned to `347b01dcb437051fd02816443f8f0bbad082b866` in v1.2.10 (see [Baseline re-pin](#baseline-re-pin-v1210)).

| ID | Status | Outcome | Approval still needed |
|----|--------|---------|-----------------------|
| DO-001 | **Decided** (deterministic rule satisfied) | V5 + `FLAG_GRAPH = 4` | None for the rule; the format itself is part of the GATE-002 proposal review |
| DO-002 | **Decided** (deterministic rule satisfied) | Attachments (PD-012) | None for the rule |
| DO-003 | **Proposed final, pending review** | Op tag 12 (lowest unused, proven); the shared engine codec implements tags 1..11 and VLite's graph-aware loader dispatches op-12 frames through `property_graph::frame::replay`. Vector mutations reuse `encode_batch_entries`; public graph-batch writing is ITEM-015 and not yet implemented. | GATE-003 and EPIC-003 code review must accept the full entry table, including the `PatchNode`/`PatchEdge` field-name length change (`u16`, see below) |
| DO-004 | **Decided** (re-grep done) | LEG-001..LEG-020 plus LEG-021..LEG-026 | Folded into PRD Section 3.4 (v1.2.9, before EPIC-004 code) |
| DO-005 | **Recorded (re-baselined 2026-10-27)** | Release 0.6.0 (`6428b69`) on an Azure Standard_D16s_v5 VM, 5 runs per workload, 100,000 × 384 with 10 queries, paired with a same-host rerun of `81ff901`; all 60 runs passed the unchanged CPU gate. See [`baseline.md`](baseline.md) and [Re-baseline on Azure](#re-baseline-on-azure-owner-decision-2026-10-27) | REG-010 budgets (PD-014) remain proposals for GATE-004. The retired `81ff901` workstation record is kept in `baseline.md` for history |
| DO-006 | **Chosen under delegation** | PD-001 string keys (1..=256 UTF-8 bytes, unique per graph) + internal monotonic `u64` IDs, never reused | Chosen by the delegated workflow on the user's instruction to proceed; formal GATE-002 sign-off still to be recorded |

## DO-001 — Graph-file marker

**Rule (PRD 4.12):** select V5 + `FLAG_GRAPH = 4` if every REQ-055 entry point in HEAD-default and in each release tag rejects a V5 file with flag bit 4 set (plus an op-12 frame) explicitly. Pre-V5 tags (`v0.1.0`..`v0.2.0`) must answer `UnsupportedVersion(5)`. `v0.3.0` and HEAD must answer `UnsupportedFeature(4)`.

**Fixtures** (`tests\fixtures\graph_compat\`, SHA-256 manifest `MANIFEST.sha256` over decoded bytes):

| File | Content |
|------|---------|
| `v5_graph_plain.hex` | V5 header (flags = 4, valid header CRC) + one op-12 frame (draft DO-003 body, valid prefix CRC and commit footer) |
| `v5_graph_timestamps.hex` | Same with `FLAG_COMMIT_TIMESTAMPS` (flags = 4 \| 2) |
| `v5_control_plain.hex`, `v5_control_timestamps.hex` | Identical files without bit 4 (controls; must open) |
| `v5_control_op12_without_flag.hex` | Op-12 frame smuggled into a non-graph V5 file (must be rejected as corruption) |
| encrypted fixture | Created at run time by each revision's own public API (encrypted V5), then bit 4 set and the header CRC recomputed; header-only (no op-12 frame), because a sealed op-12 body would require the revision's AEAD internals |

The fixtures are produced by two independent encoders (Python in `scripts\graph_compat_check.py fixtures`, Rust in `tests\graph_compat.rs`); `graph_compat.rs` asserts that they agree byte for byte, that HEAD rejects each graph fixture with `UnsupportedFeature(4)`, that the controls open, and that the smuggled op 12 is rejected.

**Result** (`python scripts\graph_compat_check.py run`, re-run 2026-10-23; rustc/cargo 1.99.1, Python 3.15.0, Node v24.22.1): **PASS, 348/348 probes matched**.

| Revision | Rust entry-point probes | Python (C ABI) | Node | Results |
|----------|-------------------------|----------------|------|---------|
| `81ff901` (HEAD, default features) | 111 | 9 | 3 | 117 × `UnsupportedFeature(4)`, 6 × `ReplicaDiverged` |
| `v0.3.0` | 105 (no `open_read_only*` in this tag) | 9 | 3 | 111 × `UnsupportedFeature(4)`, 6 × `ReplicaDiverged` |
| `v0.2.0` | 30 | 3 | 3 | 36 × `UnsupportedVersion(5)` |
| `v0.1.0`, `v0.1.1`, `v0.1.2` | 18 each | 3 each | 3 each | 24 × `UnsupportedVersion(5)` each |

Entry points probed (where present in the revision), each against plain, timestamps and encrypted fixtures:
- `Database::{open, open_read_only, open_encrypted, open_read_only_encrypted, open_from_bytes(None|key), discard_incomplete_tail, discard_incomplete_tail_encrypted}`;
- `maintenance::{inspect, inspect_detail, inspect_with_key, verify, verify_with_key, backup, backup_with_key, restore, restore_with_key, drill, compact, compact_at, compact_with_key, upgrade}`;
- `logops::{reencrypt, restore_to, export_log_segment}` (both key forms);
- `changes::read_log_changes`;
- `ReplicaFollower::{create, open, sync_from_log, apply, apply_file}`;
- the C ABI via the Python binding `NativeLibrary.{open, open(key), open_from_bytes}`, and the Node binding `NativeLibrary.open`.

Entry points absent in a revision are recorded per revision in the JSON output under `entry_points_absent_in_revision`; they are not counted as probes.

**Nuance:** `ReplicaFollower::apply` and `apply_file` answer `ReplicaDiverged`, not `UnsupportedFeature(4)`. The follower's own (valid) file is opened first, and the graph-flagged source header is rejected by the lineage/header comparison before any frame is replayed. This is an explicit rejection that applies no data, so it satisfies the rule. The script expects exactly this result for those two probes.

**Decision:** **V5 + `FLAG_GRAPH = 4`**. No HEAD-default entry point accepts the file, so no INV-001 exception is needed. Op 12 without the flag is already rejected as `Corrupt`, so a graph frame can never be silently replayed by a vector-only build.

**Reproduce:** `python scripts\graph_compat_check.py run [--revs 81ff901 v0.3.0 ...] [--skip-bindings] [--json PATH]`. The run reads nothing from the working tree except the script, the probe source `scripts\graph_compat_probe.rs` and Git objects. Fast in-tree subset: `cargo test --locked --test graph_compat`.

## DO-002 — Association of vectors to nodes

**Rule:** select attachments if a prototype attaches nodes to collection records without changing `Record`, `Mutation`, collection search code or `hybrid.rs`.

**Prototype:**
- Lives in a throwaway `git archive` copy of `81ff901` at `%TEMP%\vlite-graph-baseline\do002-81ff901\tests\do002_attachments.rs`, file SHA-256 `4703221753B55392A9EE648127D58689DD96EED1EEF55F6AF97DE12385648AFC`.
- It is never merged, and no branch was created because branch operations were not authorized. A copy outside version control is the equivalent isolation.
- It models attachments as graph-owned state keyed by `(collection id: u16, record id: u64)` plus the record revision at attach time.

The prototype enforces:
- PD-012: one node per record across the file;
- REQ-013: one attachment per (node, collection), the record must exist, and it must have no expiry.

It shows:
- `search`, `CollectionRef::search` and `search_hybrid` results map to nodes after the unchanged search returns (pivot mapping);
- staleness is detected through existing revisions (`Live` / `Changed` / `Missing`);
- detach works;
- keys are stable across close/reopen;
- a dropped collection is detectably dangling. LEG-008 makes this unreachable in the real design.

**Result:**
- `cargo test --offline --locked --test do002_attachments` passes (1 test).
- The SHA-256 of every file under the copy's `src\` was compared before and after: 0 changes. So `Record`, `Mutation`, `collection.rs`, the search code and `hybrid.rs` are untouched.

**Decision:** **Attachments (PD-012)**. Persisting `Attach`/`Detach` belongs to DO-003 and was not prototyped. Enforcing LEG-001..LEG-008 belongs to EPIC-004.

## DO-003 — Graph frame byte layout and op tag

**Op tag:** the current decoder rejects every op value 12..=255 (`Corrupt: invalid operation or reserved record bytes`; proven by `v5_control_op12_without_flag.hex`), and 1..=11 are in use. **12 is the lowest unused tag.** This part is decided.

All integers below are little-endian.

**Frame (existing layout, unchanged).** Prefix (36 bytes, plaintext even in encrypted files):

| Offset | Size | Field | Graph frame value |
|--------|------|-------|-------------------|
| 0 | 4 | magic `VREC` | — |
| 4 | 1 | op | `12` |
| 5 | 2 | collection | `0` (SEC-001: no graph identifier in plaintext) |
| 7 | 1 | reserved | `0` |
| 8 | 8 | sequence | commit sequence (= revision of every entry) |
| 16 | 8 | id | `0` |
| 24 | 4 | count | number of entries |
| 28 | 4 | payload_len | body length (as op 3 batch frames) |
| 32 | 4 | prefix CRC32 | existing |

The body follows (sealed with the existing AEAD in encrypted files: 12-byte nonce + ciphertext + 16-byte tag), then the footer `VCMT` + CRC32 u32 + sequence u64.

**Body (tested):**

| Offset | Size | Field |
|--------|------|-------|
| 0 | 1 | body_version = `1` |
| 1 | 1 | reserved = `0` |
| 2 | 2 | graph_id (u16, < PD-002 = 64) |
| 4 | … | `count` entries |

**Entry envelope (tested):** `tag u8 | flags u8 = 0 | reserved u16 = 0 | payload_len u32 | payload`. Unknown tags, non-zero flags or reserved bytes, a trailing remainder, or a count mismatch are corruption.

**Entry tags.** "Tested" means encoded by `tests\graph_compat.rs`, cross-checked against the Python encoder, and also produced byte-for-byte by the crate codec. "Unit-tested" means implemented in `crates\vlite-graph\src\codec.rs` and covered by its unit tests (golden bytes, round trip, every truncation, trailing bytes, field validation, maximum sizes); the runtime does not write or read these entries yet. "Metadata" below is the existing record metadata encoding from `src\metadata.rs`, re-implemented byte-identically in `crates\vlite-graph\src\properties.rs`; it is always preceded by `len u32` with `4 <= len <= 65,536` (4 = the `count` field alone, i.e. no fields), at most 64 fields (the `count` field is `u32` but values above 64 are rejected), sorted unique field names of 1..=256 bytes. Its layout (little-endian):

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | `count u32` (≤ 64) |
| 4 | — | `count` fields in strictly ascending byte order of the name, each: |
| +0 | 2 | `key_len u16` (1..=256) |
| +2 | `key_len` | key (UTF-8) |
| +2+`key_len` | 1 | `type u8` |
| +3+`key_len` | value | `1` String: `len u32` + UTF-8 bytes (≤ 4,096); `2` Integer: `i64` (8); `3` Float: `f64` (8); `4` Boolean: `u8` 0 or 1 (1); `5` GeoPoint: `latitude f64`, `longitude f64` (16) |

| Tag | Entry | Payload (field order) | Max size / validation | Status |
|-----|-------|-----------------------|-----------------------|--------|
| 1 | `CreateGraph` | `name_len u8`, `name` (UTF-8) | name follows the collection-name rule `^[a-z][a-z0-9_-]{0,62}$` (REQ-005), graph_id in body header | Tested |
| 2 | `DropGraph` | empty | graph must exist and be empty | Unit-tested |
| 3 | `GraphHighWater` | `next_node_id u64`, `next_edge_id u64` | monotonic; never decreases | Unit-tested |
| 4 | `PutNode` | `node_id u64`, `key_len u16`, `key`, `label_count u8`, labels × (`len u8`, bytes), `metadata_len u32`, metadata | key 1..=256 UTF-8 bytes (PD-001), ≤ 8 labels of 1..=63 bytes `[A-Za-z0-9_]` (PD-003), strictly ascending, 64 fields / 64 KiB | Unit-tested |
| 5 | `PatchNode` | `node_id u64`, `add_count u8` + labels, `remove_count u8` + labels, `set_len u32` + metadata, `unset_count u8` + field names × (`len u16`, bytes) | add/remove ≤ 8 each, disjoint; unset ≤ 64, strictly ascending, disjoint from set; tag-4 limits hold after applying | Unit-tested |
| 6 | `DeleteNode` | `node_id u64` | no incident edges (REQ-012) | Unit-tested |
| 7 | `PutEdge` | `edge_id u64`, `source u64`, `destination u64`, `type_len u8`, `type`, `key_len u16` (0 = no key), `key`, `metadata_len u32`, metadata | type 1..=63 `[A-Za-z0-9_]`, key ≤ 256 UTF-8 bytes | Unit-tested |
| 8 | `PatchEdge` | `edge_id u64`, `set_len u32` + metadata, `unset_count u8` + field names × (`len u16`, bytes) | as tag 5 | Unit-tested |
| 9 | `DeleteEdge` | `edge_id u64` | — | Unit-tested |
| 10 | `Attach` | `node_id u64`, `collection u16`, `reserved u16 = 0`, `record_id u64` | PD-012 / REQ-013 | Unit-tested |
| 11 | `Detach` | `node_id u64`, `collection u16`, `reserved u16 = 0` | — | Unit-tested |
| 12 | `PutSchema` (P2) | reserved; layout deferred to P2 | rejected as an unknown tag | Reserved |
| 64 | `VectorMutation` | `collection u16`, `reserved u16 = 0`, then exactly one `encode_batch_entries` entry verbatim (Put: `[1,0,0,0] id u64 payload_len u32 f32s payload`; Delete: `[2,0,0,0] id u64 0u32`; PutWithMetadata: `[4,0,0,0] id u64 body_len u32` + `extended_body(vector, payload, metadata)`) | REQ-010: batch ≤ `MAX_BATCH_OPERATIONS` (4,096) entries and `MAX_BATCH_BYTES` (16 MiB); the crate checks only the entry structure, the adapter checks content | Tested |

**Change from the draft (needs review):** field names in the `unset` lists of tags 5 and 8 use `len u16`, not `len u8`, because field names may be up to 256 bytes (same as metadata keys). No other draft row changed.

**Further rules implemented by the crate codec:** a body has 1..=4,096 entries (an empty batch is never written, as `write_batch_in` returns early for empty batches) and at most 16 MiB; `count` from the frame prefix must equal the decoded entry count; tags outside {1..=11, 64}, non-zero entry flags or reserved bytes, and graph IDs ≥ 64 are corruption; lists must be canonical so `encode(decode(bytes)) == bytes`.

**Status:**
- Decided: the op tag, the prefix use, the body header, the entry envelope, and tags 1 and 64.
- Proposed final, pending GATE-003 and EPIC-003 code review: the payloads for tags 2..11 and the metadata encoding reuse, all now implemented and unit-tested in `crates\vlite-graph`.
- v1.2.6: the engine source now lives in `src\property_graph\engine\` (see "EPIC-003 storage integration" below). With `--features graph`, `vlite` writes op-12 frames for tags 1 and 2 and replays tags 1..=9; tags 10, 11 and 64 are refused at replay until EPIC-004 adds the attachment and vector-entry adapter (ITEM-011 `frame.rs`). The crate is still not a dependency of `vlite`.
- v1.2.7: replay accepts tags 10, 11 and 64 through `src\property_graph\frame.rs` (see "EPIC-003 adapter completion" below). Writers still emit only tags 1 and 2 until EPIC-004.

## DO-004 — Hook inventory confirmation

**Method:**
- Grepped every `pub fn` in `81ff901` `src\{lib, collection, maintenance, logops, replica, import, changes, ann_snapshot, disk_ann}.rs`.
- Kept the mutating, deleting, expiring, importing, dropping, maintenance and open/recovery entry points.
- Compared them with LEG-001..LEG-020.

Read-only functions (search, get, inspect reports, statuses) are excluded. CLI binaries (`vlite-maintain`, `vlite-import`, `vlite-shell`, `vlite-smoke`, `vlite-tune`, `vlite-quality`) and the server routes call these library functions and inherit their behavior.

**Already covered by LEG-001..LEG-020:**
- `put*`, `update_metadata`, `delete*`, `write_batch*` and their collection equivalents;
- `import_chunk` / `finish_import` / `abort_import` (both `Database` and collection forms);
- `drop_collection`;
- `compact*`, `upgrade`, `backup*`, `restore*`, `drill`, `inspect*`, `verify*`, `reencrypt`, `restore_to`;
- `changes_after`, `feed_cursor`, `read_log_changes`;
- `export_log` and `ReplicaFollower::{create, apply, apply_file, sync_from_log, promote}`;
- HNSW build/rebuild/maintain, ANN snapshot save/load and disk-ANN (LEG-015);
- query language;
- server, gRPC and webhooks;
- binding open paths.

**Not in the table:** proposed additions, to be folded into PRD Section 3.4 before EPIC-004 starts.

| ID | Entry point (81ff901) | Vector-only file / feature off | Graph-enabled file |
|----|-----------------------|-------------------------------|--------------------|
| LEG-021 | `Database::open_from_bytes`, `create_in_memory`, `to_bytes` (`src\lib.rs` 899-913) | Unchanged | Same replay and header-flag rules as `open`; `to_bytes` must keep `FLAG_GRAPH` (same rule as Section 3.3 item 6) |
| LEG-022 | `Database::discard_incomplete_tail`, `discard_incomplete_tail_encrypted` (`src\lib.rs` 1073-1077) and the automatic tail discard in `ReplicaFollower::open` (`src\replica.rs` 89) | Unchanged | A torn graph frame is an incomplete tail and is discarded exactly like a torn vector frame; the header (and `FLAG_GRAPH`) is never rewritten |
| LEG-023 | `logops::export_log_segment`, `LogSegment::{save, load, to_bytes, from_bytes}` (`src\logops.rs` 236-370) | Unchanged | Segments carry graph frames verbatim; applying them is governed by LEG-014 |
| LEG-024 | `create_collection`, `set_collection_settings` (`src\lib.rs` 1761, 1832) | Unchanged | Unchanged (collections are not graph objects); collection ids stay stable, so attachment keys stay valid (DO-002) |
| LEG-025 | `set_commit_clock`, `set_wasm_clock_ms` (`src\lib.rs` 1594, 100) | Unchanged | Unchanged; graph entries carry no time fields (REQ-014) |
| LEG-026 | `Database::close`, `FeedCursor::{save, load}` (`src\lib.rs` 2522; `src\changes.rs` 74-98) | Unchanged | `close` unchanged; feed cursors cannot be obtained on graph-enabled files (LEG-013) |

`ReplicaFollower::{put, delete, write_batch}` always return `ReadOnlyReplica` at `81ff901`, so they need no hook.

**Decision:** LEG table + LEG-021..LEG-026. Five of the six additions change no behavior. LEG-021 and LEG-022 are hooks that EPIC-003 must honor (flag preservation, torn-tail handling).

## DO-005 — Baseline capture

**Status: Recorded (v1.2.7).** The reference run is recorded in [`baseline.md`](baseline.md) (FILE-024): clean `81ff901`, default features, 5 runs per workload, seeded 100,000 × 384 dataset with 10 queries per run plus the existing CI `vlite-bench` workloads, REG-013 `pass`. Medians: exact ingest 62,489 vectors/s, p50/p99 9.968/12.080 ms, reopen 1,131.702 ms, RSS 329.0 MiB; filtered ingest 56,274 vectors/s, p50/p99 4.762/8.039 ms, reopen 1,179.414 ms, RSS 384.7 MiB; HNSW ingest 60,903 vectors/s, build 930,819.669 ms (about 15.5 minutes), HNSW query p50/p99 3.072/3.630 ms, recall 0.26.
- Reference machine: Windows 11 build 26300, Xeon Platinum 8370C, 16 logical CPUs, 64 GiB RAM, rustc/cargo 1.99.1, Python 3.15, Node 24.22.
- Storage: `Q:` is a ReFS Dev Drive on a Windows-reported virtual SAS disk (`Msft Virtual Disk`) with unknown physical backing. **These storage details were queried after the measurement; the JSON records storage as "unrecorded".** The current script requires `--storage-description` for reference runs.
- 10 queries, not 1,000: a 1,000-query attempt exceeded `vlite-bench`'s 500-million distance-component limit for exact search.
- Not decided here: the REG-010 budgets (PD-014, GATE-004) and whether to re-pin to `62e2902` or later (baseline validity note below).

The earlier pending state and the smoke evidence are kept below for traceability.

**Tooling delivered:**
- `scripts\graph_baseline.py measure --rev <sha> --profile smoke|reference --runs N [--reference-machine-confirmed] [--output PATH]`.
- It builds the exact revision from `git archive` into an isolated target directory and runs the existing `vlite-bench` workloads.
- It records:
  - put, exact, HNSW and filtered search medians and p99;
  - `reopen_ms`, as reported by `vlite-bench`;
  - peak RSS through the existing `tests\measure_benchmark.py` (`PeakWorkingSetSize` on Windows, `getrusage` `ru_maxrss` on Linux/macOS);
  - release binary sizes for `vlite-bench` and `vlite-ffi`;
  - REG-013 (pinned artifact features versus the `Cargo.toml` feature set).
- `--profile reference` (100,000 × 384, seeded, 5 runs) refuses to run without `--reference-machine-confirmed` (exit 2).

**Smoke evidence** (not a baseline): `python scripts\graph_baseline.py measure --rev 81ff901 --profile smoke --runs 1` succeeded and REG-013 passed.

| Item | Value |
|------|-------|
| Machine | Windows 11 build 26300, Intel Family 6 Model 106, 16 logical CPUs, 64 GiB RAM; storage not recorded |
| Toolchain | rustc 1.99.1, cargo 1.99.1, Python 3.15.0 |
| Release binary sizes at `81ff901` | `vlite-bench` 1,146,368 bytes; `vlite-ffi` 2,830,336 bytes |

**Missing before DO-005 could be recorded (both resolved in v1.2.7):**
1. User confirmation that this machine (or another) is the reference machine for REG-010, NFR-002 and NFR-003, including its storage device.
2. Then run `python scripts\graph_baseline.py measure --rev 81ff901 --profile reference --runs 5 --reference-machine-confirmed --output %TEMP%\vlite-graph-baseline\reference-81ff901.json`, then record the machine, toolchain, commands and results in `docs\projects\feature-agent-graph\baseline.md` (FILE-024) and mark DO-005 decided here.

**Limitations to record with the baseline:**
- RSS is the Windows peak working set on Windows and `ru_maxrss` elsewhere, so numbers are comparable only on the same OS.
- `reopen_ms` measures `vlite-bench`'s reopen of its own file.
- Medians are taken over runs, not over requests inside a run.

## DO-006 — Identity model

**Status: PD-001 chosen under delegation; formal GATE-002 sign-off still to be recorded.**
- Chosen: PD-001, a caller-supplied string key (UTF-8, 1..=256 bytes, unique per graph) mapped to an internal monotonic `u64` node ID that is never reused. The workflow chose it on the user's instruction to proceed without stopping; it is implemented in `crates\vlite-graph`.
- Implemented rules: node keys and edge keys are separate namespaces, each unique per graph. A deleted key can be reused, but its ID cannot. New IDs must be at least the graph's high-water mark (gaps are allowed so a compaction rewrite can keep old IDs), and the high-water mark becomes `id + 1`. `u64::MAX` is never assigned. `PutNode` on an existing ID must use the same key.
- Rejected alternative: caller-supplied `u64` node IDs with the same uniqueness rules.
- Record identity is decided independently by DO-002: `(collection id u16, record id u64)`. It does not depend on this choice.

## EPIC-003 storage integration (v1.2.6, uncommitted, pending review)

Recorded under the user's delegation. These are engineering decisions, not GATE approvals.

**Packaging (supersedes PRD Section 3.2.1 rule 5's dependency plan).**
- The pure engine source moved from `crates\vlite-graph\src\` to `src\property_graph\engine\`, which the root package already ships (`src/**`).
- `crates\vlite-graph\src\lib.rs` is now a thin harness: `#[path = "../../../src/property_graph/engine/mod.rs"] mod engine; pub use engine::*;`. The crate stays `publish = false`, workspace-only and dependency-free. `cargo test --locked -p vlite-graph` still builds and tests the engine in isolation.
- `vlite` declares `graph = []`. This is not a crate dependency, and `graph` is not in `default` or in any artifact feature pin (REG-013).
- The engine uses only module-relative paths (`super::`), never `crate::`, and no `unsafe`, `vlite` types or I/O. `tests\graph_build_boundary.py` enforces this, plus:
  - the crate source is only the harness;
  - there is no `vlite-graph` entry in any `vlite` dependency table;
  - `cargo package --list -p vlite` ships every engine file.
- Result: `cargo package --locked -p vlite` passes both with and without `--features graph`, and nothing has to be published. The isolated crate, the `vlite` feature build and the `graph_storage` test target all compile the same files, so their engine unit tests are identical by construction.

**Graph IDs (`GraphId`, body header) can be reused.**
- The engine no longer keeps a graph-ID high-water mark. `CreateGraph` requires an ID below 64 that is not live, and writers use the lowest free ID.
- Dropping a graph needs it to be empty, which leaves no nodes, edges or attachments. Per-graph node and edge high-water marks live only inside the graph, and the public API names graphs, never IDs. So reusing an ID cannot alias data, and creation is not limited to 64 over the file's lifetime.
- The limit is 64 live graphs (PD-002). An atomic `DropGraph` + `CreateGraph` of the same ID in one body is valid.

**Storage integration (`--features graph`).**
- `FLAG_GRAPH = 4` and `OP_GRAPH = 12` are defined in `src\format.rs` only under the feature.
- Only the graph-aware loader (`format::load_database`, used by `Database` open, read-only open, `open_from_bytes` and tail repair) accepts the flag. It accepts op 12 only in V5 files whose header has the flag.
- Every other reader still rejects graph files with `UnsupportedFeature(4)`, even in graph builds, until hooks H-5/H-6 make them graph-aware: maintenance, change feed, log export and `ReplicaFollower`, whose open checks the header before any repair.
- Frame prefix:
  - `collection` and `reserved` are 0, `id` is 0;
  - `1 <= count <= 4,096`;
  - `payload_len` is between the body length implied by `count` (`4 + 8·count`) and 16 MiB.
- Replay order: the prefix CRC, body CRC and sequence are checked, encrypted bodies are opened, and the body is then decoded completely and applied by the atomic `GraphState::apply` (validate first, roll back on error). Any failure fails the whole open with `Corrupt`; no partial graph state is ever published.
- Writes use the existing commit path: sequence, lock, CRC, sealing, timestamp anchors and `durable_group`.
- `create_with_graph` creates a V5 file with `FLAG_GRAPH`. An explicit V3 or V4 request returns `GraphsRequireV5`.
- Public API:
  - `create_graph`, `drop_graph`, `list_graphs` (name order) and `is_graph_enabled` exist.
  - Ordinary files return `GraphsNotEnabled`.
  - Read-only handles refuse writes.
  - In-memory databases cannot be graph-enabled (WASM is out of scope).
- Not exposed in v1.2.6: `graph`/`graph_mut` handles (added in v1.2.7) and node/edge mutations (EPIC-004).

## EPIC-003 adapter completion (v1.2.7, uncommitted, pending review)

Engineering decisions under the user's delegation; no gate is approved.

**Vector and attachment entries at replay (ITEM-011).**
- A `VectorMutation` entry carries exactly one canonical batch entry (op 1, 2 or 4) from `format::encode_batch_entries`. The engine envelope check refuses import markers (op 8) and reserved bytes. The adapter decodes the entry with `format::decode_batch` using the target collection's dimensions, so vector bytes and rules are identical to op-3 batches. Unknown collections, dimension mismatches and non-finite components are `Corrupt` at the frame offset.
- Order: decode every entry first, then apply the graph entries atomically (`GraphState::apply`), then apply the vector entries in body order at the frame sequence (record revision = frame sequence, expiry cleared as with op-3 puts).
- Attachment integrity is checked on the body's final state. Every attachment the body touches (an `Attach` entry, or an attached record changed by a vector entry) must reference a live record without expiry. Otherwise the open fails with `Corrupt` ("graph attachment references a missing or expiring record"). A record created earlier in the same body may be attached. An attached record can be deleted only after a `Detach` in the same or an earlier body.
- Ordinary op-2/3/9 frames that delete or expire an attached record were not checked at replay in v1.2.7. EPIC-004 (v1.2.9) closed this: replay of such a frame fails the open with `Corrupt` ("graph attachment references a missing or expiring record"), in addition to the H-4 write hooks that prevent a writer from producing it.

**Handles, conversions and errors (ITEM-012/013).**
- `Database::graph(name) -> GraphRef` and `graph_mut(name) -> GraphMut` (`name`, `info`). `graph_mut` returns `ReadOnly` on read-only handles before the name lookup.
- Engine mirror types: `PropertyCondition` (mirrors `Condition`) and `Precondition` (mirrors `Expected`). `src\property_graph\convert.rs` validates with `metadata::validate`/`validate_filter` before converting, so non-finite floats never reach the engine. Conversions are total and lossless, and property encodings are byte-identical.
- The `GraphError` → `Error` mapping is exhaustive: lifecycle errors map exactly, `BatchSize` to `BatchTooLarge`, `InvalidProperties` to `InvalidMetadata`, and the rest to `InvalidQuery` with a fixed message until EPIC-004 adds the FILE-002 variants.

**Unresolved (GPEND):**
- GATE-002 and GATE-003 sign-off (DO-003 tags 2..11, DO-006).
- GATE-008 release bundle and graph artifact feature.
- H-5/H-6: graph-aware maintenance, change feed, log export and replication, including preserving `FLAG_GRAPH` on copies (TEST-008).

## Baseline re-pin (v1.2.10)

Decision made autonomously under the user's delegation (the user is unavailable and asked for good decisions); the user should review it, and it approves no gate.

- **Why.** The earlier note below the fixtures said the working tree contained other contributors' engine changes that REG-001 and REG-008 flagged, and that re-pinning after they landed was a user decision. They landed as `04014fc` (optimization-only) and `347b01d` ("Harden durability diagnostics and bounded maintenance behavior"), both on `origin/main`. The peer session that owns them asked for the graph baselines to be re-pinned against `347b01d`. Treating published, reviewed, test-passing upstream commits as regressions would make every graph check fail until they are reverted, which is not the intent of the baseline.
- **What changed.** `python scripts\graph_baseline.py fixtures --rev 347b01d` and `python scripts\api_surface.py baseline --rev 347b01d` regenerated `tests\fixtures\graph_baseline\` and `tests\fixtures\api_surface\vlite-default.txt`. Script defaults, test headers and docs now name `347b01d`.
- **Review of the regenerated files against the `81ff901` set.**
  - `vlite-default.txt`: six added items and no removals (`MAX_HNSW_RETAINED_RECORD_BYTES`, `records::{MAX_READ_SNAPSHOTS, MAX_READ_SNAPSHOT_BYTES, MAX_READ_SNAPSHOT_RECORDS, ReadSnapshot, SnapshotRetentionStats}`).
  - `openapi.json`: additions only. They are the `x-durable-group-commit` block, the `AutoMaintenancePolicy` retry-backoff fields and description, and the `MaintenanceStatus.rebuild.phase` enum.
  - V4/V5 `.vlite.hex` files differ only in the random lineage, header CRC and encrypted salt/nonce/ciphertext bytes that REG-003 masks.
  - MCP transcripts and the V3 file are unchanged.
- **Consequences.**
  - `APPROVED_OPENAPI_ADDITIONS` in `tests\graph_surface_parity.rs` is now empty because the golden already contains the block it used to allow. The allowlist mechanism is kept (its two tests use a test-local synthetic addition) for future reviewed additions.
  - `scripts\api_surface.py check --features graph` now implements the C-2 allowance: additions under `vlite::property_graph::` are reported as ALLOWED and any other addition or any removal fails.
  - The DO-005 performance record stays at `81ff901`. Re-measure at `347b01d` before REG-010 is enforced or GATE-008 is requested (the reference profile takes hours; run it on an otherwise idle machine).
  - `scripts\graph_compat_check.py` keeps `81ff901` and the release tags in `DEFAULT_REVS` because that is the DO-001/DO-002 evidence; its recorded results are not regenerated.
- **Later upstream commit.** `5227fad` ("Sample rebuild worker completion once per status observation") changes only `src\rebuild.rs` (a local variable so `phase` and `worker_finished` agree). It changes no public item, OpenAPI field or fixture, so the pin stays at `347b01d`. `api_surface.py check` (default and `--features graph`) and the `graph_regression`, `graph_compat`, `graph_surface_parity` and `graph_integrity` suites pass on that tree.
- **Going forward.** Every upstream commit that changes the public item list or the served OpenAPI needs the same two commands, run from a clean checkout of that commit, plus a review of the diff. The graph work still must not edit those upstream files except for the `cfg(feature = "graph")` hunks recorded in the PRD.

## EPIC-004 mutations and integrity hooks (v1.2.9, uncommitted, pending review)

These are engineering decisions made autonomously under the user's delegation. No gate is approved.

- **Location.** The planner lives in `src/property_graph/engine/mutate.rs`, the shared engine source, not in `crates/vlite-graph/src/mutate.rs`. The harness compiles the same file.
- **Planning.** `GraphState::planner(sequence, graph)` applies each mutation tentatively and rolls back on drop. Later operations in a batch see earlier ones, and a failed batch leaves the state untouched. The adapter then commits through the existing validate → append → apply path. Vector operations reuse `validate_batch` and are applied after the graph state, under the same sequence.
- **Attach liveness.** An attach requires a live record without expiry, taking earlier vector operations in the same batch into account. Otherwise it returns `GraphRecordNotAttachable`. A vector delete of a record that is attached at that point of the batch returns `GraphAttachedRecord`.
- **Edge keys.** Creating an edge with a live key succeeds without writing when the edge is identical: same endpoints, type and bit-identical properties. It returns `created = false` and the existing revision. Any other reuse of the key returns `GraphKeyConflict`. A batch that writes no entries returns `revision: None`.
- **Preconditions.** Preconditions are checked before existence. A failure maps to the existing `RevisionMismatch { current }` instead of a new graph-specific error.
- **Import.** The LEG-007 check runs after the idempotency and conflict checks, so an already-committed chunk is still `AlreadyCommitted`. It runs before the `RejectChunk` invalid-row outcome, so an attached delete is an error under either policy.
- **`graph_sequence`.** It is raised to the commit sequence (as a maximum) for the owner of an attached record changed by `apply` (put, batch put, import) or by a metadata update, and by that record's delete or expiry, which the H-4 hooks reject while it is attached. Replay does the same in `format.rs`. Named and sparse vector writes (`vectors.rs`) deliberately do not raise it. This is a decision for user review, not a known gap: those writes replace a collection's side-vector data in one vector frame and cannot be attributed to individual records without diffing the old and new data, and graph read results carry no vectors by default (PRD Section 3.6) while evidence items never do (REQ-042), so they cannot change any default traversal result or evidence field (record ID, revision, metadata, payload). If a later epic adds an explicit vector projection to a core read API, this decision must be revisited. The PRD Section 3.5 wording now says "the base record".
- **New public errors.** These variants go beyond the FILE-002 list: `GraphEdgeNotFound`, `GraphAttachmentConflict`, `GraphAttachmentNotFound` and `GraphRecordNotAttachable`. They are `cfg(feature = "graph")` variants of the non-exhaustive public `Error` enum; the vector-only API surface is unchanged.
- **GPEND:**
  - TEST-005 rows LEG-009..LEG-016 and LEG-018 (EPIC-006/EPIC-007);
  - a file-backed model at the full TEST-003 scale (10,000 steps per seed, reopen every 500);
  - formal GATE-002/GATE-003 sign-off.

## EPIC-005 bounded reads (v1.2.11 engine, v1.2.12 public surface; uncommitted, pending review)

These are engineering decisions made autonomously under the user's delegation. No gate is approved. The engine is `src/property_graph/engine/` (`query.rs`, `traverse.rs`, `cursor.rs`, `meter.rs`, `limits.rs`, `filter.rs`), compiled unchanged by the `vlite-graph` harness. The public adapter is `src/property_graph/read.rs`.

- **One IR, one executor.** Every read, including `explain`, builds the engine's `GraphQuery` IR and runs `engine::execute`. `explain` validates and plans exactly like the read, then returns the normalized query, effective limits, projection and index choices. It examines no node or edge, and its stats counters stay at zero.
- **Public shape.** The `GraphRef` methods are `view_sequence`, `query`, `explain`, `get_node`, `get_node_by_id`, `get_edge`, `get_edge_by_key`, `find_nodes`, `neighbors`, `traverse` and `shortest_path`.
  - Each read returns `GraphRead<T> { output, complete, truncation, view_sequence, stats, cursor }`.
  - Requests are plain structs with public fields (`FindNodes`, `Neighbors`, `Traverse`, `ShortestPath`). Per-call `ReadOptions { limits, projection, cancel }` carries the limits, projection and cancel flag.
  - Output, query and error-adjacent enums are `#[non_exhaustive]`.
  - Inputs use the crate's `Condition` and `Metadata`, and results never carry vectors.
  - Everything is exported under `vlite::property_graph::` only (the C-2 allowance). `GraphLimits::validate` is crate-private, so the public `GraphLimits` exposes no engine error type.
- **Conditions.** Every condition list is checked with `metadata::validate_filter` before conversion, so invalid conditions fail exactly like record filters, and matching is identical to `metadata::matches`. Randomized adapter tests compare the two directly and through `find_nodes`/`neighbors` on a file.
- **`view_sequence`.** It is the database commit sequence when the `GraphRef` was obtained. A `GraphRef` borrows the `Database`, so no commit can happen while it lives. A read-only handle therefore reports its open-time sequence for its whole lifetime, and unrelated vector writes advance it.
- **Limits.** Validation runs before anything else.
  - A value above `GraphLimits::CEILING` fails with `GraphLimitExceedsCeiling`, naming the first offending field in declaration order. Limits are never clamped.
  - `max_hops` and the output caps may be zero. The work budgets (`max_visited_nodes`, `max_edges_examined`, `max_edges_per_node`, `deadline_ms`) must be at least 1, or the read fails with `InvalidQuery`.
  - A paged read's `limit` (default: the matching `max_output_*` cap) must be between 1 and that cap's ceiling.
- **Failure precedence.** Public entry points check limits before converting query inputs. The engine then validates query structure (labels, types, conditions, projection), the cursor, and node resolution. Work limits, deadlines and cancellation produce truncation reasons; an internal invariant violation remains an explicit error. The adapter regression test combines invalid filters/selectors with invalid limits to preserve this ordering.
- **Scan budget (PD-006).** `find_nodes` charges one unit of `max_edges_examined` per scanned candidate node. `neighbors`, `traverse` and `shortest_path` charge one unit per adjacency entry examined. When `find_nodes` runs out of budget, it returns `EdgeLimit` with a cursor positioned after the last scanned candidate, so paging can always continue.
- **Cursors.** The v1 layout is version, kind, graph ID, graph sequence, query fingerprint, position, then a checksum.
  - The fingerprint is FNV-1a 64 over the normalized query, including page size and projection. The checksum is FNV-1a over the preceding bytes; it detects accidents and is not a MAC.
  - Decoding checks structure, checksum, kind, graph and fingerprint. Any failure is `InvalidQuery`.
  - A graph sequence that differs from the current one then fails with `GraphCursorStale`. A cursor survives reopen and unrelated vector writes, but not writes to the graph or to an attached record's base record.
- **Deadline and cancellation (PD-008, PD-010).**
  - Both are checked once before any work and then at least every 64 budget units (`CHECK_INTERVAL`).
  - A stop before any work returns empty output, and a paged read's cursor repeats the page.
  - The adapter uses a monotonic `Instant` clock (the host clock on wasm32), and the engine tolerates a clock that goes backwards.
  - Deadline expiry is tested with an injected clock in the engine tests. Cancellation is tested through the public API.
- **Traversal.**
  - The search is FIFO breadth-first. Adjacency is ordered by (relationship type, edge ID), `Both` merges both directions, and a self-loop appears once.
  - Nodes at depth `max_hops`, and nodes failing `expand_filter`, are visited but not expanded.
  - Every edge that passes the edge filter and is examined from an expanded node is returned once, in first-examined order. This includes edges to already-visited nodes and self-loops.
  - Returned nodes are the discovered nodes that pass `return_filter`, in discovery order, each with its minimum depth.
- **Canonical paths.** Among the shortest paths from the start set, a read returns the lexicographically smallest edge-ID sequence. It is computed exactly over the BFS layer DAG, which is cheap because the hop ceiling is 4.
  - Intermediate nodes of traversal paths must be expandable.
  - A traversal path is returned only if all of its edges are among the kept (output-capped) edges. Otherwise the read reports `OutputLimit`.
  - `shortest_path` takes only direction and types. When source equals destination, it returns `Found` with no edges.
- **Certification.** Any truncation during `shortest_path` yields `Exhausted { truncation }`, so `Found` and `NoPathWithinHops` are always certified. The destination counts towards `max_visited_nodes`.
- **Truncation.** Reasons are deduplicated and listed in declaration order (`VisitedLimit`, `EdgeLimit`, `DegreeLimit { node_ids }` with at most 64 IDs, `OutputLimit`, `Deadline`, `Cancelled`). `complete == truncation.is_empty()`. Output caps never stop exploration.
- **Evidence.** The adapter tests in `src/property_graph/read/tests.rs` cover condition parity, invalid-filter parity and `view_sequence`. `tests/graph_traversal.rs` compares the public reads with the independent BFS model in `tests/graph_model/`, and also covers truncation soundness, the 100,000-edge hub, cursor staleness and reopen, cancellation and ceilings. `examples/graph_bench.rs`, the allocation-counting `examples/graph_memory.rs` and the runner described below produced the accepted five-repetition measurement in [`graph-measurements.md`](graph-measurements.md) and `measurements/epic005-reference-0.4.1.json`. Medians: replay 3.52 s per million edges, `neighbors` p99 69.5 µs, `traverse` p99 677.2 µs, and 1,015,992,601 retained allocator bytes (about 1,016 B per edge, 15.5 times the log, edge records 75% of it). The host is a virtual machine that is never idle (see the protocol). The cost per edge is a review flag for GATE-004, not a decision. Hybrid-retrieve latency and property-index memory remain with ITEM-039 and EPIC-008.

### Reference collection protocol

`scripts/graph_reference.py` records five latency/allocation repetitions using
prebuilt release examples, with database files explicitly on the DO-005 `Q:`
ReFS volume. It checks the recorded CPU, logical CPU count, RAM, OS build and
Windows-reported disk identity. No process is stopped or reprioritized.

The DO-005 host is a virtual machine whose security and management agents are
always running, so it is never idle. A passive three-minute survey with the
runner's own clock (360 half-second samples) measured a 4.99% mean background
load across the 16 logical CPUs, 17% of samples above 10% and a worst sample of
45.1%. Only 13% of five-second windows, and none of the 50- or 100-second
windows, met the first draft's 2% mean / 10% sample criterion, so that
criterion could never accept a benchmark. It is replaced by a gate calibrated
to the measured floor: a five-second preflight and each benchmark must have mean
background CPU at most 10% of the 16-logical-CPU machine (twice the floor), with
no half-second sample over 50% (just above the worst surveyed sample). A
preflight that fails is repeated for up to twelve probes, and every rejected
probe is recorded in the report.

Background CPU is Windows guest system busy CPU minus the benchmark process's
CPU, conservatively including supporting OS work. During preparation the
contributors included Microsoft Defender scanning, the Global Secure Access
client, authentication helpers, WMI and the application's WebView. The gate
catches builds, test runs and similar competing work. It is a collection-quality
check, not an idle-machine claim and not a new graph performance guarantee. Because
it is looser than the first draft, the published figures are medians with their
five-run ranges and each run's measured background load, never a single best
run. A failed check during a benchmark preserves the raw attempt and rejects the
whole collection; it is not silently counted among the five repetitions. This
cannot establish that the backing hypervisor or physical disk is idle.

The report records the base commit and dirty-worktree status, SHA-256 hashes of
the source inputs and executables, toolchain, complete stdout/stderr and CPU
samples. It does not claim that unpublished graph changes are part of the base
commit. Timing and allocator-instrumented measurements use separate executables.
NFR-002 metrics follow implementation order: nonexistent property indexes have
no allocated component yet (EPIC-008), and hybrid-retrieve latency remains an
explicit ITEM-039 measurement after EPIC-010 supplies that operation.

### Memory attribution method

`examples/graph_memory.rs` splits the retained memory of the replayed graph
state by component without estimating anything. A counting global allocator
(`examples/graph_support/counting_alloc.rs`) tracks live requested heap bytes
and blocks for the whole process. It is compiled only into that example, so the
latency executable is never instrumented. `GraphState::release_components` takes
the state by value and drops its owned components one at a time in a documented
order (`MemoryComponent`: the name index and attachment owners, then for each
graph its table, name, node records, edge records, attachments, node and edge
key indexes, out and in adjacency, and label index). It calls an observer after
each drop, and the bytes freed since the previous call belong to the component
just reported. Nothing is cloned or sized by formula. The method allocates
nothing, and the observer must not allocate or free.

The example exits non-zero unless the components add up to the retained total in
bytes and blocks, no drop increased the live count, the live count returns to
its value before the replay, and every measured read returns it to where it
started. It also fails if the file's frames differ from the frames the shared
engine source plans, so the attributed state is the state a replay of the file
builds. Opening the file read-only retains 7,391 B (6 blocks) more than that
state, a 0.001% difference that serves as a cross-check. The unit is requested
allocator bytes, not resident set size. The counters are process-wide, so every
window runs on the main thread. The example compiles its own copy of the engine
source through `#[path]`; the extra CI test time is recorded in the PRD (v1.2.14
"Wiring").

## EPIC-006 maintenance, recovery, change feeds and replicas (v1.2.15, on `main` after `8eb5d7a`; pending review)

These are engineering decisions made autonomously under the user's delegation. No gate is approved. The new code is `src/property_graph/maintain.rs` and `frame::compacted_bodies`; the cfg-gated changes are in `src/format.rs`, `src/maintenance.rs`, `src/logops.rs`, `src/changes.rs`, `src/replica.rs`, `src/lib.rs` and `src/bin/vlite-maintain.rs`.

- **One loader.** `format.rs` no longer has a separate graph-rejecting reader. `load`, `load_with` and `read_log_header(_with)` accept `FLAG_GRAPH` in graph builds, so every caller now either carries graph state or rejects explicitly; none drops it (entry-point table below). The header reader refuses any flag outside `KNOWN_FLAGS | FLAG_GRAPH`, so `loaded.flags & !KNOWN_FLAGS` is 0 or `FLAG_GRAPH` in graph builds and always 0 in default builds. `compact*` and `reencrypt` derive the graph bit from that mask.
- **Compaction.** `compact*`, `enable_graph` and `strip_graph` all write through `maintenance::write_v5`, which now takes the extra header flags. With `FLAG_GRAPH` it writes the graph state after the vector records and import markers as op-12 frames planned by `frame::compacted_bodies`. For each graph in ID order the frames hold `CreateGraph`, then every node, edge and attachment, then `GraphHighWater` with the graph's node and edge high-water marks. Bodies are split under `MAX_BATCH_ENTRIES` and `MAX_BATCH_BYTES`. Graph, node and edge IDs are kept, and the high-water entry stops a deleted highest node or edge ID from being reused after compact and reopen. As for vector files, compaction drops commit timestamps and restarts sequences, so revisions and `graph_sequence` restart. TEST-008 therefore compares compacted state without revisions; the other operations compare it with revisions.
- **Copy-outs.** `enable_graph` and `strip_graph` are compactions published through the existing staged, verified `publish_with`, and the source is never modified.
  - `enable_graph` refuses v1–v3 sources ("run `maintenance::upgrade` first") and sources that are already graph-enabled, both with the new `GraphInvalidOptions(&'static str)`.
  - `strip_graph` checks `confirm_graph_data_loss` before opening anything (`GraphInvalidOptions`). It returns the existing `GraphsNotEnabled` for a vector-only source. It reports the removed graph, node, edge and attachment counts and keeps the attached records.
  - For AC-015 "opens in C-1", the test asserts that the output header carries no `FLAG_GRAPH`. Every loader rejects op 12 under such a header, so the output is a plain vector v5 file. No C-1 binary is run in the suite.
- **Graph counts and checks.** `inspect_graph` and `verify_graph` wrap the unchanged `Inspection` and `Verification` and add `graph_enabled` and `GraphCounts`. They also check the graph invariants and that every attached record is live without expiry (INV-008, INV-009); a violation is `Corrupt` at offset 0. `verify_graph` keeps `verify`'s refusal of an incomplete tail. `verify` now shares `verify_loaded` with it.
- **Other maintenance (LEG-011).**
  - `backup*` copies bytes.
  - `restore*` keeps the source header flags.
  - `inspect`, `inspect_detail` and `verify*` accept graph files with unchanged result types.
  - `drill` also compares the restored graph state with the source and fails with a drill error if they differ.
- **`reencrypt` and `restore_to`.** `reencrypt` keeps every header flag except encryption and reseals op-12 frames like any other frame. `restore_to` copies the source header and cuts at whole commits, so op-12 frames are kept or dropped with their commit.
- **Upgrade (LEG-010).** In graph builds `upgrade` returns `GraphUnsupportedOperation` before anything is published. The error text names `maintenance::enable_graph` and `maintenance::strip_graph`.
- **Change feeds (LEG-013, REQ-053).**
  - `changes::read_log_changes` returns `GraphChangeFeedUnsupported` right after the header is read.
  - `Database::feed_cursor` and `changes_after` return it before they touch anything.
  - Vector-only files keep their feeds.
- **Key errors come first.** The header reader checks the key before the graph checks, so the key error a vector-only file gets wins. `upgrade` takes no key, so an encrypted graph file gets `EncryptionKeyRequired`. A key mismatch on `read_log_changes` gets the key error, not the feed error.
- **Replicas (LEG-014, REQ-054).** Carry-through, not rejection.
  - In graph builds, a follower opens, creates, applies, syncs and promotes graph files and replays op-12 frames; a torn graph frame is trimmed by the existing tail repair.
  - Builds without `graph` still reject at the header with `UnsupportedFeature(4)` before any tail repair.
  - A vector-only follower in a graph build refuses a graph primary's segment with `ReplicaDiverged("segment header differs from follower")` and is unchanged, because the segment header differs from its own.
  - **`src/replica.rs` change.** PRD FILE-014 allows a cfg change to `src/replica.rs` only if TEST-008 shows a gap, and it did:
    - `replica_followers_replay_graph_frames` first failed with `UnsupportedFeature(4)`, because `ReplicaFollower::open` still rejected graph-enabled files.
    - `Database::replica_extend` in `src/lib.rs` rebuilt the follower from the replayed log without its graph state, so graph state was lost silently.
    - The fix removes the `open` rejection and assigns the replayed graph state in `replica_extend`.
  - The REST and gRPC `replication_export` routes therefore need no graph rejection.
- **CLI.** In graph builds `vlite-maintain` adds `enable-graph SOURCE DESTINATION` and `strip-graph SOURCE DESTINATION --confirm-graph-data-loss`. Like the existing commands, they take no key, so they work only on unencrypted files. The usage text is assembled with a macro: the default build's text is byte-identical, and graph builds append the two commands.
- **Changes to existing tests.** EPIC-006 deliberately reverses two EPIC-002 assertions in graph builds:
  - `tests/graph_storage.rs`: `graph_unaware_paths_keep_rejecting_graph_files` asserted `UnsupportedFeature(4)` from maintenance, feeds, replicas and log export. It is replaced by `maintenance_replicas_and_feeds_handle_graph_files_explicitly`, which asserts the EPIC-006 behaviour on the same file. Its unused `assert_graph_rejected` helper is removed.
  - `tests/graph_compat.rs`: `probe_all` keeps its 35 calls per fixture and the byte-identical source check. Default-build assertions are unchanged (REQ-055 and TEST-002 for graph-unaware builds); graph builds assert each call's EPIC-006 outcome.
- **Red/green evidence.**
  - TEST-008 (`tests/graph_maintenance.rs`) was written first. It did not compile until the public functions existed, and it then failed on five behaviours:
    - replica `open` returned `UnsupportedFeature(4)`;
    - `upgrade` was not rejected;
    - `reencrypt` returned `Corrupt { offset: 811 }` because the flag was dropped;
    - compaction output flags were 0 instead of 4, in the library and in the CLI.
  - TEST-004 (`tests/graph_recovery.rs`) passed on its first run because it was written after the loader change. It characterises crash and fault behaviour; it did not drive the change.
- **Verification (Windows, `--locked`, parent `8eb5d7a`; the PRD v1.2.15 changelog has the full run, including the all-features and default workspace totals).**
  - `cargo fmt --all -- --check` is clean.
  - The CI graph test step (`--features graph --lib --test graph_storage --test graph_compat --test graph_integrity --test graph_traversal --test graph_maintenance --test graph_recovery`) passes:

    | Suite | Passed | Ignored |
    |---|---|---|
    | lib | 179 | 2 |
    | graph_storage | 109 | 0 |
    | graph_compat | 6 | 0 |
    | graph_integrity | 13 | 0 |
    | graph_traversal | 6 | 0 |
    | graph_maintenance | 10 | 0 |
    | graph_recovery | 4 | 0 |

  - Default build:

    | Suite | Passed | Ignored |
    |---|---|---|
    | graph_regression | 3 | 0 |
    | graph_compat | 5 | 0 |
    | lib | 68 | 2 |

  - These existing suites pass in both builds: maintenance 12, change_feed 6, replica 5, pitr 6, encryption 10, format_v4 6, format_v5 3, smoke_cli 3, compatibility 7, reliability 3 plus 1 ignored, read_only 6 and database 17. graph_regression (3) also passes in the graph build.
  - `cargo clippy --locked --workspace --all-targets -- -D warnings` passes by default, with `--features graph` and with `--all-features`, including `graph_bench` and `graph_memory`.
  - `scripts/api_surface.py check` passes with 159 items. With `--features graph` it passes with 200 baseline items and 41 additions allowed under `vlite::property_graph::`. Eight of those are new in EPIC-006: `enable_graph`, `inspect_graph`, `strip_graph`, `verify_graph`, `GraphCounts`, `GraphInspection`, `GraphVerification` and `StripGraphReport`. No addition falls outside that prefix.
  - The checker reads rustdoc `all.html`, which lists neither enum variants nor re-exports. It therefore cannot see the three new cfg-gated `Error` variants or the planned `vlite::maintenance::` re-exports of the eight items (REQ-003, REQ-004, REQ-051). In rustdoc these re-exports appear only as re-export lines on the `maintenance` page.
  - The default `vlite-maintain` prints exactly the usage text of the previous source and exits 1, including for `enable-graph` and `strip-graph`.

## EPIC-007 server error surfaces (v1.2.15, on `main` after `8eb5d7a`; pending review)

These are engineering decisions made autonomously under the user's delegation. No gate is approved. Scope: ITEM-027..ITEM-030, LEG-017, LEG-019, LEG-020 (REQ-053, REQ-055). Every server, gRPC and webhook surface that sees a graph-enabled file fails explicitly. Nothing reaches the generic 500, and nothing skips silently. Graph-off builds are unchanged: every edit is `#[cfg(feature = "graph")]`.

### Error mapping (`From<Error> for ApiError`, `src/server.rs`)

| Class | `Error` variants | REST status | gRPC code | Stable code (REST `code`, gRPC `vlite-error-code`) |
|---|---|---|---|---|
| State conflict | `GraphsNotEnabled`, `GraphExists`, `GraphNotFound`, `GraphNotEmpty`, `TooManyGraphs`, `GraphKeyExists`, `GraphKeyConflict`, `GraphNodeNotFound`, `GraphEdgeNotFound`, `GraphNodeHasEdges`, `GraphAttachmentConflict`, `GraphAttachmentNotFound`, `GraphRecordNotAttachable`, `GraphAttachedRecord`, `GraphAttachedRecordExpiry`, `GraphCursorStale`, `GraphUnsupportedOperation` | 409 | `FailedPrecondition` | snake_case of the variant name |
| Invalid input | `GraphsRequireV5`, `InvalidGraphName`, `GraphLimitExceedsCeiling`, `GraphInvalidOptions` | 422 | `InvalidArgument` | snake_case of the variant name |
| Unsupported surface | `GraphChangeFeedUnsupported` | 501 | `Unimplemented` | `graph_change_feed_unsupported` |

- The gRPC code is derived from the REST status by the existing translation in `src/server/grpc.rs` (`(409, _) => FailedPrecondition`, `(400..=499, _) => InvalidArgument`, and the new `(501, _) => Unimplemented`), so REST and gRPC cannot drift.
- Rule: a reference to a missing graph entity is a state conflict (409), not 404, because the request is well formed and the resource it names is the graph state. Before v1.2.15 the PRD's LEG-017 row listed "409/400/422/501"; no variant maps to 400, because every graph validation error names an invalid value in a well-formed request, which the server already reports as 422 (`InvalidCollectionName`, `InvalidCollectionSettings` and `InvalidQuery` are the existing examples). The row now says 409/422/501.
- `graph_code()` is the single source of the stable codes. Its `_ => "internal_error"` arm is unreachable for graph variants and is guarded by the `graph_errors::table()` unit test, which lists every variant, its status and its code. The table is completed by hand: a new graph variant must be added to it, as its doc comment says.
- Open question for review: `GraphInvalidOptions` is 422 because it means a bad argument (for example `strip_graph` without `confirm_graph_data_loss`). `enable_graph` also returns it for an unsuitable source file, which could be read as 409. The server cannot reach either call (maintenance runs offline on closed files), so the status is defensive. It is unchanged unless the reviewer asks otherwise.

### Change feed surfaces (REQ-053, LEG-019, LEG-020)

- `Service` caches `graph_enabled` once at construction (`Database::is_graph_enabled()`). The flag is a file property that cannot change while a process holds the file (`enable_graph` and `strip_graph` write new files), so no per-request locking is needed.
- `Service::reject_graph_feed()` returns `GraphChangeFeedUnsupported` (501). It is called first in `GET /v1/changes` and in the gRPC `Changes` and `WatchChanges` handlers, before argument validation, so the answer does not depend on the request. The gRPC handlers still authenticate first.
- Webhooks: `POST /v1/admin/webhooks` is rejected before validation and stores nothing. The dispatcher never starts a worker on a graph-enabled file. Subscriptions already stored in `webhooks.json` for a graph-enabled file fail startup with `GraphChangeFeedUnsupported`, before any audit, credential, scheduler or webhook state is written (`reject_graph_subscriptions`). An empty or missing store is accepted, and so is a missing admin directory.
- Replica export (`replication_export` over REST and gRPC) needs no rejection. LEG-014 chose follower carry-through, so graph segments are valid payloads for a graph-feature follower, and a follower without the feature rejects them at the header (`UnsupportedFeature`).
- The server serves the vector APIs on graph-enabled files in graph builds. Graph-specific server, MCP and SDK surfaces remain later work (EPIC-011 and later).

### Evidence

- `tests/graph_server_surfaces.rs` (TEST-015, `#![cfg(all(feature = "server", feature = "graph"))]`) runs each case on a vector-only file, an empty graph-enabled file and a populated graph-enabled file in the same build:
  - `leg017_attached_record_writes_fail_explicitly_and_others_match_vector_only`: expiring or deleting (single, batch, import) an attached record is an explicit 409; unattached records keep the vector-only results.
  - `leg020_rest_change_feed_is_rejected_on_graph_files`: `/v1/changes` is 501 `graph_change_feed_unsupported`; vector-only files keep the feed.
  - `leg019_webhook_registration_is_rejected_on_graph_files_without_side_effects`: nothing stored, no worker started.
  - `leg019_startup_with_configured_webhooks_fails_on_graph_files_without_side_effects`: startup fails with `GraphChangeFeedUnsupported` and the admin directory still holds only `webhooks.json`; an empty store is accepted and still starts no worker.
  - `grpc::leg017_leg020_grpc_rejects_feeds_before_streaming_and_maps_attached_records` (feature `grpc`): `Changes` and `WatchChanges` answer `UNIMPLEMENTED` before any stream opens and before cursor or limit parsing, after authentication; attached-record writes are `FAILED_PRECONDITION`. Each carries the `vlite-error-code` trailer.
- Unit tests: `server::graph_errors` (the every-variant status and code table) and the gRPC status translation in `src/server/grpc.rs`.
- The file has no `[[test]]` entry and is gated at file level, so it compiles to an empty test binary in every build without `server` and `graph`. CI runs it in two places: the `--all-features` workspace run, and a dedicated step `--features server,graph`, which also proves that the cfg gaps `--all-features` hides (no `grpc` or `mcp`) compile and pass.

## ITEM-018 TEST-005 matrix and reference model (v1.2.15, on `main` after `8eb5d7a`; pending review)

These are engineering decisions made autonomously under the user's delegation. No gate is approved. ITEM-018 was left In Progress in v1.2.9 with two gaps: the TEST-005 rows LEG-009..LEG-016 and LEG-018, which needed the EPIC-006 and EPIC-007 surfaces, and a file-backed random model that did not reach 10,000 steps per seed.

- **Matrix shape.** `tests/graph_integrity.rs` now has 13 tests: the four v1.2.9 tests for LEG-001..LEG-007 (LEG-008, dropping a collection, cannot reach an attached record) and nine new ones. Each new test runs its operation on the three file states (vector-only v5, graph-enabled without graph data, graph-enabled with nodes, edges and attachments) in the `default` collection and in a named one. Where the operation writes a new file, the test compares the source bytes and its inspection before and after.

  | LEG | Test | What it shows |
  |---|---|---|
  | LEG-009 | `compaction_copies_graph_state_flag_and_id_high_water` | `compact*` keeps `FLAG_GRAPH`, the graph state and the node and edge high-water marks; a deleted highest ID is not reused after compact and reopen |
  | LEG-009 | `compaction_expiry_omits_expired_records_but_never_attached_ones` | expired records are dropped as before; attached records cannot carry an expiry (LEG-004), so none is dropped |
  | LEG-010 | `upgrade_refuses_graph_files_and_still_upgrades_vector_files` | `GraphUnsupportedOperation` and nothing published for graph files; vector files upgrade unchanged |
  | LEG-011 | `inspection_verification_backup_restore_and_drill_accept_graph_files` | `inspect*`, `verify*`, `backup*`, `restore*` and `drill` accept graph files and keep the graph state |
  | LEG-012 | `reencryption_and_point_in_time_restore_carry_graph_frames` | `reencrypt` and `restore_to` keep op-12 frames and the flag |
  | LEG-013 | `change_feeds_refuse_graph_files_and_write_nothing` | `read_log_changes`, `feed_cursor` and `changes_after` return `GraphChangeFeedUnsupported` and write nothing |
  | LEG-014 | `replicas_bootstrap_apply_sync_and_promote_graph_files` | followers bootstrap, apply, sync and promote graph files with equal state; a vector-only follower refuses a graph segment and is unchanged |
  | LEG-015 | `ann_indexes_snapshots_and_disk_ann_ignore_graph_state` | HNSW, ANN snapshots and disk ANN behave as on vector-only files |
  | LEG-016 | `query_language_has_no_graph_syntax_and_unchanged_results` | six graph-shaped statements fail to parse; vector query results are unchanged (REG-005 pins the parse goldens) |

- **Findings that differ from the plan's wording.** None is a product defect.
  - Node IDs start at 0, not 1.
  - Commit counts: on the `default` collection, the vector-only setup makes 2 commits and the graph setups 4; on the named collection they make 3 and 5 (the collection creation is a commit).
  - `upgrade` of a v4 source writes v4, as before; the LEG-010 vector case asserts the existing behavior.
  - In the LEG-014 matrix the vector-only follower is a separate file with its own lineage, so its refusal comes from the lineage check ("segment is from another lineage"). The EPIC-006 test above reaches the header check instead. Both are `ReplicaDiverged`, and the follower is unchanged in both.
  - ANN snapshots report `Loaded` on graph files, as on vector files.
  - The background HNSW rebuild in `src/rebuild.rs` has no separate graph case; it reads only vector records.
  - LEG-011 and LEG-012 run on plain files. The encrypted and timestamped variants are in TEST-008 (`tests/graph_maintenance.rs`) and TEST-004 (`tests/graph_recovery.rs`).
- **LEG-018 (graph-unaware builds).** Two pieces of evidence, because `--workspace --all-features` links `vlite-c` with `graph`:
  - `tests/graph_compat.rs` in the default build: every graph-unaware entry point returns `UnsupportedFeature(4)` on graph fixtures and leaves the source bytes unchanged.
  - `scripts/graph_compat_check.py run` builds a revision without `graph` from a clean `git archive` and checks the Rust probe, the C ABI through Python and the Node addon against graph-enabled files. On the candidate tree of this change (an unpublished commit object with the same tree, because the script archives commits, not the working tree), 123 entry-point probes matched: 117 `UnsupportedFeature(4)` and 6 `ReplicaDiverged` (the follower `apply` calls).
- **Reference model.** `random_batches_are_atomic_and_replay_identically` in `src/property_graph/tests.rs` now runs 4 seeds × 10,000 steps, each on its own file, mixing graph batches with vector puts and deletes and reopening every 500 steps. Each failed step leaves the file length and the graph state unchanged, the invariants hold after every step, and every reopen reproduces the live state including `graph_sequence`. The seeds run in parallel threads because every commit is synced. The engine-level model (20 seeds × 10,000 mutations) is unchanged.

## Requested command-line and Dashboard follow-ups (v1.2.16)

The user's request to add graph command-line and Dashboard support after the
current work is now a persistent backlog in PRD Section 14.3
(FOLLOWUP-001/FOLLOWUP-002). Existing command-line support remains limited to
the EPIC-006 maintenance commands; no new CLI grammar or operation coverage has
been selected or implemented.

Dashboard graph support depends on EPIC-011's REST routes, graph-scoped
credentials, authorization, OpenAPI and endpoint tests, with its existing P3
entry and security gates. It must use the server's single database owner.
The MVP UI non-goal and graph-off behavior are unchanged until that follow-up
is implemented under its own reviewed contract.

This request does not approve any open gate, add graph to distributed
artifacts, or authorize graph query-language experiments or implementation.
EPIC-014/EPIC-015 remain separate, conditional work.

### DO-005 graph-off refresh attempts (rejected)

The final session report at 2026-10-25T05:56:47.493Z records the campaign for
`4312fb30b88e3527c245287858a64bdebf34038e`: default features (graph off),
release build, `--offline --locked`, from a clean `git archive`. The host
matched the EPIC-005 reference VM. All 30 workload runs exited zero, but only
6 of 30 met the unchanged background CPU policy (mean <=10% and <=50% per
0.5 s sample); no workload passed all five repetitions. The outcome is
**REJECTED**, not baseline evidence. The earlier `059650e` attempt was also
rejected, contended by concurrent work.

Observed CPU contributors included Defender, System and the WSL VM. Fifteen
commits landed during the campaign, including Docker/WSL and Dashboard
capture work; the intended quiet window did not hold. These are observations,
not proof of causation. No performance figures from either rejected campaign
are recorded here or accepted as baseline evidence. `baseline.md` is untouched
and the historical `81ff901` claims stand.

The raw logs are retained outside the repository in the private recovery
handoff and must be preserved until the decision below. They are not accepted
baseline evidence. The retained artifacts include
`graph-off-reference-059650e.json`, `graph-off-reference-4312fb3.json` and its
log, diagnostic JSON/log, machine identity and process CPU snapshots. The
source report is the timestamped final assistant message retained in the
recovery handoff. Large run artifacts are not copied into this repository.

**Open decision; owner: user/maintainers.** Decide whether
benchmark-attributable scanner/System CPU counts as background CPU for
sub-second workloads. The alternatives are:

- **A:** Keep the policy unchanged and rerun in an exclusive host window with
  other sessions and Docker/WSL stopped by their owners. The original report
  expects the sub-second workloads to fail again under the same policy.
- **B:** Keep the thresholds but gate acceptance only on workloads long enough
  to yield several CPU samples; report sub-second workloads as informational.
- **C:** Exclude attributable scanner CPU from the background figure. This is
  a policy change that must be documented in `baseline.md` before use.

No option is chosen, no thresholds are changed and no new run is authorized
by this record. The graph-off refresh remains blocked pending that decision
and a genuine coordinated quiet host window; existing gates remain unchanged.

**Separate-host comparison (2026-10-25/26; informational only).** A campaign
on a different host compared `81ff901` with `c024dce` under the unchanged CPU
policy (handoff branch `mahpate-do005-sephost-handoff`, commit `3bef798`). It
is not baseline evidence. The outcome was INCOMPLETE: 58 of 60 slots were
accepted, and two `hnsw_100k_384` slots failed the policy on all three
attempts. The host was an AMD EPYC Hyper-V VM, so the reference identity
checks fail. The quiet window held only partially, and the VM stalled for
about 74 minutes. The pinned revision is also behind current `main`. Its
figures are not cited here; `baseline.md` and `81ff901` are unchanged. By
owner direction (2026-10-25), the DO-005 rerun is deferred until the Dashboard
graph work is finished and will measure the `main` of that time.

## FOLLOWUP-001 source-only graph CLI (v1.2.17; implemented and reviewed; source-only, not distributed)

**Ordering.** The user chose to implement and test a source-only, opt-in graph
CLI ahead of the deferred benchmark work. This removes FOLLOWUP-001's
dependency on the current publication and the DO-005 refresh. It approves no
phase, security or release gate: GATE-008 and the other open gates are
unchanged, the rejected DO-005 attempts and the `81ff901` baseline stay as
recorded above, and the refresh remains blocked. FOLLOWUP-002 (Dashboard) is
unchanged.

**Placement.** A new binary, `vlite-graph-cli` (`src/bin/vlite-graph-cli.rs`),
with `required-features = ["graph", "json"]`. The name avoids confusion with the
`crates/vlite-graph` harness. `json` is needed because the existing
`Metadata`/`MetadataValue`/`Condition` serde implementations live behind it.
`vlite-shell` and `vlite-maintain` are unchanged. Artifact builds pin features
without `graph`, so the binary is never built for release archives, the Docker
image or packages; `tests/graph_build_boundary.py`
(`test_graph_binaries_stay_source_only`) asserts that it is the only
graph-requiring binary and that `Dockerfile` and `release.yml` do not name it.

**Contract.** `vlite-graph-cli --db PATH [--writable] [--explain] COMMAND [REQUEST]`,
one JSON request object per call (`-` for stdin, capped at 64 MiB). The full
user contract is in [`docs/TOOLS.md`](../../TOOLS.md#graph-operations).

- Reads (`graphs`, `get-node`, `get-edge`, `find-nodes`, `neighbors`,
  `traverse`, `shortest-path`) use `Database::open_read_only` and
  `GraphRef::query`/`explain`; they never write and share the file with
  other readers.
- Writes need `--writable` and the exclusive lock: `init` (new file only,
  `create_with_graph`), `create-graph`, `drop-graph` and `batch`. `batch` maps
  every `GraphOp` variant, plus `Mutation::Put`/`PutWithMetadata`/`Delete`, to
  one `GraphMut::write_batch` call, so mutation semantics, atomicity and
  limits come from the adapter. There are no separate single-operation write
  commands; a one-element batch is the same call.
- Requests are parsed with unknown fields and repeated property or metadata
  keys rejected before the file is opened, so a bad request leaves the file
  untouched and no input is silently ignored.
  Vector-only files are never enabled implicitly (`graphs_not_enabled`).
- Limits use `GraphLimits::DEFAULT` for omitted fields and pass the result to
  the adapter, which rejects values above the ceiling instead of clamping.
  Truncation, cursors, statistics and `view_sequence` are returned as-is.
- Output is one JSON object on stdout (exit 0) or
  `{"error":{"code","message"}}` on stderr (exit 1). Graph error codes reuse
  the REST names; `server.rs` keeps its mapping private, so the CLI holds a
  copy. All u64 IDs, revisions and sequences are decimal strings in output and
  accept strings or JSON integers in input, so values up to `u64::MAX` keep
  full precision.

**Not covered.** Encrypted files (no key option), non-UTF-8 vector payloads,
interactive shell integration, schema, hybrid retrieval and any graph query
language. REST, MCP, SDK and Dashboard surfaces are unchanged.

**Evidence.** `tests/graph_cli.rs` (6 tests, written first and run failing
against a stub) covers round trips, atomic batches with vector mutations and
`u64::MAX` records, unchanged file bytes after failed requests and reads, lock
ownership, refusal of non-graph files and the help contract.

## Graph wave 1 (v1.2.18, 2026-10-25; source-only, opt-in `graph` builds, not distributed)

Wave 1 integrates EPIC-008 to EPIC-012 and EPIC-015 ITEM-051 on one branch.
Graph stays out of the Docker image, release binaries and packages, and the
artifact feature pins are unchanged, so GATE-008 is not approved. DO-005 is
deferred by the owner on 2026-10-25: the benchmark policy is unchanged, the
baseline stays `81ff901`, and the rejected `4312fb3` results above are not
evidence. Graph writes go through the normal commit path, so they follow the
database's durability mode, including the opt-in periodic mode added
separately (outside this PRD).

### EPIC-008 schema and property indexes (DO-008)

- **Entry.** `PutSchema` is graph entry tag 12. Each entry carries the whole
  declared schema of one graph (labels, relationship types, required, typed
  and unique properties, allowed endpoints, `Multi`/`UniqueTriple`
  multiplicity, exact/range property indexes) and replaces the previous one;
  there are no partial schema edits. Other tags are unchanged, and unknown tags
  are still corruption.
- **Enforcement.** Validation runs in the engine's shared apply path, so the
  planner, replay and recovery accept and reject the same entries. A rejected
  batch writes nothing. Files without a schema behave exactly as before.
- **Indexes.** Declared property indexes are derived state. They are not
  persisted; replay rebuilds them on reopen. `find_nodes` returns the same
  results with and without an index (`IndexChoice::PropertyExactIndex` or
  `PropertyRangeIndex` in `explain`).
- **Maintenance.** Compaction writes `PutSchema` right after `CreateGraph`, so
  compact, reencrypt and backup keep the schema.
- **Errors.** Violations are `Error::GraphSchemaViolation`: REST 409
  `graph_schema_violation`, the same code in the CLI, MCP and C ABI.
- `ensure_edge` applies to `UniqueTriple` relationship types. The observed
  schema keeps the PD-011 budget and marks entries declared or observed.

### EPIC-009 MCP graph tools

- Tools are compiled only with `graph` and listed and callable only on
  graph-enabled files. Read tools: `vlite_graph_list`, `vlite_graph_get`,
  `vlite_graph_find_nodes`, `vlite_graph_neighbors`, `vlite_graph_traverse`,
  `vlite_graph_shortest_path`. With `--writable` only: `vlite_graph_create`,
  `vlite_graph_drop` and `vlite_graph_write` (one atomic `GraphOp` batch).
  `vlite_info` adds `graph_enabled`/`graphs` on graph-enabled files only.
- **Output shape.** The tools use the same JSON contract as `vlite-graph-cli`
  through the shared `src/bin/tool_support/graph_json.rs`: u64 values as
  decimal strings, unknown fields rejected, explicit limits up to
  `GraphLimits::CEILING`, truncation and cursors returned as-is, optional
  `explain`, and the REST error codes.
- Vector-only files keep the golden `tools/list` and transcript byte for byte.
- Pending for wave 2: schema tools (AC-018), `vlite_graph_retrieve`
  (ITEM-038) and `vlite_graph_query` (ITEM-052).

### EPIC-010 hybrid retrieval

- Pivots use the unchanged `search_hybrid` (exact or ready-ANN vector leg, BM25
  text leg) on the same view as the expansion. Hits are re-validated, mapped
  through attachments, and unmapped ones are counted in a bounded
  `unmapped_pivots`.
- **Timing.** `stats.pivot_ms` measures pivot search, re-validation and
  mapping, and has no deadline. `stats.expand_ms` measures expansion and
  evidence projection. The read deadline (PD-008) and cancellation apply only
  to the expansion.
- **Bounds.** PD-015 `pivot.k` ≤ 100 (default 20). PD-016 exact-scan bound:
  default 100,000 records, ceiling 1,000,000; above the bound the call fails
  with `Error::GraphPivotTooExpensive` (REST 422
  `graph_pivot_too_expensive`). Evidence excerpts default 512 bytes, ceiling
  4,096 (PD-013). Evidence items are capped by `max_evidence_items`, default
  100, ceiling 1,000. All bounds are checked before execution.
- An ANN vector leg without a ready index returns `IndexNotReady`; it never
  falls back to an exact scan silently.

### EPIC-011 REST routes and authorization (GATE-007)

- **Roles.** The admin token has full access. The read-only token can call the
  read routes. `vlk_` credentials need `graph_read` or `graph_write` plus a
  `graphs` binding of `["*"]` or 1–64 graph names. Collection scope never
  implies graph access, and graph scope never implies collection access.
  Tenant credentials cannot hold graph operations. Signed-in accounts are
  covered in the 2026-10-25 section below.
- **Order.** Authentication and authorization come first; vector-only files
  then answer 409 `graphs_not_enabled`.
- **Collections.** Batches with vector or attachment operations also need
  collection `write`, collection access and quota. GATE-007 fix: for scoped
  credentials, `delete_node` looks up the node's attached records under the
  held write lock and needs collection `write` and access to every attached
  collection, because deleting the node removes those attachments. No other
  graph operation drops attachments implicitly (`drop_graph` needs an empty
  graph).
- **OpenAPI.** `src/openapi_graph.json` is merged into the served document only
  in graph builds. `src/openapi.json` stays byte-identical.
- **Hold time.** Read and write guard hold times are kept per guard and served
  at `GET /v1/admin/graph/stats` (admin only). An indicative debug-build
  measurement in `tests/rest_graph.rs` showed read p99 of 11 ms; it is not a
  reference-machine figure.
- **GATE-007:** passed by security review on 2026-10-25. The review read the
  code (no penetration test). It found one LOW issue, the `delete_node`
  attachment case above, which is fixed and covered by
  `deleting_an_attached_node_needs_collection_write_scope`. Proxy credential
  handling is reviewed with ITEM-043 in wave 2.

### Signed-in user graph access (owner decision 2026-10-25)

> Superseded by [Tenant and user graph scoping (v1.2.20)](#tenant-and-user-graph-scoping-v1220).

- **Decision.** The owner chose, after being told graphs are not per tenant,
  to give signed-in Dashboard accounts graph access: super admins keep full
  access (they are the admin role); editors get `graph_read` and
  `graph_write`, viewers `graph_read`, all bound to `["*"]` and regardless of
  the account's tenant.
- **Mechanism.** `AccountRole::ops` adds the graph operations in graph builds
  only. `Grant::for_user` binds `["*"]` when a graph operation is present and
  sets a private `account` flag that no API credential sets;
  `Grant::graph_allowed` exempts only that flag from the tenant denial.
  `vlk_` credentials are unchanged: tenant keys still cannot hold graph scope
  and are denied every graph.
- **Trade-off.** Graphs are shared, so a tenanted account sees and can change
  graph structure (nodes, edges, properties, graph create and drop of empty
  graphs) created by other tenants' accounts. Graph names, node keys and
  property values are therefore visible across tenants.
- **Kept vector isolation.** Batch operations that attach, detach, put or
  delete vectors, and `delete_node` on a node with attachments, still need
  `write` and a collection the grant covers. The check now uses the stored
  collection name exactly (`cover` in `src/server/graph.rs`) instead of the
  tenant-qualifying `open`: qualifying a graph op's name for the check while
  the batch wrote the unqualified name would have authorized one collection
  and touched another. Tenanted accounts name their collections
  `t<tenant_id>__<name>`. Wave-1 graph reads return no record data; any
  retrieve or evidence route (wave 2) must filter by collection coverage.
- **Tests.** `signed_in_accounts_get_graph_access_by_role`,
  `tenanted_editors_write_any_graph_but_only_their_own_vectors` and
  `tenant_credentials_still_get_no_graph_scope` in `tests/rest_graph.rs`, and
  `only_signed_in_accounts_keep_graph_scope_when_tenanted` in
  `src/server/credentials.rs`.
- **Follow-up.** This changes the GATE-007-reviewed surface and needs a
  follow-up security review. Per-tenant or per-user graph scoping (graph
  ownership or bindings on accounts) is a planned follow-up.

### EPIC-012 C ABI and Python

- Graph commands use the envelope `{"op":"graph","request":{...}}` with the
  same JSON contract as the CLI. `{"op":"capabilities"}` reports
  `{"graph": bool}`.
- `vlite-ffi` has a non-default `graph = ["vlite/graph"]` feature. Builds
  without it parse the envelope and return `unsupported`; Python raises
  `EmbeddedError("unsupported")`, including with libraries that predate the
  feature.
- Graph create accepts `flush_interval_ms` like the other create paths.
- Error codes match REST, including `graph_schema_violation` and
  `graph_pivot_too_expensive`.

### EPIC-015 graph query text (ITEM-051)

- **Decision.** The repository owner directed adding the graph query language
  on 2026-10-25. EPIC-014 was skipped: no LLM evaluation was run, GATE-005
  experiments were not run, and GATE-006 is satisfied by the owner's decision
  rather than by experiment evidence.
- **Scope.** The language is read-only and VLite-specific. Each accepted text
  maps one-to-one to a typed `GraphQuery` (`GetNode`, `FindNodes`, `GetEdge`,
  `Neighbors`, `Traverse`, `ShortestPath`) and adds no execution semantics.
  Unsupported constructs, parameter problems and size caps return typed errors
  with byte offsets.
- **Keywords.** It may share clause words with `src/query_language.rs` because
  the entry points are separate and each rejects the other's texts.
  `SEARCH` is rejected. `src/query_language.rs` is unchanged.
- Entry points: `GraphRef::query_text`/`explain_text`, `parse_graph_query` and
  `vlite-graph-cli query`. The grammar is in `docs/GRAPH-QUERY.md`. The MCP
  tool (ITEM-052) is pending.

## Graph wave 2 (v1.2.19, 2026-10-25; source-only, not distributed)

Wave 2 adds the MCP schema, retrieve and query tools (ITEM-038, ITEM-052), the
matching REST routes, the MCP proxy (ITEM-043), the Node graph API and
`docs/GRAPH.md` (EPIC-013), and the Dashboard explorer files (FOLLOWUP-002).
GATE-008 is not approved; the artifact pins exclude `graph` and `mcp-proxy`.

- **Proxy dependency.** `mcp-proxy` reuses the existing optional `reqwest`
  dependency (blocking client, rustls); no new crate is added.
- **Proxy scope.** `vlite-mcp --server URL` proxies the graph tools only. It
  never opens a file, lists graph tools only when `GET /v1/graphs` answers
  200, reads its token from `--token-env` and never logs it, and refuses
  write tools in read-only sessions before contacting the server. Vector
  tools stay local-file only.
- **Dashboard.** `src/dashboard/graph.js` is a read-only explorer over the
  typed REST routes, hidden without graph support or on vector-only files,
  with an admin graph-stats panel. Writes stay with REST, the CLI and MCP.
  The navigation hooks wait for the Dashboard redesign; CI already runs
  `tests/dashboard_graph.test.cjs`.
- **Retrieve redaction.** Retrieve returns vector record data, so a scoped
  caller needs `read` on the pivot collection (403 otherwise), and evidence
  from any collection it cannot read is dropped and counted in
  `redacted: {count, collections}`. Pivot and evidence names are checked by
  their exact stored name, like batch collections (`cover`). The first
  version qualified them with the caller's tenant, so a tenanted account's
  check missed the collection actually read and could return another
  tenant's or untenanted evidence. That is fixed and covered by
  `tenanted_accounts_retrieve_only_their_own_evidence`. Schema and query
  text return graph structure only. The proxy goes through these routes.
- **Status.** Signed-in account graph access stands by owner decision; the
  GATE-007 re-review of wave 2 and the account grants is pending. DO-005 is
  deferred by the owner until after the UI work. Remaining: EPIC-014
  (skipped by owner), GATE-008 and tenant- or user-scoped graphs (future).

## Tenant and user graph scoping (v1.2.20)

Owner request: "tenant specific or user specific knobs for graph access".
Supersedes [Signed-in user graph access](#signed-in-user-graph-access-owner-decision-2026-10-25).
Source-only, opt-in `graph` builds. **GATE-007 re-review passed
(2026-10-25):** a code-reading security review of `0d1469a..be98a80` (wave 2
routes, retrieve redaction, the MCP proxy, account grants, tenant scoping,
the Dashboard explorer and query text) found no exploitable issue. It did
not cover resource limits beyond `GraphLimits::CEILING`.

- **Tenant-owned graphs.** Graph names are qualified like collections: a
  tenanted principal (signed-in user with `tenant_id` or tenant `vlk_` key)
  addresses `g`, stored as `t<tenant_id>__g` (`tenants::qualify`), on create,
  list, get and every per-graph route. Responses unqualify names; lists keep
  only graphs the grant covers. Untenanted principals and the admin/read-only
  tokens use stored names, as for collections. `Grant::graph_allowed` checks
  the stored name against `owns` and the qualified bindings; the private
  `account` exemption is gone. Tenant-local names are at most 28 bytes (the
  35-byte prefix plus the 63-byte graph-name limit).
- **Tenant keys.** `graph_scope` no longer refuses tenant credentials; their
  `graphs` bindings are tenant-local and validated with the 28-byte limit.
- **Account knobs.** `graphs` (absent = role default `["*"]`; `[]` = none;
  or 1-64 names validated like credential bindings) and `graph_access`
  (`none` | `read` | `write`, capped by the role; a viewer with `write`, or a
  super admin with either knob, is 422). Stored with serde defaults and
  skipped when absent, so older `users.json` files load unchanged; builds
  without `graph` refuse both fields (`deny_unknown_fields`), failing closed
  rather than dropping a restriction. Changing either bumps the account
  version and ends its sessions. Account views and `/v1/auth/session`
  report the stored knobs; `GET /v1/capabilities` `scope` reports the grant.
- **Unchanged.** Vector checks for batches and retrieve keep the exact
  stored collection name (`cover`). Graph files and on-disk names are
  unchanged. The CLI and MCP-on-file paths have no tenants; the MCP proxy
  goes through REST and inherits the new rules.
- **Deliberate narrowing.** Existing tenanted accounts lose access to
  untenanted and other tenants' graphs. Untenanted accounts and keys are
  unchanged. Tenant delete is refused (409 `tenant_not_empty`) while the
  tenant owns graphs.
- **Residual risks.** The 64-graph limit is per file and shared by all
  tenants, so one tenant can exhaust it. Untenanted principals can create
  `t<id>__*` names directly, as for collections. Graph error messages carry
  no graph name.
- **Tests.** `tests/rest_graph.rs` (`tenant_accounts_own_isolated_graphs_under_unqualified_names`,
  `tenant_credentials_bind_graphs_inside_their_tenant`,
  `account_graph_knobs_narrow_access`, `tenanted_editors_write_only_their_own_vectors`,
  `tenanted_accounts_retrieve_only_their_own_evidence`), `tests/users.rs`
  (`graph_knobs_are_validated_and_reported`,
  `account_files_without_graph_knobs_still_load`), `graph_scope_is_qualified_by_tenant`
  in `src/server/credentials.rs`, and the Dashboard helpers in
  `tests/dashboard_graph.test.cjs`.
- **Dashboard.** `src/dashboard/graph.js` exports `accountGraphRequest`,
  `accountGraphChanges`, `accountGraphFields` and `accountGraphView`. The
  Users & access form uses them (v1.2.21): the graph fields appear only when
  graph support is detected and are never sent for super admins.

## GATE-008 approval and v0.5.0 release (owner decision 2026-10-25)

- **Decision.** The owner approved GATE-008 on 2026-10-25 ("Enable graph in
  artifacts, prepare v0.5.0, and push the tag to publish"). VLite 0.5.0 ships
  the experimental, opt-in property graph in distributed artifacts.
- **Artifacts.** Release and Docker binary builds add `graph,mcp-proxy`. The
  Docker, release C library, wheel and Node release tarball `vlite-ffi`
  builds add `--features graph`. The Docker image's API server (`vlite-server`) has graph; `vlite-graph-cli`
  is not in the image. Release archives add `vlite-mcp`, `vlite-graph-cli`, the
  graph guides and `src/openapi_graph.json`. REG-013
  (`tests/graph_build_boundary.py`) asserts these inclusions instead of the
  former exclusions.
- **Unchanged.** The Cargo `default` features and the default `vlite-ffi`
  crate build stay graph-free (CI still checks that default bindings reject
  graph calls). Graph remains opt-in per file; vector-only files, APIs and
  defaults behave as before.
- **DO-005 deferred.** At the owner's direction ("go ahead with publish the
  release and we can update banchmark at the end") the graph-off baseline
  refresh happens after the release. The baseline stays `81ff901`, the
  REG-010 policy is unchanged, and the release claims no new benchmark
  numbers. This overrides the v1.2.16 precondition that DO-005 precede a
  GATE-008 request.
- **Release note.** The ITEM-048 text in `docs/GRAPH.md` ships in
  `docs/RELEASE-0.5.0.md`.

## Graph compiled by default, no longer experimental (v0.5.1, owner decision 2026-10-26)

- **Decision.** The owner asked to "include the property graph by default and
  remove the experimental tag/feature" for v0.5.1, and confirmed that new
  files stay plain unless graph is explicitly requested.
- **Build.** Graph code is always compiled. The `graph` Cargo feature (crate
  and `vlite-ffi`) remains as an empty no-op alias so existing
  `--features graph` / `server,graph` commands, scripts and CI still build;
  `mcp-proxy` is unchanged. The graph-free code paths are removed.
- **Files.** Unchanged: a file holds graphs only after `create_with_graph`,
  `vlite-graph-cli init` or `vlite-maintain enable-graph`. Graph-enabled files
  still reject change feeds, webhooks and upgrade.
- **Docs.** "Experimental" labels for the property graph are removed from the
  guides, READMEs and help text (PRD v1.2.23). Website and Dashboard copy is
  updated separately.
- **DO-005.** Still deferred; the baseline stays `81ff901` and the REG-010
  policy is unchanged.

## Graph/vector feature parity (v0.6.0, user decision 2026-10-26)

- **Decision.** The user asked to "make sure both graph and vector gets enabled
  with 100% features" and approved parity for change feeds, webhooks,
  capabilities and upgrade on graph-enabled files (PRD v1.2.24, EPIC-016).
  Next step, not part of this change: new files graph-enabled by default.
  Create defaults are unchanged here.
- **Feed events.** Graph commits produce `GraphCreated`, `GraphDropped` and
  `GraphBatch` (`graph_created`, `graph_dropped`, `graph_batch` in JSON).
  A batch lists the frame's entries as the engine records them, with node and
  edge keys resolved from the replayed log, and `revision` = commit sequence.
  The cursor format is unchanged and a commit is never split across pages.
  A frame that interleaves graph entries with inline vector mutations yields,
  in log order, a `graph_batch` per maximal run of graph entries and the
  usual `put`/`delete` events, all with the commit's sequence. Graph events
  have an empty `collection`. Encrypted files decrypt with the database key.
- **No compatibility machinery (user guidance, 2026-10-26).** "Since we
  don't have the customers who is using VLite, you can assume users will use
  the latest version and Vlite formats." So:
  - `ChangeKind` and `GraphChange` are exhaustive; the new variants are a
    source break for exhaustive matches, noted only in the release notes.
    Bindings match the graph variants explicitly; there are no
    unknown-type shims, feature negotiation or versioned feed parameters.
  - `GraphChangeFeedUnsupported` and `GraphUnsupportedOperation` are removed
    with their codes; no graph error maps to HTTP 501 or gRPC
    `UNIMPLEMENTED` any more.
  - The `graph` Cargo feature alias is removed from `vlite` and `vlite-ffi`;
    no CI, release, Docker, wheel or Node build passed it for a reason other
    than the alias, so they now build with their other features.
  - v3/v4 file reading is unchanged (out of scope). The next step, new files
    graph-enabled by default, will not keep a pre-0.5.0 opt-out.
- **Webhooks.** `events` is optional: a subscription without it receives every
  vector and graph event. `events` selects types, an optional `graphs` list
  filters graph events by name and `collections` filters only vector events.
  Responses include `events` and `graphs` (`null` when unset).
- **Authorization.** REST scoped readers see graph events only with
  `graph_read` and a `graphs` binding that covers the graph; tenant
  credentials still cannot read `/v1/changes`; gRPC feeds stay admin-only.
- **Capabilities.** `change_feed` and `webhooks` no longer depend on whether
  the file is graph-enabled.
- **Upgrade.** Graph files are already the latest format, so `upgrade` behaves
  as for a plain latest v5 file: a validated copy that keeps `FLAG_GRAPH` and
  every graph frame.
- **DO-005.** Unchanged: still deferred, baseline `81ff901`, REG-010 policy
  unchanged.

## New files graph-enabled by default (v0.6.0, user decision 2026-10-26)

- **Decision.** After parity (PRD v1.2.25) the user asked that new files be
  graph-enabled by default, with no compatibility shims (PRD v1.2.26,
  ITEM-059). Every V5 create sets `FLAG_GRAPH`, and V5 is the default on
  every create path: Rust (file and in-memory), `vlite-server
  --create-if-missing`, `vlite-shell`/`vlite-import --create`,
  `vlite-graph-cli init`, the C ABI, Python, Node and wasm.
- **Removed opt-ins.** `create_with_graph` (Rust, Python), `createWithGraph`
  (Node), the C graph `create` op, the wasm `formatV5` argument and
  `Error::GraphsRequireV5`. There is no plain-create opt-out.
- **Kept.** An explicit V3 or V4 request (`create_with_format`, server
  `--create-format 3|4`, binding `format`) stays plain because those formats
  cannot carry `FLAG_GRAPH`. `vlite-graph-cli init` stays as the graph CLI's
  create command. `maintenance::enable_graph` and `strip_graph` are unchanged.
- **Existing files.** v3, v4 and plain v5 files open as vector-only and are
  never upgraded silently; `upgrade` behaviour is unchanged.

## Re-baseline on Azure (owner decision 2026-10-27)

- **Decision.** After reviewing the combined Azure campaign, the owner asked
  to "publish new results as the baseline". DO-005 now uses release 0.6.0
  (`6428b69`) measured on an Azure Standard_D16s_v5 VM (Xeon Platinum 8370C,
  16 vCPU, 64 GiB, Windows Server 2022, 256 GiB Premium SSD P15 data disk).
  [`baseline.md`](baseline.md) records the machine, commands and medians.
- **Evidence.** Reference profile, five runs per workload, `81ff901` and
  `6428b69` alternating per run. 54 runs came from the first VM; the six
  small-workload runs that failed the CPU gate there were rerun once each, all
  passing, on a second VM of the same configuration. All 60 runs passed the
  unchanged policy (background CPU mean ≤10%, no half-second sample above
  50%). Raw runs, including the superseded ones, are in
  `benchmarks/results/graph-reference-v0.6.0-vs-81ff901-azure.json`.
- **Unchanged.** The quiet-host policy, the REG-010 budget proposals (PD-014,
  GATE-004) and the rejected `4312fb3` attempts. The rejected local runs stay
  invalid; this decision adopts only the Azure campaign.
- **Follow-up.** `scripts\graph_reference.py` still pins the retired
  workstation's identity, so NFR-002/NFR-003 graph-on measurements must move
  to this VM configuration before they are compared with the new baseline.
