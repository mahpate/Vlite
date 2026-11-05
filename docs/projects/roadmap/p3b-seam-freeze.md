# P3b read seam freeze for 0.7.1 (lane R0)

Status: written at the start of 0.7.1 from `v0.7.0` (`02ef3e5f`). Sections 3 and 4 describe what the code does today and
pin it with tests, so they are frozen for the lanes that follow: B4, A4, E4, C4, K71, L5 and the ANN, P4 and P5 lanes.
Section 5 is a proposal that waits for owner approval (OPEN-71-02 to OPEN-71-04 in the
[0.7.1 plan](handover-0.7.0.md#071)). Nothing here changes behavior, a file format or the wire, activates a compact
writer, float16 or temporal copy-out, or supports a size or speed claim. Companion pages: the
[survivor ledgers](p3b-survivor-ledgers.md) of the parked lanes, the [P3 design](p3-compact-vectors.md) and the
[P3c design](p3c-compact-ann.md).

## 1. Baseline

| Item | State at `v0.7.0` |
|---|---|
| Compiled in release builds | `compact::catalog` (the precision catalog kinds, without `payload` and `column`), `compact::half`, the in-memory part of `compact::stats`, and `ImageCheck` in the load, feed and export paths. Compact blocks of a critical or unassigned kind refuse with `UnsupportedFeature(FLAG_COMPACT)`. |
| Compiled only under `cfg(test)` | `compact::source`, `compact::graph`, `compact::vectors`, `catalog::payload`, `catalog::column`, `stats::codec`, `CollectionRecords::Compact`, `RecordOverlay`, `OverlayEntry`, and every compact dispatch arm in `lib.rs`, `collection.rs`, `rerank.rs`, `quality.rs`, `metadata_index.rs` and `text_index.rs`. |
| Gates | Three thread-local test switches (`compact::Activation`, `catalog::payload::Activation`, `compact::source::Activation`), all off by default. No setting, option or file content turns them on. |
| Opening a compact image | `format::load_with` still builds the resident `RecordMap`; `Database::open_backed` then installs the generation and an overlay over it (`compact_from_replay`). The seam does not bound memory at open. |
| Production compact writer | None. Compact images exist only as test fixtures written by `maintenance::write_v5_image`. |
| Lane B (derived indexes, read seam) | Integrated: seam, effective rows, metadata, text and rerank consumers. Its fix-forward `250a98bc` is not. |
| Lane A (record consumers) | Not integrated; four patches parked at `834a0a6f`. |
| Lane C (server and MCP consumers), slice 5 (lifecycle) | Not started. |

Between the 0.7.0 candidate that the readiness inventory used (`5cc496d4`) and the tag, only `src/kv.rs`,
`src/lib.rs` and `src/property_graph/retrieve.rs` changed among the seam's files, and none of those changes is in
`src/compact`, `src/records.rs` or `src/collection.rs`. The seam code is therefore identical in the two.

## 2. The seam in one table

The seam is three concrete pieces and no trait. `CompactView` answers physical questions about one collection of one
pinned generation. `CollectionRecords::Compact` answers effective questions by putting an overlay in front of a view.
`ReadWorkspace` holds the operation-local scratch. P3c's `ReadAccess` is a name for these three together.

| Piece | Functions at `v0.7.0` | Meaning |
|---|---|---|
| `CompactGeneration` (`compact/source.rs`) | `Capture::{push, finish}`, `new`, `view`, `has_collection`, `decode` | One immutable, authenticated image: owned or mapped bytes, the block directory, one lazy `Record` cell per row, the cipher context. Built once, after the whole image validated. |
| `CompactView` | `generation_id`, `len`, `first_id`, `contains_id`, `row_of`, `id`, `revision`, `expiry`, `get`, `get_at`, `get_physical`, `read_vector`, `read_metadata`, `read_payload` | Physical rows of one collection, in ascending ID order. `get_at` applies a deadline; every other accessor is physical. The `read_*` methods copy out of one authenticated frame and never create a cell. |
| `ReadWorkspace` | `new`, `accounting`, `body` (private) | At most 4 MiB of decoded vectors, the latest authenticated plaintext frame and the counters. One per operation, serial, never shared and never persisted. |
| `CollectionRecords` (`records.rs`) | `len`, `is_empty`, `first_id`, `contains_key`, `get_at`, `get_physical`, `apply_committed`, `compact_iter`, `compact_overlay_revision`, `compact_from_replay`, `pin_effective`, `effective_ids`, `effective_vector`, `effective_metadata`, `effective_payload`, `effective_visible`, `source_identity`, `source_accounting`, `overlay_logical_bytes` | The effective view: overlay first, then the base. `pin_effective` clones the enum, which pins the generation and the overlay root together. |
| Consumers (test-gated) | `Collection::rebuild_derived`, `MetadataIndexes::rebuilt_effective`, `TextIndex::rebuilt_effective`, `rerank_effective`, `quality::effective_memory`, the compact arm of `Database::set_collection_settings` | Read through the effective view with a 4 MiB workspace and, where they test visibility, an explicit clock. |

## 3. Frozen semantics

Each statement has an ID so that a test, a mutant and a later change can cite it. A change to a statement needs an
amendment to this page, a test that cites the new text and an updated mutant ledger. The tests named here are the
pinning ones; `freeze_*` tests live in `src/compact/source/freeze_tests.rs`.

### 3.1 The effective-read clock

| ID | Statement | Pinned by |
|---|---|---|
| CLK-1 | An operation captures one clock `T` (milliseconds since the Unix epoch) and passes it to every effective read, including each leg of a multi-leg operation. Seam code never reads the wall clock itself. | `freeze_source_never_reads_the_wall_clock`; the consumers' explicit `now_ms` arguments |
| CLK-2 | A row is hidden when its deadline `<= T`, so a deadline equal to `T` hides it and `T = deadline - 1` shows it. | `freeze_deadline_hides_exactly_at_the_clock_in_every_arm`; `compact_view_get_matches_resident_after_source_drop` |
| CLK-3 | A row with no deadline is never hidden by the clock. | same |
| CLK-4 | The newest overlay entry decides. An overlay put uses its own deadline, so an expired put hides the ID and never reveals the older base row; a tombstone hides the ID at every `T` and never falls through. | `freeze_deadline_hides_exactly_at_the_clock_in_every_arm`; `compact_expired_overlay_never_reveals_base`; `compact_delete_tombstone_shadows_base` |
| CLK-5 | Every other read is physical and ignores deadlines: `len`, `first_id`, `contains_id`, `row_of`, `id`, `revision`, `expiry`, `get_physical`, `effective_ids`, `effective_vector`, `effective_metadata`, `effective_payload` and the `read_*` methods. A caller that needs visibility asks `get_at` or `effective_visible` with its own `T` first. Counts (`len`) are physical and include hidden rows. | same; `freeze_accounting_reports_exact_values_and_keeps_peaks` |
| CLK-6 | A compact collection holds two expiry (and revision) representations: the resident `Collection.expiries` and `Collection.revisions` maps, and the view and overlay columns. They agree by construction (replay builds the overlay from the replayed resident maps; `apply_committed` writes both). Exact scans, browse and `ReadSnapshot` read the resident map; `get_at` and `effective_visible` (and so `rerank_effective`) read the columns. One operation reads one representation and never mixes them. | `freeze_resident_and_compact_expiry_sources_agree_after_mutations`; `compact_mutations_match_resident_put_delete_metadata_and_ttl` |
| CLK-7 | A public call that takes no clock samples the wall clock once and passes it down; a `ReadSnapshot` uses the clock it captured. The resident hybrid search is the one known exception (section 4). | `compact_browse_cursor_skips_tombstones_and_expiry`; `compact_exact_scan_is_bit_identical_at_same_clock` |

### 3.2 Overlay precedence and affected IDs

| ID | Statement | Pinned by |
|---|---|---|
| PRE-1 | The effective row of an ID is the overlay put if there is one, nothing if there is a tombstone, and the base row otherwise. A base row without an overlay entry is served from the pinned source, never from a cell. | `freeze_effective_rows_follow_overlay_precedence_in_ascending_order`; `compact_effective_accessor_pins_overlay_without_materializing_cells` |
| PRE-2 | `effective_ids` is ascending, has no duplicates and equals the base IDs, plus overlay puts, minus tombstones. | same |
| PRE-3 | The overlay is the complete difference between the pinned base and the effective state. Replay records an overlay put for every ID whose revision or deadline differs from its base row (a metadata-only write, a deadline-only change, a rewrite), a put for every ID absent from the base, and a tombstone, at the replay sequence, for every base ID that is gone. An unchanged ID has no entry. | `freeze_replay_overlay_covers_every_kind_of_change`; `compact_install_real_committed_suffix_replays_into_overlay` |
| PRE-4 | A pinned clone of the enum keeps the overlay root it was cloned with; later commits do not reach it. | `compact_effective_accessor_pins_overlay_without_materializing_cells`; `compact_overlay_snapshot_and_revision_condition_are_immutable` |
| PRE-5 | `overlay_logical_bytes` counts `record_body_len` for a put and 0 for a tombstone. | `freeze_effective_rows_follow_overlay_precedence_in_ascending_order` |

### 3.3 Errors

| ID | Statement | Pinned by |
|---|---|---|
| ERR-1 | Order of a read: bounded bytes, footer and checksum, authentication, envelope and extents, then exposure. A cache hit repeats the first three steps. | `compact_seam_authenticates_cached_vectors`; `freeze_cached_blocks_are_authenticated_again_and_shape_checked` |
| ERR-2 | A frame whose recorded kind or footer checksum changed, or whose range is outside the backing, is `Corrupt` with reason `compact source frame changed` or `compact source frame is out of bounds`, before any authentication. A frame body over `MAX_BODY + SEAL_OVERHEAD` is the first reason; a plain body over `MAX_BODY` is `compact source body exceeds limit`. A failed authentication is `Corrupt` with reason `frame authentication failed`. | `freeze_frame_tampering_is_an_error_on_every_read_path`; `freeze_frame_size_bound_is_exact` |
| ERR-3 | An extent outside the body is `Corrupt`: `compact payload extent is out of bounds`, `compact metadata extent is out of bounds`, `invalid compact vector extent` (a vector read) or `compact vector extent is out of bounds` (a cell), and undecodable metadata is `invalid compact record metadata`. | `freeze_extents_outside_the_body_are_corrupt_on_every_read_path`; `compact_seam_rejects_bad_extents_and_accounting_overflow` |
| ERR-4 | Errors are never converted into absence or a cache miss. `Ok(None)` means there is no such row, and every effective accessor propagates the error of a base read. | `freeze_frame_tampering_is_an_error_on_every_read_path` |
| ERR-5 | A refused read leaves the workspace unchanged: no block is accounted, no cell is published and the plaintext is released. | same |
| ERR-6 | A budget problem is `InvalidQuery` with a fixed reason: `compact workspace exceeds 4 MiB`, `compact scratch budget is smaller than one block`, `compact workspace counter exhausted`, `compact cached block shape changed` and the overflow, underflow and `cache accounting mismatch` reasons. A counter that would pass `u64::MAX` fails; it never wraps. | `freeze_budget_and_exact_fit_boundaries`; `freeze_cached_blocks_are_authenticated_again_and_shape_checked`; `compact_seam_lru_budget_and_accounting` |
| ERR-7 | A commit that succeeds but cannot be applied to the compact state poisons the handle and returns the error; the next operation answers `Poisoned`. | `committed_apply_errors_poison_compact_handle` |
| ERR-8 | A consumer checks the handle's health first, then (for a write) read-only, then validates its arguments, then reads. Lane B's `250a98bc` adds the read-only gate to the compact settings arm. | `compact_dispatch_*`; `250a98bc` (parked) |

### 3.4 Accounting

`SeamAccounting` has separate categories on purpose. None is a total-memory or RSS figure, and none supports a size
claim.

| ID | Statement | Pinned by |
|---|---|---|
| ACC-1 | `source_live` is the byte length of the pinned backing and `source_peak` the largest value `accounting` has seen in this workspace, across generations. Both are generation-wide. | `compact_seam_source_peak_survives_smaller_generation`; `freeze_accounting_reports_exact_values_and_keeps_peaks` |
| ACC-2 | `mapped_live` is the backing length of a mapped generation and 0 for an owned one; `mapped_peak` is kept like `source_peak`. | `compact_seam_mapped_source_accounting`; `freeze_mapped_accounting_keeps_its_own_peak` |
| ACC-3 | `cells_live` is the sum of `record_body_len` over the materialized cells of every collection of the generation, and `cells_peak` is kept like `source_peak`. Cells are never evicted. `read_vector`, `read_metadata`, `read_payload` and the effective accessors never create one. | `compact_seam_vector_metadata_leave_cells_empty`; `freeze_accounting_reports_exact_values_and_keeps_peaks` |
| ACC-4 | `vectors_live` is the decoded `f32` byte size of the cached blocks (`rows * dimensions * 4`, also for float16) and never exceeds the budget; `vectors_peak` is a running maximum updated at every insertion, so it survives an eviction. | `compact_seam_lru_budget_and_accounting`; `freeze_vector_peak_survives_eviction_by_a_smaller_block` |
| ACC-5 | `plaintext_live` is the length of the latest authenticated plaintext frame, 0 for plain generations, and 0 again after any failed read; `plaintext_peak` is a running maximum. | `compact_seam_vector_metadata_leave_cells_empty`; `compact_seam_authenticates_cached_vectors` |
| ACC-6 | `hits`, `misses` and `evictions` count per workspace. | `compact_seam_lru_budget_and_accounting` |
| ACC-7 | The budget is at most 4 MiB (exactly 4 MiB is allowed) and must hold one whole decoded block (a block of exactly the budget is read). A block that fits next to the live ones, exactly or with room to spare, evicts nothing. Eviction takes the least recently used block by a logical counter, ties broken by the cache key. | `freeze_budget_and_exact_fit_boundaries`; `compact_seam_lru_budget_and_accounting` |
| ACC-8 | `source_*`, `mapped_*` and `cells_*` peaks are sampled when `accounting` is called; `vectors_*` and `plaintext_*` peaks are maintained on every read. | `freeze_accounting_reports_exact_values_and_keeps_peaks`; `freeze_vector_peak_survives_eviction_by_a_smaller_block` |

### 3.5 Identity

| ID | Statement | Pinned by |
|---|---|---|
| IDN-1 | A `GenerationId` is a process-local counter value, minted once per `CompactGeneration` from a counter that starts at 1 and fails explicitly on exhaustion. It is never persisted, never compared across processes and never stands in for a lineage, a sequence or a digest. A reopen mints a new one. | `compact_seam_generation_pin_and_cache_identity`; `freeze_cache_entries_belong_to_one_collection_of_one_generation` |
| IDN-2 | A cached block is identified by `(generation, collection, kind, partition)`, where `kind` is the low 16 bits and `partition` the rest of the block's frame ID. Its shape (element count, dimensions, precision) is checked as well but is not an identity. | `compact_seam_partitions_have_distinct_cache_entries`; `freeze_cache_entries_belong_to_one_collection_of_one_generation`; `freeze_cache_key_separates_block_kinds` |
| IDN-3 | A view pins its generation until it is dropped; the workspace cache never does. | `compact_seam_generation_pin_and_cache_identity` |
| IDN-4 | A row ordinal is the position in ascending ID order across all blocks of the collection (`block start + row in block`), and IDs ascend strictly across blocks. | `compact_multiblock_point_view_matches_resident`; `freeze_accounting_reports_exact_values_and_keeps_peaks` |
| IDN-5 | At `v0.7.0` every block of every collection source is a complete-record block of kind `0x0101`. A second kind needs a review of `CacheKey` before it is admitted. | `freeze_cache_entries_belong_to_one_collection_of_one_generation` |

## 4. Deviations and limits that are not frozen

These are true at `v0.7.0`. They are not statements to preserve; the lane named in each row is expected to change them.

| Where | What | Lane |
|---|---|---|
| `hybrid.rs::search_hybrid` | The vector leg samples its own clock and the text leg samples the wall clock again for every candidate, so the legs can disagree at a deadline. Compact hybrid refuses, so only the resident path is affected. | A4 (`search_hybrid_at` in `02062058`) |
| `records.rs::effective_ids` | Builds a full ID set and vector. The 4 MiB workspace bounds vector scratch only, not ID memory. | ANN lane, via section 5 |
| Exact scan, browse, snapshot search | The compact path walks `compact_iter`, materializes a cell per visited row and reads deadlines from the resident map (CLK-6). | E4 |
| `Database::open_backed` | The loader decodes every record into a resident map before the generation is installed. | L5 |
| `CollectionRecords::len`, `first_id` | Walk the base rows and the overlay on every call. | not scheduled |
| Compact scan draft in `c859d2d5` (lane A) | Decodes metadata for every row even without a filter, and takes visibility from the columns. | E4 |

## 5. Additions for P3c (proposed)

[P3c section G](p3c-compact-ann.md#g-integration-seams-and-consumers) lists five things the ANN needs that the seam
lacks. This section gives them concrete, additive shapes. Existing signatures do not change. Nothing is implemented in
0.7.1 by R0.

| P3c name | At `v0.7.0` | Proposed |
|---|---|---|
| `len`, `contains_id`, `row_of`, `id_at`, `revision_at`, `expiry_at` | `CompactView::{len, contains_id, row_of, id, revision, expiry}` | Frozen as they are. |
| `vector_at`, `metadata_at` | `CompactView::{read_vector, read_metadata}` and the effective accessors | Frozen as they are. |
| `base_vector_at` | `CompactView::read_vector` already ignores the overlay and deadlines and returns the base row's canonical `f32` values | Frozen as it is; no new code. |
| `ordered_visible` | `compact_iter` | A4 may fix the cursor; the signature stays. |
| `base_rows` | A loop over `0..len()` | New, below. |
| `source_identity` | `CollectionRecords::source_identity` (process-local) | New portable binding, below. |
| `affected_since` | None (the overlay is private) | New, below. |
| `score_batch` | None | New, below. |

```rust
// src/compact/source.rs (source owner)
pub(crate) struct BaseRow { pub row: usize, pub id: u64, pub revision: u64, pub expiry: Option<u64> }
pub(crate) struct SourceBinding {
    pub lineage: u64, pub floor: u64, pub collection: u16,
    pub dimensions: u32, pub precision: Precision, pub digest: [u8; 32],
}
impl CompactView {
    /// Physical rows in ascending ID order, deadlines ignored, so rows hidden by expiry are included.
    pub(crate) fn base_rows(&self) -> impl ExactSizeIterator<Item = BaseRow> + '_;
    /// What the ANN binds to; computed once while the image is checked.
    pub(crate) fn binding(&self) -> &SourceBinding;
    /// One decoded block at a time through the workspace, in ascending ID order.
    pub(crate) fn for_each_vector_block<F>(&self, workspace: &mut ReadWorkspace, visit: F) -> Result<()>
    where F: FnMut(usize /* first row */, usize /* rows */, &[f32] /* rows * dimensions */) -> Result<()>;
}
// src/records.rs (records owner)
pub(crate) enum Affected { Put { revision: u64, expiry: Option<u64> }, Delete { revision: u64 } }
impl CollectionRecords {
    /// Overlay entries in ascending ID order, no allocation.
    pub(crate) fn affected_ids(&self) -> impl Iterator<Item = (u64, Affected)> + '_;
    /// Effective IDs in ascending order without materializing them: a merge of `base_rows` and `affected_ids`.
    pub(crate) fn ordered_ids(&self) -> impl Iterator<Item = u64> + '_;
}
```

- **`base_rows` (physical base).** Infallible and allocation-free, because ID, revision and deadline of every row are
  resident in the directory. It ignores deadlines (CLK-5), so an expired row that a checkpoint retained is still
  listed. It is the source for the ANN's content digest and for training.
- **`binding` (portable binding).** `lineage` and `floor` (the checkpoint sequence `S`) come from the file header and
  the image; `dimensions` and `precision` from the block layout. `digest` is `B` of P3c section D, a SHA-256 over the
  complete-record blocks in partition order. It is computed incrementally while `ImageCheck::block` holds each verified
  plaintext body, so it costs no extra authenticated read. `G` and `C` of section D derive from it, the physical rows
  and the vector bits. `GenerationId` stays the process-local pin (IDN-1) and is never part of a digest.
- **`affected_ids` (affected-ID cursor).** By PRE-3 the overlay is the complete difference between the base and the
  effective state, so for the ANN, which covers the base only, "affected since `S`" is the whole overlay. The entries
  keep the latest revision, which is enough: an ID changed twice appears once. Tombstones for IDs that the base never
  held are harmless and can be skipped. P3c's 256-ID, 8 MiB readiness bound is the ANN's, not the storage's: the ANN
  stops counting entries and invalidates readiness when it crosses.
- **`ordered_ids`.** Replaces the materializing `effective_ids` on the scan and ANN paths. `effective_ids` stays for
  derived-index rebuild and resident parity until its last caller moves.
- **`for_each_vector_block` (bounded batch).** Decodes one block through the same path as `read_vector`: it
  authenticates every visit, accounts the block under ACC-4, evicts under ACC-7 and never creates a cell. The visitor
  borrows one block at a time, so the live decoded bytes stay within the workspace budget. An error from the visitor or
  from a read stops the walk and is returned unchanged. Exact scans (E4) and the ANN's two-pass training and batch
  scoring through `exact_kernels::Scorer` use it instead of one `read_vector` call per row.

Tests and mutants required of whoever implements them, in addition to the statements above: an independent oracle for
the digest (P3c oracle fixtures), `base_rows` equal to the resident rows including an expired one, `affected_ids` and
`ordered_ids` against the resident oracle after metadata-only, deadline-only, delete, reinsert and duplicate-ID batches,
the 4 MiB bound and a tampered block under the batch walk, and mutants that drop a metadata-only entry, reverse the
order, skip the last row, count an expired row as absent and wrap a counter.

## 6. Single-writer boundaries

Reserve by function, not by file. A lane edits only the functions reserved for it. A needed change in another lane's
function goes to the coordinator, who routes it to that owner. New production code is additive; a signature in section 2
changes only through an amendment. Seam code stays under `cfg(test)` until L5 decides otherwise, and new tests go to
sibling files, not to the end of a test module that another lane's patch also extends.

Order of integration follows the readiness inventory: R0, then B4 and A4 (file-disjoint except where marked), then E4,
then C4, then L5, then the ANN lanes.

### `src/compact/source.rs`

One source owner at a time; nobody edits authentication or accounting in parallel.

| Region | R0 | B4 | A4 | E4 | C4 | L5 | ANN |
|---|---|---|---|---|---|---|---|
| Gate: `enabled`, `Activation` | frozen | | | | | un-gates | |
| `Capture`, `CompactGeneration::{new, decode, mapped_copy, view, has_collection}`, `GenerationId` | frozen | | | | | pin and retirement fields in a separate struct | computes `SourceBinding` while capturing |
| `ReadWorkspace`, `SeamAccounting`, `body` | frozen | | | | consumes `accounting` | separate counters struct | `for_each_vector_block` (additive) |
| `CompactView` read and physical methods | frozen | | | | | | `base_rows`, `binding` (additive) |
| Test module declarations at the end of the file | adds `freeze_tests` | | | | | | |

B4 and A4 make no production edit here: B4's only production hunks are three lines in `Database::set_collection_settings`
and a GPU preflight extraction.

### `src/records.rs`

Methods first, then files; never add a second effective-record iterator.

| Region | R0 | B4 | A4 | E4 | C4 | L5 | ANN |
|---|---|---|---|---|---|---|---|
| `CollectionRecords`, `OverlayEntry`, `RecordOverlay` (types) | frozen | | | | | | |
| Effective block: `pin_effective`, `effective_*`, `source_identity`, `source_accounting`, `overlay_logical_bytes` | frozen | tests only (`effective_rows_tests`, 28 lines in `250a98bc`) | | consumes | consumes | | `affected_ids`, `ordered_ids` (additive); `effective_ids` retired last |
| `len`, `is_empty`, `first_id`, `contains_key`, `get_at`, `get_physical`, `apply_committed`, `compact_overlay_revision`, `compact_from_replay` | frozen | | | | | | |
| `compact_iter`, `CompactRecordIter` | frozen | | cursor and seek | consumes | | | |
| `ReadSnapshot::{browse_at, search_with_stats_at}`, `compact_browse` | frozen | | | the call into the exact scan | | | |
| `CollectionRef::{search_grouped, facets}` compact arms | | | consumer arms (`02062058`) | | | | |

### `src/lib.rs`

One integrator owns the `Database` fields and constructors, `open_backed`, compact dispatch, the checkpoint swap and
`replica_extend`.

| Function | R0 | B4 | A4 | E4 | L5 |
|---|---|---|---|---|---|
| `Database` fields, `open_backed`, `compact_base_matches`, `discard_tail_inner`, `ensure_resident_in`, `ensure_mutable_in`, `ensure_all_resident`, `revision_in`, `apply` | frozen | | | | the one integrator |
| `set_collection_settings` compact arm | frozen | the read-only gate (`250a98bc`) | | | |
| `search_in_at` compact arm | frozen | | draft (`02062058`) | finalizes | |
| `exact_search_compact` (`c859d2d5` adds `exact_search_compact_with_workspace`) | frozen | | draft (`c859d2d5`) | sole owner | |
| `create_collection_as_occupied` and every non-compact function | out of scope | | | | |

Other shared surfaces, reserved by function as in the inventory: `collection.rs::{exact_search_candidates,
rebuild_derived}` (E4 and B4), `rerank.rs`, `metadata_index.rs`, `text_index.rs`, `quality.rs` (B4, then C4 consumes),
`kv.rs`, `hybrid.rs`, `query_language.rs`, `vectors.rs`, `property_graph/retrieve.rs` (A4, with K71 reserving `kv.rs`
and `server/kv.rs` functions), `compact/mod.rs::ImageCheck`, `format.rs`, `maintenance.rs`, `directory.rs`,
`changes.rs`, `logops.rs` (L5 and the storage owner).

## 7. The R0 mutation campaign

R0 adds tests only. To show that they are effective, and to give the next lanes a replayable record, it ran 86 hand
mutants of the seam: the 17 of the earlier seam campaign, rebased onto `v0.7.0`, and 69 new ones in five classes
(identity, authentication and bounded extents, expiry and overlay precedence, budget, accounting). Each mutant is one
anchored replacement in `src/compact/source.rs` (56), `src/records.rs` (29) or `src/quality.rs` (1). The list, with
each anchor and replacement, and the result of every run are in
[`tests/compact_seam_mutants.json`](../../../tests/compact_seam_mutants.json).

A mutant counts only when its anchor matches exactly once, the tree compiles, the selector executes, at least one named
test fails and the file is restored byte-identically (SHA-256 before and after). A compile error, a crash, a timeout or
a run that executes no test is recorded as such and never counts as a kill. A build holds exactly one mutant.

| Class | Mutants | Earlier campaign | New | Survive on `v0.7.0` | Killed on the final tree | Equivalent |
|---|---|---|---|---|---|---|
| identity | 21 | 6 | 15 | 1 | 21 | 0 |
| authentication | 18 | 2 | 16 | 11 | 18 | 0 |
| expiry | 14 | 2 | 12 | 5 | 14 | 0 |
| budget | 11 | 1 | 10 | 4 | 11 | 0 |
| accounting | 22 | 6 | 16 | 5 | 22 | 0 |
| **Total** | **86** | **17** | **69** | **26** | **86** | **0** |

Replays, all with `cargo test --locked --offline --lib -- compact::source records:: metadata_index::tests::compact rerank::`:

- **Baseline**, `v0.7.0` (`02ef3e5f`), 75 tests, green before the first mutant. Sixty mutants fail an existing test.
  Twenty-six survive, so those are the places where replay evidence was missing.
- **Final**, `71129c2a` (`v0.7.0` plus the R0 tests), 92 tests, green before the first mutant and again after the last.
  All 86 mutants fail at least one named test; none fails to compile, crashes or times out, and every file restores
  byte-identically.
- **Independent**, `af6dc340` (the first 15 of the 17 tests), on separate machines in six shards: 84 of the 86 mutants
  (all but `R0-AU-15` and `R0-AU-16`, which were written for the last two tests) were killed by the same `freeze_*`
  tests and restored byte-identically.

The 26 gaps and what closes them:

| Mutant | What it does | Killed by |
|---|---|---|
| `R0-AU-01` | drops the frame-kind guard from the bounded body read | `freeze_frame_tampering_is_an_error_on_every_read_path` |
| `R0-AU-02` | drops it from record-cell materialization | same |
| `R0-AU-03` | drops the footer checksum guard from the bounded body read | same, and `freeze_cached_blocks_are_authenticated_again_and_shape_checked` |
| `R0-AU-08` | `read_payload` reads an out-of-bounds extent as an empty payload | `freeze_extents_outside_the_body_are_corrupt_on_every_read_path` |
| `R0-AU-09` | `read_metadata` reads one as empty metadata, still `Corrupt` but with another reason | same |
| `R0-AU-11` | removes the cached-block shape guard | `freeze_cached_blocks_are_authenticated_again_and_shape_checked` |
| `R0-AU-12` | `effective_vector` turns a base-row read error into no row | `freeze_frame_tampering_is_an_error_on_every_read_path` |
| `R0-AU-13` | the same for `effective_metadata` | same |
| `R0-AU-14` | the same for `effective_payload` | same |
| `R0-AU-15` | the bounded body read treats a frame range outside the backing as an empty frame | `freeze_frame_range_outside_the_backing_is_corrupt` |
| `R0-AU-16` | the same for record-cell materialization | same |
| `R0-EX-05` | an expired overlay put is visible to `effective_visible` | `freeze_deadline_hides_exactly_at_the_clock_in_every_arm`, `freeze_resident_and_compact_expiry_sources_agree_after_mutations` |
| `R0-EX-06` | a base row whose deadline equals the clock stays visible to `effective_visible` | `freeze_deadline_hides_exactly_at_the_clock_in_every_arm` |
| `R0-EX-07` | the resident arm of `effective_visible` hides one tick late | same |
| `R0-AF-02` | replay misses an ID whose deadline changed (a TTL-only write) | `freeze_replay_overlay_covers_every_kind_of_change` |
| `R0-EF-02` | `effective_payload` lets an overlay tombstone fall through to the base payload | `freeze_effective_rows_follow_overlay_precedence_in_ascending_order` |
| `R0-BU-07` | the frame-size bound of a bounded body read is inclusive | `freeze_frame_size_bound_is_exact` |
| `R0-BU-08` | that bound is dropped | same |
| `R0-BU-09` | the decoded-body bound after authentication is dropped | same |
| `R0-BU-10` | that bound is inclusive | same |
| `R0-ID-04` | the cache key ignores the block kind | `freeze_cache_key_separates_block_kinds` |
| `R0-AC-02` | `mapped_peak` is overwritten instead of kept as a maximum | `freeze_mapped_accounting_keeps_its_own_peak` |
| `R0-AC-03` | each materialized cell counts as one byte | `freeze_accounting_reports_exact_values_and_keeps_peaks` |
| `R0-AC-04` | `cells_peak` is overwritten | same |
| `R0-AC-05` | cell accounting covers the view's collection, not the whole generation | same |
| `R0-AC-08` | `vectors_peak` is overwritten | `freeze_vector_peak_survives_eviction_by_a_smaller_block` |

Seven of lane B's 18 open obligations are mutations of this seam (the [ledgers](p3b-survivor-ledgers.md#lane-b)). R0
re-created each one as `R0-AU-08`, `R0-AU-13`, `R0-AU-14`, `R0-EF-02`, `R0-EX-05`, `R0-EX-06` and `R0-EX-07`; all seven
survive on `v0.7.0` and all seven are killed with the freeze tests.

**Equivalents: none.** One mutant needs a note. `R0-ID-04` changes behavior only for a block whose kind is not
`0x0101`, and no writer produces such a block at `v0.7.0` (IDN-5). It is killed by a synthetic fixture that gives a
second block another kind, and it is counted as killed because the identity it protects is the one a second kind would
break. The minimum of 12 gaps closed does not depend on it. `R0-BU-09` and `R0-BU-10` are reachable only for a plain
frame whose body is between `MAX_BODY` and `MAX_BODY + SEAL_OVERHEAD` bytes, which `freeze_frame_size_bound_is_exact`
builds.

**What the campaign does not show.** It shows that the tests pin the statements in section 3, not that the statements
are complete, and it supports no size or speed claim. Anchors are valid on the trees named above; a lane that changes
the lines around one re-bases it and replays the mutant, and does not trust the recorded result.

## 8. Decisions requested

- **OPEN-71-02, survivor evidence.** Accept the evidence form in the [ledgers](p3b-survivor-ledgers.md#evidence-that-would-settle-open-71-02):
  one record per mutant with before and after SHA-256, the failing test names and a restored-green run, replayed
  independently. R0's ledger has that shape.
- **OPEN-71-03, seam freeze.** Approve sections 3 and 4 as frozen, and section 5 as the additive shapes of the five
  ANN operations. Recommended order: land `base_rows`, `affected_ids` and `ordered_ids` before E4, because the exact scan
  needs an ID source that does not materialize; leave `binding` and `for_each_vector_block` to the ANN lane.
- **OPEN-71-04, J's moved tests.** Ratify that the HNSW and snapshot names (`compact_hnsw_rerank_uses_pinned_generation`,
  `compact_ann_snapshot_rejects_other_generation`, `compact_rebuild_snapshot_survives_drop_and_refused_checkpoint`) move
  to P3c, that slice 4 proves the generic pin and the refusal with the tests that exist
  (`compact_metadata_text_indexes_verify_overlay`, `compact_ann_paths_remain_refused_with_unchanged_state`), and that
  `compact_server_materializes_only_results` stays with C4.
- **OPEN-71-14, campaign sizes.** Keep the inventory's minima and every historical survivor, and count a mutant only
  when it compiles, runs, fails a named test and restores byte-identically. R0 shows that 69 new mutants over one seam
  are practical.

## 9. Verification

```text
cargo test --locked --offline --lib -- compact::source records:: metadata_index::tests::compact rerank::
cargo test --locked --offline --all-features --lib -- compact
cargo fmt --all -- --check
cargo clippy --locked --offline --workspace --all-targets --all-features -- -D warnings
python tests/compact_method_matrix.py
```

The mutant list replays with any runner that applies one anchored replacement at a time (LF-normalized match, line
endings preserved on write), runs `command`, classifies the outcome and restores the file; the ledger records the
selector, the failing tests and the SHA-256 of the file before, during and after.
