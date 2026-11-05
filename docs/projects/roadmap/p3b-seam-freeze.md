# P3b read seam freeze for 0.7.1 (lane R0)

Status: written at the start of 0.7.1 from `v0.7.0` (`02ef3e5f`) and approved on 2026-11-05 (the decisions OPEN-71-01 to OPEN-71-04
and OPEN-71-14, stated in section 8, were taken by the coordinator under the owner's delegation; the OPEN-71 identifiers are
those of the storage readiness inventory behind the [0.7.1 plan](handover-0.7.0.md#071)).
Sections 3 and 4 describe what the code does today and pin it with tests. Section 3 is frozen for the lanes that follow:
B4, A4, E4, C4, K71, L5 and the ANN, P4 and P5 lanes. Section 4 is the approved inventory of deviations and limits: it
is not frozen, and a lane may remove any of them. Section 5 gives additive shapes, approved as additive: lane R0's
slice 3 implemented `base_rows`, `affected_ids` and `ordered_ids` (crate-private, under the seam's test gate; statements
BAS, AFF and ORD in section 3.7), and `binding` and `for_each_vector_block` stay with the ANN lane. Nothing here changes
behavior, a file format or the wire, activates a compact writer, float16 or temporal copy-out, or supports a size or
speed claim. Companion pages: the [survivor ledgers](p3b-survivor-ledgers.md) of the parked lanes, the
[P3 design](p3-compact-vectors.md) and the [P3c design](p3c-compact-ann.md).

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
`ReadWorkspace` holds the operation-local scratch. P3c's `ReadAccess` is a name for these three together. The functions
marked new were added by slice 3 of 0.7.1; every other function is the one at `v0.7.0`, unchanged.

| Piece | Functions | Meaning |
|---|---|---|
| `CompactGeneration` (`compact/source.rs`) | `Capture::{push, finish}`, `new`, `view`, `has_collection`, `decode` | One immutable, authenticated image: owned or mapped bytes, the block directory, one lazy `Record` cell per row, the cipher context. Built once, after the whole image validated. |
| `CompactView` | `generation_id`, `len`, `first_id`, `contains_id`, `row_of`, `id`, `revision`, `expiry`, `base_rows` (new), `get`, `get_at`, `get_physical`, `read_vector`, `read_metadata`, `read_payload` | Physical rows of one collection, in ascending ID order. `get` and `get_at` apply a deadline, every other accessor is physical. `base_rows` lists every physical row from the resident directory. `get`, `get_at` and `get_physical` decode a row into its cell once and serve the cell afterwards. The `read_*` methods copy out of one authenticated frame on every call and never create a cell. |
| `ReadWorkspace` | `new`, `accounting`, `body` (private) | At most 4 MiB of decoded vectors, the latest authenticated plaintext frame and the counters. One per operation, serial, never shared and never persisted. |
| `CollectionRecords` (`records.rs`) | `len`, `is_empty`, `first_id`, `contains_key`, `get_at`, `get_physical`, `apply_committed`, `compact_iter`, `compact_overlay_revision`, `compact_from_replay`, `pin_effective`, `effective_ids`, `effective_vector`, `effective_metadata`, `effective_payload`, `effective_visible`, `source_identity`, `source_accounting`, `overlay_logical_bytes`, `affected_ids` (new), `ordered_ids` (new) | The effective view: overlay first, then the base. `pin_effective` clones the enum, which pins the generation and the overlay root together. `get_at`, `get_physical` and `compact_iter` hand out records, so they materialize base rows into cells; the `effective_*` accessors never do. `affected_ids` lists the overlay entries and `ordered_ids` the effective IDs, neither reads a block. |
| Consumers (test-gated) | `Collection::rebuild_derived`, `MetadataIndexes::rebuilt_effective`, `TextIndex::rebuilt_effective`, `rerank_effective`, `quality::effective_memory`, the compact arm of `Database::set_collection_settings` | Read through the effective view with a 4 MiB workspace and, where they test visibility, an explicit clock. |

## 3. Frozen semantics

Each statement has an ID so that a test, a mutant and a later change can cite it. A change to a statement needs an
amendment to this page, a test that cites the new text and an updated mutant ledger. The tests named here are the
pinning ones; `freeze_*` tests live in `src/compact/source/freeze_tests.rs`. A dagger marks a detail that was verified
by reading the code (at `v0.7.0`, and for section 3.7 at the head of slice 3) and has no test of its own.

### 3.1 The effective-read clock

| ID | Statement | Pinned by |
|---|---|---|
| CLK-1 | Every deadline decision in the seam takes the caller's clock `T` (milliseconds since the Unix epoch): `src/compact/source.rs` never reads a clock of its own, and the effective accessors and consumers receive `T` as an argument and pass the one clock to every read of the operation. | `freeze_source_never_reads_the_wall_clock` (source.rs only); the explicit `now_ms` arguments of `get_at`, `effective_visible`, `rerank_effective` and the `*_at` scans |
| CLK-2 | A row is hidden when its deadline `<= T`, so a deadline equal to `T` hides it and `T = deadline - 1` shows it. | `freeze_deadline_hides_exactly_at_the_clock_in_every_arm`; `compact_view_get_matches_resident_after_source_drop` |
| CLK-3 | A row with no deadline is never hidden by the clock. | same |
| CLK-4 | The newest overlay entry decides. An overlay put uses its own deadline, so an expired put hides the ID and never reveals the older base row; a tombstone hides the ID at every `T` and never falls through. | `freeze_deadline_hides_exactly_at_the_clock_in_every_arm`; `compact_expired_overlay_never_reveals_base`; `compact_delete_tombstone_shadows_base` |
| CLK-5 | Every read that takes no clock is physical and ignores deadlines: `len`, `first_id`, `contains_id`, `row_of`, `id`, `revision`, `expiry`, `base_rows`, `get_physical`, `effective_ids`, `ordered_ids`, `effective_vector`, `effective_metadata`, `effective_payload` and the `read_*` methods. The point readers that take a clock are `get`, `get_at` and `effective_visible`; a caller that needs visibility asks one of them with its own `T` first. Counts (`len`) are physical and include hidden rows. | same; `freeze_accounting_reports_exact_values_and_keeps_peaks`; section 3.7 for `base_rows` and `ordered_ids` |
| CLK-6 | The expiry and revision of a compact row exist in two places: the resident `Collection.expiries` and `Collection.revisions` maps, and the view and overlay columns. They agree at every replay and every commit (replay builds the overlay from the replayed resident maps; `apply_committed` writes both). Which reader uses which representation is not frozen (section 4). | `freeze_resident_and_compact_expiry_sources_agree_after_mutations`; `compact_mutations_match_resident_put_delete_metadata_and_ttl` |
| CLK-7 | A public call that takes no clock samples the wall clock once on entry and passes it down, and a `ReadSnapshot` uses the clock it captured.† Four resident-only paths are known exceptions that sample per candidate or per call: the hybrid text leg, query-language execution, `ann_quality` and `search_ready_with_options_in` (section 4). Other callers that sample per candidate were not audited. The tests pin the explicit-clock half only: one clock passed down gives one answer. | `compact_browse_cursor_skips_tombstones_and_expiry`; `compact_exact_scan_is_bit_identical_at_same_clock` |

### 3.2 Overlay precedence and affected IDs

| ID | Statement | Pinned by |
|---|---|---|
| PRE-1 | The effective row of an ID is the overlay put if there is one, nothing if there is a tombstone, and the base row otherwise. The `effective_*` accessors serve a base row without an overlay entry from the pinned source and never create a cell; `get_at`, `get_physical` and `compact_iter` return records and so materialize base rows into cells (ACC-3). | `freeze_effective_rows_follow_overlay_precedence_in_ascending_order`; `compact_effective_accessor_pins_overlay_without_materializing_cells` |
| PRE-2 | `effective_ids` is ascending, has no duplicates and equals the base IDs, plus overlay puts, minus tombstones. | same |
| PRE-3 | The overlay is the complete difference between the pinned base and the effective state. Replay records an overlay put for every ID whose revision or deadline differs from its base row (a metadata-only write, a deadline-only change, a rewrite), a put for every ID absent from the base, and a tombstone, at the replay sequence, for every base ID that is gone. An unchanged ID has no entry. | `freeze_replay_overlay_covers_every_kind_of_change`; `compact_install_real_committed_suffix_replays_into_overlay` |
| PRE-4 | A pinned clone of the enum keeps the overlay root it was cloned with; later commits do not reach it. | `compact_effective_accessor_pins_overlay_without_materializing_cells`; `compact_overlay_snapshot_and_revision_condition_are_immutable` |
| PRE-5 | `overlay_logical_bytes` counts `record_body_len` for a put and 0 for a tombstone. | `freeze_effective_rows_follow_overlay_precedence_in_ascending_order` |
| PRE-6 | Each compact collection has its own view and overlay: a commit to one collection changes the length and the effective IDs of that collection only. | `freeze_named_collections_keep_one_overlay_each` |

### 3.3 Errors

| ID | Statement | Pinned by |
|---|---|---|
| ERR-1 | A workspace read (`read_vector`, `read_metadata`, `read_payload`) runs in this order. An absent row answers `Ok(None)`. A vector read then checks sizes and refuses a block that exceeds the scratch budget, before any frame is read. The frame read follows: the previous plaintext is released, the frame range, size, kind and footer-checksum checks run, an encrypted frame is authenticated and the body bound is checked. The extents come next and exposure last. A vector cache hit repeats the frame read, authentication included, and then checks the cached block's shape; it does not decode again. A record cell (`get`, `get_at`, `get_physical`) runs the range, kind and checksum checks and the authentication once, when it first decodes its row, and is served from memory afterwards. What a refused read leaves behind is section 4's limit, not a frozen order. | `compact_seam_authenticates_cached_vectors`; `freeze_cached_blocks_are_authenticated_again_and_shape_checked`; `freeze_a_refused_read_leaves_the_rest_of_the_workspace_as_it_was` |
| ERR-2 | A frame whose recorded kind or footer checksum changed, or whose range is outside the backing, is `Corrupt` with reason `compact source frame changed` or `compact source frame is out of bounds`, before any authentication, on the workspace reads and on cell decoding alike. On the workspace reads a frame body over `MAX_BODY + SEAL_OVERHEAD` is the first reason, and a plain body over `MAX_BODY` is `compact source body exceeds limit`; cell decoding applies neither bound. An encrypted frame is then checked for its footer magic, its sequence and a CRC over the stored bytes (`Corrupt` with reason `commit footer or record checksum mismatch`),† and only then opened: a failed authentication is `Corrupt` with reason `frame authentication failed`. | `freeze_frame_tampering_is_an_error_on_every_read_path`; `freeze_frame_range_outside_the_backing_is_corrupt`; `freeze_frame_size_bound_is_exact` |
| ERR-3 | An extent outside the body is `Corrupt`: `compact payload extent is out of bounds`, `compact metadata extent is out of bounds`, `invalid compact vector extent` (a vector read) or `compact vector extent is out of bounds` (a cell), and undecodable metadata is `invalid compact record metadata`.† | `freeze_extents_outside_the_body_are_corrupt_on_every_read_path`; `compact_seam_rejects_bad_extents_and_accounting_overflow` |
| ERR-4 | Errors are never converted into absence or a cache miss. `Ok(None)` means there is no such row, and every effective accessor propagates the error of a base read. | `freeze_frame_tampering_is_an_error_on_every_read_path`; `freeze_budget_and_exact_fit_boundaries` (the budget refusal of `effective_vector`) |
| ERR-5 | A refused read caches no vector block and publishes no cell. When it fails while reading its frame (range, size, kind, checksum or authentication) it leaves no plaintext (`plaintext_live` is 0), and the blocks cached before it stay when no eviction was needed. A refused read is not atomic in other respects (section 4). | `freeze_a_refused_read_leaves_the_rest_of_the_workspace_as_it_was`; `freeze_frame_tampering_is_an_error_on_every_read_path`; `freeze_extents_outside_the_body_are_corrupt_on_every_read_path` |
| ERR-6 | A budget problem is `InvalidQuery` with a fixed reason: `compact workspace exceeds 4 MiB`, `compact scratch budget is smaller than one block` and `compact cached block shape changed` (pinned), and `compact workspace counter exhausted`, the vector-size, accounting overflow and underflow reasons and `compact cache accounting mismatch`.† A counter that would pass `u64::MAX` fails; it never wraps.† | `freeze_budget_and_exact_fit_boundaries`; `freeze_cached_blocks_are_authenticated_again_and_shape_checked`; `compact_seam_lru_budget_and_accounting` |
| ERR-7 | A commit that succeeds but cannot be applied to the compact state poisons the handle and returns the error; the next operation answers `Poisoned`. | `committed_apply_errors_poison_compact_handle` |

### 3.4 Accounting

`SeamAccounting` has separate categories on purpose. None is a total-memory or RSS figure, and none supports a size
claim.

| ID | Statement | Pinned by |
|---|---|---|
| ACC-1 | `source_live` is the byte length of the pinned backing and `source_peak` the largest value `accounting` has seen in this workspace, across generations. Both are generation-wide. | `compact_seam_source_peak_survives_smaller_generation`; `freeze_accounting_reports_exact_values_and_keeps_peaks` |
| ACC-2 | `mapped_live` is the backing length of a mapped generation and 0 for an owned one; `mapped_peak` is kept like `source_peak`. | `compact_seam_mapped_source_accounting`; `freeze_mapped_accounting_keeps_its_own_peak` |
| ACC-3 | `cells_live` is the sum of `record_body_len` over the materialized cells of every collection of the generation, and `cells_peak` is kept like `source_peak`. Cells are never evicted. `read_vector`, `read_metadata`, `read_payload` and the effective accessors never create one. | `compact_seam_vector_metadata_leave_cells_empty`; `freeze_accounting_reports_exact_values_and_keeps_peaks` |
| ACC-4 | `vectors_live` is the decoded `f32` byte size of the cached blocks (`rows * dimensions * 4`, also for float16†) and never exceeds the budget; `vectors_peak` is a running maximum updated at every insertion, so it survives an eviction. | `compact_seam_lru_budget_and_accounting`; `freeze_vector_peak_survives_eviction_by_a_smaller_block` |
| ACC-5 | `plaintext_live` is the length of the latest authenticated plaintext frame and 0 for plain generations; `plaintext_peak` is a running maximum. A read releases the previous plaintext when it starts reading its frame, so a read that fails while reading its frame leaves 0 (the two other refusals are in section 4). | `compact_seam_vector_metadata_leave_cells_empty`; `compact_seam_authenticates_cached_vectors`; `freeze_plaintext_peak_survives_a_smaller_frame`; `freeze_a_refused_read_leaves_the_rest_of_the_workspace_as_it_was`; `freeze_frame_tampering_is_an_error_on_every_read_path` |
| ACC-6 | `hits`, `misses` and `evictions` count per workspace. | `compact_seam_lru_budget_and_accounting` |
| ACC-7 | The budget is at most 4 MiB (exactly 4 MiB is allowed) and must hold one whole decoded block (a block of exactly the budget is read). A block that fits next to the live ones, exactly or with room to spare, evicts nothing. Eviction takes the least recently used block: the one with the smallest value of a logical counter that advances on every vector read that passes the budget check and is stamped on the block that read touches. The values are distinct, so the code's cache-key tie-break is never reached.† | `freeze_budget_and_exact_fit_boundaries`; `compact_seam_lru_budget_and_accounting` (the one that tells least recently used from first in, first out) |
| ACC-8 | `source_*`, `mapped_*` and `cells_*` peaks are sampled when `accounting` is called; `vectors_*` and `plaintext_*` peaks are maintained on every read. | `freeze_accounting_reports_exact_values_and_keeps_peaks`; `freeze_vector_peak_survives_eviction_by_a_smaller_block`; `freeze_plaintext_peak_survives_a_smaller_frame` |

### 3.5 Identity

| ID | Statement | Pinned by |
|---|---|---|
| IDN-1 | A `GenerationId` is a process-local counter value, minted once per `CompactGeneration` from a counter that starts at 1 and fails explicitly on exhaustion.† It is never persisted, never compared across processes and never stands in for a lineage, a sequence or a digest. A reopen mints a new one. | `compact_seam_generation_pin_and_cache_identity`; `freeze_cache_entries_belong_to_one_collection_of_one_generation` |
| IDN-2 | A cached block is identified by `(generation, collection, kind, partition)`, where `kind` is the low 16 bits and `partition` the rest of the block's frame ID. Its shape (element count, dimensions, precision) is checked as well but is not an identity. | `compact_seam_partitions_have_distinct_cache_entries`; `freeze_cache_entries_belong_to_one_collection_of_one_generation`; `freeze_cache_key_separates_block_kinds` |
| IDN-3 | A view pins its generation until it is dropped; the workspace cache never does. | `compact_seam_generation_pin_and_cache_identity` |
| IDN-4 | A row ordinal is the position in ascending ID order across all blocks of the collection (`block start + row in block`), and IDs ascend strictly across blocks. | `compact_multiblock_point_view_matches_resident`; `freeze_accounting_reports_exact_values_and_keeps_peaks` |
| IDN-5 | At `v0.7.0` every block of every collection source is a complete-record block of kind `0x0101`. A second kind needs a review of `CacheKey` before it is admitted. | `freeze_cache_entries_belong_to_one_collection_of_one_generation` |

### 3.6 Concurrency

| ID | Statement | Pinned by |
|---|---|---|
| CON-1 | A `CompactView` moves to other threads and its pinned `CompactGeneration` is shared by them: the first access to a record cell decodes it once under the cell's lock, and every reader shares that cell. A `ReadWorkspace` is plain owned scratch of one operation, used through `&mut self`: it is never shared between operations and never used by two threads at once. | `compact_concurrent_first_access_decodes_once_under_lock` |

### 3.7 Physical base, affected IDs and ordered IDs

Added by slice 3 of 0.7.1 and frozen from the day it landed: `CompactView::base_rows`, `CollectionRecords::affected_ids`
and `CollectionRecords::ordered_ids`. All three are crate-private and compile only under `cfg(test)`, like the rest of
the seam. `effective_ids` is unchanged. The tests are in `src/compact/source/ordered_tests.rs`; their expectations are
literal rows, the resident decode of the same image and a model that the test keeps next to every commit it applies,
never the code under test.

| ID | Statement | Pinned by |
|---|---|---|
| BAS-1 | `base_rows` yields one `BaseRow { row, id, revision, expiry }` for every physical row of the pinned collection, in ascending row order. IDs ascend strictly across all blocks (IDN-4), and `row` is the ordinal that `row_of(id)` answers. | `ordered_base_rows_list_every_physical_row_in_ascending_id_order`; `ordered_base_rows_cross_every_block_and_read_nothing` |
| BAS-2 | It lists the base only: it ignores the overlay and every deadline (CLK-5), so a row that a deadline-aware reader hides is listed with its deadline, and `id`, `revision` and `expiry` equal the point accessors of the same row and the resident decode of the same image. | `ordered_base_rows_list_every_physical_row_in_ascending_id_order` |
| BAS-3 | It reads only the resident directory. It cannot fail, allocates nothing,† never reads or authenticates a frame,† never touches the workspace and never decodes a record or creates a cell, so no hit, miss, eviction, vector, plaintext or cell counter of `SeamAccounting` and no decode counter moves. The tests walk all rows with every frame pointed outside the backing, where any frame read fails, and on readable frames of an untouched generation, where a decode would leave a cell behind. | `ordered_base_rows_cross_every_block_and_read_nothing`; `ordered_ids_ignore_deadlines_and_read_no_block` |
| BAS-4 | It is an exact-size, fused and cloneable iterator (a compile-time check): `len` and `size_hint` are the rows not yet yielded, after the last row `next` answers `None` for good, and a clone walks on its own from where it was taken. A view of a collection that the generation does not hold yields nothing. | `ordered_base_rows_list_every_physical_row_in_ascending_id_order`; `ordered_base_rows_cross_every_block_and_read_nothing` |
| AFF-1 | `affected_ids` yields one `(id, Affected)` for every overlay entry, in ascending ID order, without allocating.† `Put { revision, expiry }` carries the overlay put's own revision and deadline. `Delete { revision }` is a tombstone and carries the sequence at which the overlay learned of the delete: the replay sequence for a replayed delete, the commit's revision for a committed one. A resident collection has no base and no overlay and yields nothing. | `ordered_ids_and_affected_ids_follow_every_kind_of_change`; `ordered_ids_over_a_collection_without_base_rows`; `ordered_ids_resident_variant_lists_its_keys_and_no_overlay` |
| AFF-2 | The overlay holds at most one entry per ID, the newest, so an ID changed several times appears once with its latest revision and deadline. It is the difference between the pinned base and the effective state (PRE-3). A reopen replays the final state over the same base: it holds a put for every ID that is new or whose revision or deadline changed, a tombstone at the replay sequence for every base ID that is gone, and nothing for an unchanged ID. An ID that is put and deleted again after an image that never held it is therefore a tombstone while the committing handle is open and has no entry after a reopen. The tests derive the reopened entries from the base rows and the final state alone. | `ordered_ids_and_affected_ids_follow_every_kind_of_change`; `ordered_ids_match_a_seeded_model_over_random_commits`; `ordered_ids_over_a_collection_without_base_rows`; `freeze_replay_overlay_covers_every_kind_of_change` (the replay itself) |
| ORD-1 | `ordered_ids` yields the effective IDs in ascending order without duplicates: the base IDs, plus the overlay puts, minus the tombstones (PRE-2). An ID that the base holds and the overlay puts is listed once, and a tombstone for an ID that the base never held lists nothing, whether the base rows above it are listed or already exhausted. After every operation of the model tests it equals the sorted `effective_ids` and the keys of the resident oracle, and after a reopen of the final state it lists the same IDs. It also equals the IDs of `compact_iter`, from the start and after seek points, after every operation of the scenario tests and every 15th of the seeded stream: every ID of a small set and its neighbours, and for the 8,195-row set the first, the last, every 2,000th ID and both sides of each block boundary. | `ordered_ids_and_affected_ids_follow_every_kind_of_change`; `ordered_ids_match_a_seeded_model_over_random_commits`; `ordered_ids_over_a_collection_without_base_rows` |
| ORD-2 | It applies no deadline, like `effective_ids` (CLK-5): an expired base row and an expired overlay put are both listed. It reads no block, authenticates nothing,† decodes no record and creates no cell, so it works while every frame is unreadable and moves no counter. The tests check this on readable frames of an untouched generation, with overlay entries over base rows (the decode counter and the cell accounting stay as they were), and with every frame unreadable (no decode is attempted); the model tests also check the decode counter around every call until their record cursor has decoded every row. | `ordered_ids_ignore_deadlines_and_read_no_block`; the model tests |
| ORD-3 | A pinned clone (`pin_effective`) keeps the overlay root it was cloned with: its `ordered_ids` and `affected_ids` equal what the model held when the pin was taken, immediately and after later commits (PRE-4). | `ordered_ids_and_affected_ids_follow_every_kind_of_change` |
| ORD-4 | On a resident collection `ordered_ids` yields its keys in ascending order, also after writes, and `affected_ids` yields nothing. | `ordered_ids_resident_variant_lists_its_keys_and_no_overlay` |
| ORD-5 | Over a view that has no base rows (the image held the collection with no rows, so the generation has no source for it) the overlay alone decides: puts are listed in ascending order, tombstones are not, and a reopen holds no entry for a tombstoned ID. | `ordered_ids_over_a_collection_without_base_rows` |

## 4. Deviations and limits that are not frozen

These are true at `v0.7.0`, except the rows that name slice 3, which describe the head of 0.7.1. They are not statements
to preserve; the lane named in each row is expected to change them.

| Where | What | Lane |
|---|---|---|
| `hybrid.rs::search_hybrid`, `query_language.rs::execute`, `quality.rs::ann_quality`, `lib.rs::search_ready_with_options_in` | The resident vector leg samples its own clock, and the hybrid text leg (`hybrid.rs:197`) and query-language execution (`query_language.rs:422-452`) call `get` per candidate, so each candidate is judged at its own wall-clock sample and the legs can disagree at a deadline. `ann_quality` samples once per sampled query (`quality.rs:227`) and `search_ready_with_options_in` once per call (`lib.rs:3720`). All four read the resident map, so they refuse compact records today. | A4 (`search_hybrid_at` and the query-language edits in `02062058`); the ANN lane for the other two |
| `records.rs::effective_ids` | Builds a full ID set and vector. The 4 MiB workspace bounds vector scratch only, not ID memory. `ordered_ids` (slice 3) lists the same IDs lazily. | its callers move to `ordered_ids` one by one; `effective_ids` stays until the last one has moved |
| `CompactView::base_rows` and `CollectionRecords::ordered_ids` (slice 3), `compact_iter(after)` | None of the three seeks. `base_rows` and `ordered_ids` start at the first row, and `compact_iter(after)` finds its start by counting the base rows up to `after` (`records.rs`), so a walk that resumes after an ID pays for every row before it. `Affected` carries the overlay's revision and deadline, not its metadata or payload. | A4 (`compact_iter(after)`); E4 (a seek in the new cursors, if its scan needs one: an additive edit of `base_rows` and `ordered_ids`, section 6) |
| Exact scan, browse, snapshot search | The compact path walks `compact_iter`, materializes a cell per visited row and reads deadlines from the resident map (CLK-6). | E4 |
| Expiry and revision readers | Exact scans, browse and `ReadSnapshot` read the resident expiry map, and `get_at`, `effective_visible` (and so `rerank_effective`) read the view and overlay columns (CLK-6). `revision_in`, which serves `revision()`, the server and graph retrieve, answers from the overlay when it holds an entry for the ID and from the resident map otherwise, and a conditional write (`put_with_expiry_in`, `put_in`, `delete_in`, `update_metadata_in` and `apply_reembed_batch_checked`) checks the resident map alone.† A lane may move a reader to the other representation if it keeps one clock per operation and the agreement of the two. | E4, L5 |
| `Database::open_backed` | The loader decodes every record into a resident map before the generation is installed. | L5 |
| `CollectionRecords::len`, `first_id` | `len` walks the overlay on every call, and `first_id` scans the base from row 0 past tombstoned rows and then the overlay. | not scheduled |
| Refused reads are not atomic | `read_vector` counts a miss, advances the logical clock and evicts to make room before it reads the frame, so a read that then fails can leave a block evicted. A read refused by the scratch budget keeps the previous plaintext, and an encrypted read that fails after authentication (an extent or decode error, or the shape refusal on a hit) keeps that frame's plaintext until the next read. A consumer must not rely on a rollback. | E4, L5 |
| Plain frames | A read of a plain generation compares only the footer's stored checksum field with the one recorded at capture. It does not recompute the checksum over the body, so a mapped plain frame whose bytes change under an unchanged footer is served as it is. An encrypted frame is authenticated in full on every workspace read and on the first decode of a cell. | L5 |
| Record cells | A materialized cell is never read or authenticated again and never evicted, and every committed write to a base ID creates that row's cell, because `Collection::apply_committed` reads the previous record through `get_physical`. Cell decoding has no frame-size or body bound and does not check a vector extent's width. | L5, E4 |
| Compact settings arm | The two arms of `Database::set_collection_settings` check in different orders at `v0.7.0`.† The compact arm checks the handle's health and the collection name (`resolve`), validates its arguments, scans the effective rows and only then reaches `commit_frame`, the one place that refuses a read-only handle, so an error of the scan is reported before `ReadOnly`. The resident arm checks health and name, validates, commits (so `ReadOnly` comes here, for a file of version 4 or later; an older file keeps its settings in memory and is never refused) and rebuilds its derived state last. Lane B's parked `250a98bc` adds a read-only check to the compact arm between validation and the scan, so both arms then answer in the order health, name, validation, `ReadOnly`, read. Its regression `compact_read_only_settings_preserve_error_precedence` pins that an invalid setting is reported before `ReadOnly` and that `ReadOnly` is reported before an error of the read; no test on this branch pins either order. | B4 |
| Compact scan draft in `c859d2d5` (lane A) | Decodes metadata for every row even without a filter, and takes visibility from the columns. | E4 |

## 5. Additions for P3c

[P3c section G](p3c-compact-ann.md#g-integration-seams-and-consumers) lists five things the ANN needs that the seam
lacks. This section gives them concrete, additive shapes, which the owner approved as additive on 2026-11-05
(OPEN-71-03). Existing signatures do not change. Lane R0's slice 3 implemented `base_rows`, `affected_ids` and
`ordered_ids` with the signatures below, crate-private and under the seam's test gate (statements in section 3.7);
`binding` and `for_each_vector_block` stay with the ANN lane, which has not started in 0.7.1.

| P3c name | At `v0.7.0` | Shape |
|---|---|---|
| `len`, `contains_id`, `row_of`, `id_at`, `revision_at`, `expiry_at` | `CompactView::{len, contains_id, row_of, id, revision, expiry}` | Frozen as they are. |
| `vector_at`, `metadata_at` | `CompactView::{read_vector, read_metadata}` and the effective accessors | Frozen as they are. |
| `base_vector_at` | `CompactView::read_vector` already ignores the overlay and deadlines and returns the base row's canonical `f32` values | Frozen as it is; no new code. |
| `ordered_visible` | `compact_iter` | A4 may fix the cursor; the signature stays. |
| `base_rows` | A loop over `0..len()` | Implemented in slice 3: `CompactView::base_rows` (BAS-1 to BAS-4). |
| `source_identity` | `CollectionRecords::source_identity` (process-local) | New portable binding, below (ANN lane). |
| `affected_since` | None (the overlay is private) | Implemented in slice 3: `CollectionRecords::affected_ids` (AFF-1, AFF-2), without an `S` argument, see below. |
| `score_batch` | None | New, below (ANN lane). |

```rust
// src/compact/source.rs (source owner)
// implemented in slice 3, test-gated
pub(crate) struct BaseRow { pub(crate) row: usize, pub(crate) id: u64, pub(crate) revision: u64, pub(crate) expiry: Option<u64> }
pub(crate) struct BaseRows<'a>; // Iterator<Item = BaseRow> + ExactSizeIterator + FusedIterator + Clone
impl CompactView {
    /// Physical rows in ascending ID order, deadlines ignored, so rows hidden by expiry are included.
    pub(crate) fn base_rows(&self) -> BaseRows<'_>;
}
// proposed for the ANN lane, not implemented
pub(crate) struct SourceBinding {
    pub lineage: u64, pub floor: u64, pub collection: u16,
    pub dimensions: u32, pub precision: Precision, pub digest: [u8; 32],
}
impl CompactView {
    /// What the ANN binds to; computed once while the image is checked.
    pub(crate) fn binding(&self) -> &SourceBinding;
    /// One decoded block at a time through the workspace, in ascending ID order.
    pub(crate) fn for_each_vector_block<F>(&self, workspace: &mut ReadWorkspace, visit: F) -> Result<()>
    where F: FnMut(usize /* first row */, usize /* rows */, &[f32] /* rows * dimensions */) -> Result<()>;
}
// src/records.rs (records owner), implemented in slice 3, test-gated
pub(crate) enum Affected { Put { revision: u64, expiry: Option<u64> }, Delete { revision: u64 } }
pub(crate) struct OrderedIds<'a>; // Iterator<Item = u64>
impl CollectionRecords {
    /// Overlay entries in ascending ID order, no allocation.
    pub(crate) fn affected_ids(&self) -> impl Iterator<Item = (u64, Affected)> + '_;
    /// Effective IDs in ascending order without materializing them: a merge of `base_rows` and `affected_ids`.
    pub(crate) fn ordered_ids(&self) -> OrderedIds<'_>;
}
```

- **`base_rows` (physical base).** Infallible and allocation-free, because ID, revision and deadline of every row are
  resident in the directory. It ignores deadlines (CLK-5), so an expired row that a checkpoint retained is still
  listed. It is the source for the ANN's content digest and for training. It returns a named cursor (`BaseRows`:
  exact-size, fused and cloneable) rather than `impl ExactSizeIterator`, so that `OrderedIds` holds it by value
  without boxing and later lanes can name it.
- **`binding` (portable binding).** `lineage` and `floor` (the checkpoint sequence `S`) come from the file header and
  the image; `dimensions` and `precision` from the block layout. `digest` is `B` of P3c section D, a SHA-256 over the
  complete-record blocks in partition order. It needs the verified plaintext body of each block, which only
  `ImageCheck::block` holds: `Capture::push` receives the on-disk frame, which is ciphertext for an encrypted image. The
  storage owner (L5) therefore adds the running digest to `ImageCheck` and hands the finished value to `Capture`, so it
  costs no extra authenticated read, and the ANN lane specifies and tests it. `G` and `C` of section D derive from it,
  the physical rows and the vector bits. `GenerationId` stays the process-local pin (IDN-1) and is never part of a digest.
- **`affected_ids` (affected-ID cursor).** By PRE-3 the overlay is the complete difference between the base and the
  effective state, so for the ANN, which covers the base only, the whole overlay is the delta since the base's floor.
  It is a state difference, not a commit history: an ID put and deleted again after an image that never held it is a
  tombstone while that handle is open and has no entry after a reopen, so the ANN's 256-ID, 8 MiB readiness bound, which
  counts deletes, is a bound on the entries the overlay holds. The cursor takes no `S`, because the overlay cannot
  answer for any floor but its own base's. P3c section G's "cover every commit S..T or fail explicitly" is therefore a
  check of the ANN: before it reads the cursor it compares the `S` and the lineage of its persisted graph with
  `binding().floor` and `binding().lineage`, and treats a mismatch as stale. The entries keep the latest revision, which
  is enough: an ID changed twice appears once. Tombstones for IDs that the base never held are harmless and can be
  skipped.
- **`ordered_ids`.** Replaces the materializing `effective_ids` on the scan and ANN paths; `effective_ids` stays for
  derived-index rebuild and resident parity until its last caller moves. It is an ID-only merge beside `compact_iter`,
  which stays the record cursor of A4 and E4. The slice 3 tests pin the two against each other (the IDs of
  `compact_iter` equal `ordered_ids`, at sampled seek points too), so that scan and browse cannot drift apart, and A4
  or E4 may later rebuild `compact_iter` on `ordered_ids` and point reads. It returns a named cursor (`OrderedIds`)
  and, like `base_rows`, starts at the first row.
- **`for_each_vector_block` (bounded batch).** Decodes one block through the same path as `read_vector`: it
  authenticates every visit, accounts the block under ACC-4, evicts under ACC-7 and never creates a cell. The visitor
  borrows one block at a time, so the live decoded bytes stay within the workspace budget. An error from the visitor or
  from a read stops the walk and is returned unchanged. The ANN's two-pass training and batch scoring through
  `exact_kernels::Scorer` use it instead of one `read_vector` call per row. E4's exact scan keeps one `read_vector` per
  row through the workspace cache, as `c859d2d5` does, and can adopt the walker later.

Slice 3 delivered the tests and mutants of its three cursors (section 7): `base_rows` equal to the resident rows including
an expired one, `affected_ids` and `ordered_ids` against the resident oracle after metadata-only, deadline-only, delete,
reinsert and duplicate-ID batches, and mutants that leave out the puts or the tombstones, reverse the order, skip the
first or last row and drop a deadline. Still required of the ANN lane for `binding` and `for_each_vector_block`: an
independent oracle for the digest (P3c oracle fixtures), the 4 MiB bound and a tampered block under the batch walk, and
mutants that skip a block, drop an authentication and wrap a counter.

## 6. Single-writer boundaries

Reserve by function, not by file. A lane edits only the functions reserved for it. A needed change in another lane's
function goes to the coordinator, who routes it to that owner. New production code is additive; a signature in section 2
changes only through an amendment. Seam code stays under `cfg(test)` until L5 decides otherwise, and new tests go to
sibling files, not to the end of a test module that another lane's patch also extends.

Order of integration follows the readiness inventory: R0, then B4 and A4 (file-disjoint except where marked), then E4,
then C4, then L5, then the ANN lanes.

Decision of 2026-11-05 (OPEN-71-03): lane R0 was the single writer of `src/compact/source.rs` and of the effective
block of `src/records.rs` until its slice 3 landed, so B4 and A4 made no production edit in either. B4's optional
ordered-cursor slice was dropped, because `ordered_ids` belongs to R0's slice 3. With slice 3 in, the reservation passes
to the lanes named in the tables below.

### `src/compact/source.rs`

One source owner at a time; nobody edits authentication or accounting in parallel.

| Region | R0 | B4 | A4 | E4 | C4 | L5 | ANN |
|---|---|---|---|---|---|---|---|
| Gate: `enabled`, `Activation` | frozen | | | | | un-gates | |
| `Capture`, `CompactGeneration::{new, decode, mapped_copy, view, has_collection}`, `GenerationId` | frozen | | | | | pin and retirement fields in a separate struct; `ImageCheck` (storage owner) computes the digest | adds the `SourceBinding` field and accessor |
| `ReadWorkspace`, `SeamAccounting`, `body` | frozen | | | | consumes `accounting` | separate counters struct | `for_each_vector_block` (additive) |
| `CompactView` read and physical methods | frozen; `base_rows` added in slice 3 (additive) | | | a seek in `base_rows`, if the scan needs one (additive) | | | `binding` (additive) |
| Test module declarations at the end of the file | adds `freeze_tests` and `ordered_tests` | | | | | | |

B4 and A4 make no production edit here: B4's only production hunks are three lines in `Database::set_collection_settings`
and a GPU preflight extraction.

### `src/records.rs`

Methods first, then files; never add a second effective-record iterator.

| Region | R0 | B4 | A4 | E4 | C4 | L5 | ANN |
|---|---|---|---|---|---|---|---|
| `CollectionRecords`, `OverlayEntry`, `RecordOverlay` (types) | frozen | | | | | | |
| Effective block: `pin_effective`, `effective_*`, `source_identity`, `source_accounting`, `overlay_logical_bytes` | frozen; `affected_ids`, `ordered_ids` added in slice 3 (additive) | tests only (`effective_rows_tests`, 28 lines in `250a98bc`) | | consumes; a seek in `ordered_ids`, if the scan needs one (additive) | consumes | | consumes; `effective_ids` retired last |
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
| `start_rebuild_with_options_in`, `poll_rebuild_in`, `maintain_background_hnsw_in`, `search_ready_with_options_in`, `build_hnsw*`, the snapshot and disk entry points | the compact arms refuse with unchanged state (frozen) | | | reserved | reserved for the ANN and F5 lanes |

Other shared surfaces, reserved by function as in the inventory: `collection.rs::{exact_search_candidates,
rebuild_derived}` (E4 and B4), `rerank.rs`, `metadata_index.rs`, `text_index.rs`, `quality.rs` (B4, then C4 consumes),
`kv.rs`, `hybrid.rs`, `query_language.rs`, `vectors.rs`, `property_graph/retrieve.rs` (A4, with K71 reserving `kv.rs`
and `server/kv.rs` functions), `hnsw.rs`, `rebuild.rs`, `ann_snapshot.rs`, `disk_ann.rs`, `gpu.rs` (the ANN and F5 lanes;
their compact refusal arms are pinned by `compact_ann_paths_remain_refused_with_unchanged_state`),
`compact/mod.rs::ImageCheck`, `format.rs`, `maintenance.rs`, `directory.rs`, `changes.rs`, `logops.rs` (L5 and the
storage owner).

## 7. The R0 mutation campaign

### 7.1 Slice 2: the freeze tests

Slice 2 adds tests only. To show that they are effective, and to give the next lanes a replayable record, it ran 91
hand mutants of the seam: the 17 of the earlier seam campaign, rebased onto `v0.7.0`, and 74 new ones in five classes
(identity, authentication and bounded extents, expiry and overlay precedence, budget, and accounting, which here covers
the counters, the peaks and the physical row columns). Each mutant is one anchored replacement in
`src/compact/source.rs` (59), `src/records.rs` (29), `src/lib.rs` (2) or `src/quality.rs` (1). The list,
with each anchor and replacement, and the result of every run are in
[`tests/compact_seam_mutants.json`](../../../tests/compact_seam_mutants.json).

A mutant counts only when its anchor matches exactly once, the tree compiles, the selector executes, at least one named
test fails and the file is restored byte-identically (SHA-256 before and after). A compile error, a crash (a linker
crash included), a timeout or a run that executes no test is recorded as such, never counts as a kill and is repeated.
A build holds exactly one mutant. The builds ran on CRLF checkouts, without `-D warnings`, so an unused binding that a
mutant leaves behind is not an error.

| Class | Mutants | Earlier campaign | New | Survive on `v0.7.0` | Killed on the final tree (replayed) |
|---|---|---|---|---|---|
| identity | 22 | 6 | 16 | 1 | 5 |
| authentication | 20 | 2 | 18 | 12 | 13 |
| expiry | 15 | 2 | 13 | 5 | 6 |
| budget | 11 | 1 | 10 | 4 | 4 |
| accounting | 23 | 6 | 17 | 6 | 6 |
| **Total** | **91** | **17** | **74** | **28** | **34** |

Replays, all with `cargo test --locked --offline --lib -- compact::source records:: metadata_index::tests::compact rerank::`:

- **Baseline**, `v0.7.0` (`02ef3e5f`), 75 tests, green before the first mutant. Of the 91 mutants, 87 were replayed on
  three other machines, and 86 of those also on the lane's machine (`R0-AU-17` was added after that pass); the two
  replays agree on every mutant they share. The other four (`R0-AC-17`, `R0-EX-11`, `R0-ID-11` and `R0-AU-18`) were added
  after the review of the first final tree and ran on the lane's machine only. Together they give 63 mutants that fail
  an existing test and 28 that survive, so those 28 are the places where replay evidence was missing.
- **Final**, `5c982e99` (`v0.7.0` plus the R0 tests, 94 selected). The lane's machine replayed 34 mutants
  there, in three shards, each with the selector green before its first mutant and again after its last: the 28 gaps,
  the three statement mutants (`R0-EX-11`, `R0-ID-11` and `R0-AU-18`) and three identity mutants that an existing test
  kills on `v0.7.0` (`R0-ID-01` to `R0-ID-03`). All 34 are killed. Five first attempts (`R0-AC-04`, `R0-AU-09`,
  `R0-AU-12`, `R0-BU-08` and `R0-BU-10`) ended in a toolchain or linker failure and are not kills; their reruns are the
  ones counted. No replay of a final tree ran on a separate machine, because none was available in time, so that
  replay is open (section 8, OPEN-71-02). The other 57 mutants, which an existing test kills on `v0.7.0`,
  were not replayed on the final tree. R0 changes no existing test (the only non-documentation changes since `v0.7.0`
  are the new `freeze_tests.rs` and its two-line module declaration), so each of them still fails the test that killed
  it; replaying them is open.

The 28 gaps, with how each is reached and what closes it:

| Mutant | Reach | What it does | Killed by |
|---|---|---|---|
| `R0-AC-02` | mapped | mapped_peak is overwritten instead of kept as a maximum | `freeze_mapped_accounting_keeps_its_own_peak` |
| `R0-AC-03` | api | each materialized cell is counted as one byte | `freeze_accounting_reports_exact_values_and_keeps_peaks` |
| `R0-AC-04` | api | cells_peak is overwritten instead of kept as a maximum | `freeze_accounting_reports_exact_values_and_keeps_peaks` |
| `R0-AC-05` | api | cell accounting covers only the view's own collection instead of the whole generation | `freeze_accounting_reports_exact_values_and_keeps_peaks` |
| `R0-AC-08` | api | vectors_peak is overwritten instead of kept as a maximum | `freeze_vector_peak_survives_eviction_by_a_smaller_block` |
| `R0-AC-17` | api | plaintext_peak is overwritten instead of kept as a maximum | `freeze_plaintext_peak_survives_a_smaller_frame` |
| `R0-AF-02` | input | replay misses an ID whose deadline changed (TTL-only write) when its revision did not | `freeze_replay_overlay_covers_every_kind_of_change` |
| `R0-AU-01` | guard | the frame-kind guard is dropped from the bounded body read | `freeze_frame_tampering_is_an_error_on_every_read_path` |
| `R0-AU-02` | guard | the frame-kind guard is dropped from record-cell materialization | `freeze_frame_tampering_is_an_error_on_every_read_path` |
| `R0-AU-03` | tamper | the footer checksum guard is dropped from the bounded body read | `freeze_a_refused_read_leaves_the_rest_of_the_workspace_as_it_was`, `freeze_cached_blocks_are_authenticated_again_and_shape_checked`, `freeze_frame_tampering_is_an_error_on_every_read_path` |
| `R0-AU-08` | guard | read_payload treats an out-of-bounds extent as an empty payload | `freeze_extents_outside_the_body_are_corrupt_on_every_read_path` |
| `R0-AU-09` | guard | read_metadata treats an out-of-bounds extent as empty metadata | `freeze_extents_outside_the_body_are_corrupt_on_every_read_path` |
| `R0-AU-11` | guard | the cached-block shape guard is removed | `freeze_cached_blocks_are_authenticated_again_and_shape_checked` |
| `R0-AU-12` | api | effective_vector turns a base-row read or authentication error into no row | `freeze_budget_and_exact_fit_boundaries`, `freeze_frame_tampering_is_an_error_on_every_read_path` |
| `R0-AU-13` | tamper | effective_metadata turns a base-row read or authentication error into no row | `freeze_frame_tampering_is_an_error_on_every_read_path` |
| `R0-AU-14` | tamper | effective_payload turns a base-row read or authentication error into no row | `freeze_frame_tampering_is_an_error_on_every_read_path` |
| `R0-AU-15` | guard | the bounded body read reports a frame range outside the backing as a changed frame (the read is still refused, only its reason changes) | `freeze_frame_range_outside_the_backing_is_corrupt` |
| `R0-AU-16` | guard | record-cell materialization reports a frame range outside the backing as a changed frame (the read is still refused, only its reason changes) | `freeze_frame_range_outside_the_backing_is_corrupt` |
| `R0-AU-17` | tamper | a block whose frame read fails is cached anyway, as an empty unaccounted entry | `freeze_a_refused_read_leaves_the_rest_of_the_workspace_as_it_was` |
| `R0-BU-07` | guard | a frame body of exactly MAX_BODY plus the seal overhead is refused as a changed frame (the size bound is inclusive) instead of as over the body limit | `freeze_frame_size_bound_is_exact` |
| `R0-BU-08` | guard | the frame-size bound is dropped, so an oversize plain frame is refused as over the body limit instead of as a changed frame | `freeze_frame_size_bound_is_exact` |
| `R0-BU-09` | guard | the decoded-body bound after authentication is dropped | `freeze_frame_size_bound_is_exact` |
| `R0-BU-10` | guard | the decoded-body bound after authentication is inclusive | `freeze_frame_size_bound_is_exact` |
| `R0-EF-02` | api | effective_payload lets an overlay tombstone fall through to the base payload | `freeze_effective_rows_follow_overlay_precedence_in_ascending_order` |
| `R0-EX-05` | api | an expired overlay put is visible to effective_visible (its deadline is ignored) | `freeze_deadline_hides_exactly_at_the_clock_in_every_arm`, `freeze_resident_and_compact_expiry_sources_agree_after_mutations` |
| `R0-EX-06` | api | a base row whose deadline equals the clock stays visible to effective_visible | `freeze_deadline_hides_exactly_at_the_clock_in_every_arm` |
| `R0-EX-07` | api | the resident arm of effective_visible hides one tick late | `freeze_deadline_hides_exactly_at_the_clock_in_every_arm` |
| `R0-ID-04` | guard | cache key ignores the block kind | `freeze_cache_key_separates_block_kinds` |

Seven of lane B's 18 open obligations are mutations of this seam (the [ledgers](p3b-survivor-ledgers.md#lane-b)). R0
re-created each one as `R0-AU-08`, `R0-AU-13`, `R0-AU-14`, `R0-EF-02`, `R0-EX-05`, `R0-EX-06` and `R0-EX-07`; all seven
survive on `v0.7.0` and all seven are killed with the freeze tests.

**Reach.** No mutant is claimed equivalent: every one changes the outcome of a state that a test builds, and a test
kills it. The states differ in how they arise, which the `Reach` column records:

- `api`: a call of the seam on a valid generation with valid inputs, such as the effective-read, expiry, replay and
  accounting semantics, or a budget refusal of an effective read.
- `mapped`: needs the test-only mapped copy of the backing, a state that no open path creates at `v0.7.0` and that L5
  will.
- `input`: calls a function with an input that no writer produces but that its contract covers, here
  `compact_from_replay` with a deadline that changed under an unchanged revision (every deadline write also writes a
  revision).
- `tamper`: changes the stored bytes of an installed generation in place.
- `guard`: checks internal consistency that no writer or committing handle can break. The recorded kind and frame ID
  come from one value at capture, a frame is always a range of the backing it was appended to, installation refuses an
  oversize body and validates every extent before capture, a cache key has one block, and the only block kind is
  `0x0101` (IDN-5). A test kills these only by editing the in-memory directory that a load derives, as lane B's own seam
  tests do.

Of the 28 gaps, 10 are `api`, 1 is `mapped`, 1 is `input`, 4 are `tamper` and 12 are `guard`. By the approved definition of an effective mutant, all 28 count. Two stricter readings are
possible. Counting only the 12 that need neither stored bytes nor the directory to be edited (`api`, `mapped` and
`input`) meets the minimum of 12 as well. Counting only the 10 `api` mutants, the ones that arise from states the
product can produce at `v0.7.0`, does not. A reader who counts over those states may call the `tamper` and `guard`
mutants equivalent; they are kept because the guards are the error contract of section 3. `R0-BU-09` and `R0-BU-10` are
reachable only for a plain frame whose body lies between `MAX_BODY` and `MAX_BODY + SEAL_OVERHEAD` bytes, which
`freeze_frame_size_bound_is_exact` builds by hand.

**What the campaign does and does not measure.** All 19 freeze tests fail at least one of the replayed mutants, so each new test has a measured kill, and existing tests fail
many of the same mutants, so the freeze tests are not the only pins of the seam. Sixteen of the 19 kill a gap. The other
three kill the mutants of their own statements: `freeze_source_never_reads_the_wall_clock` kills `R0-EX-11`,
`freeze_named_collections_keep_one_overlay_each` kills `R0-ID-11`, and
`freeze_cache_entries_belong_to_one_collection_of_one_generation` kills `R0-ID-01` and `R0-ID-02`. It applies no mutant to CLK-7 (section 3.1 marks it as
not pinned), PRE-4, IDN-3 or CON-1, whose tests existed at `v0.7.0`: a lane that changes one of those four statements
adds its mutant first. `R0-EX-11` (CLK-1), `R0-ID-11` (PRE-6) and `R0-AU-18` (ERR-7) were added after the reviews,
because the tests of those statements had no mutant. The campaign shows that the tests pin the statements in section 3,
not that the statements are complete, and it supports no size or speed claim. Anchors are valid on the trees named
above; a lane that changes the lines around one re-bases it and replays the mutant, and does not trust the recorded
result.

### 7.2 Slice 3: the ordered cursors

Slice 3 adds `CompactView::base_rows`, `CollectionRecords::affected_ids` and `CollectionRecords::ordered_ids` (section
3.7) and ran 34 hand mutants of them in six classes (coverage, identity, expiry, accounting, overlay and ordering).
Each mutant is one anchored replacement in `src/compact/source.rs` (13) or `src/records.rs` (21). The list, with
each anchor and replacement, and the result of every run are in
[`tests/compact_ordered_cursors_mutants.json`](../../../tests/compact_ordered_cursors_mutants.json). The counting rules
of 7.1 apply. The three functions do not exist on `v0.7.0`, so there is no baseline on the previous tag: every mutant
mutates new code, and the replay below is the evidence.

| Class | Mutants | Killed |
|---|---|---|
| coverage | 7 | 7 |
| identity | 4 | 4 |
| expiry | 4 | 4 |
| accounting | 6 | 6 |
| overlay | 10 | 10 |
| ordering | 3 | 3 |
| **Total** | **34** | **34** |

The replay ran on `f167cf3d` (the tests of 7.1 and of section 3.7, 101 selected with the selector of 7.1, green
before the first mutant and again after the last of each of three shards) on the lane's machine: 34 of 34
killed, every file restored byte-identically. No replay of slice 3 ran on a separate machine, because none was available
in time, so that replay is open (section 8, OPEN-71-02). The independent reviews of the first version of the tests found
four mutants that survived it: `S3-OR-12`, `S3-OR-14` and `S3-BR-13`, which decode and cache record cells that no cursor
may touch, and `S3-OR-13`, which lists the ID of a tombstone while a larger base row is pending. They are in the list,
the tests were strengthened until each is killed, and the whole list was replayed again on the final tests.

Not in the list, because no test can tell them from the code they replace:

- `self.next >= self.end` in `BaseRows::next` changed to `>` or removed, because `row_at` answers `None` at the end;
- `self.source?` in `BaseRows::next` changed to `.expect(..)`, because `end` is 0 whenever there is no source;
- `base_id == Some(overlay_id)` in the merge changed to `<=`, because that arm only sees `None` or an equal ID, and
  `next` on an exhausted `Peekable` does nothing.

These three guards are redundant defensive code. Deleting the `Clone` derive or the `FusedIterator` implementation of
`BaseRows` is a compile error and does not count as a kill; the trait set is checked at compile time by
`ordered_base_rows_list_every_physical_row_in_ascending_id_order`. Claims marked with a dagger in section 3.7 (no
allocation, no frame read or authentication) cannot be observed by a test through the seam and have no mutant.

## 8. Decisions

Decided on 2026-11-05 by the coordinator under the owner's delegation, as recommended.

- **OPEN-71-01, activation: private integration only.** Nothing in 0.7.1 turns the compact seam on. The production compact
  writer, float16 and temporal copy-out stay opt-in and closed, no setting, option or file content enables a gate, and every
  statement on this page describes private, test-gated code (section 1).
- **OPEN-71-02, survivor evidence: approved.** The evidence form in the [ledgers](p3b-survivor-ledgers.md#evidence-that-would-settle-open-71-02)
  is the acceptance form: one record per mutant with before and after SHA-256, the failing test names and a restored-green
  run, replayed independently. R0's ledgers have that shape with two qualifications, which the slice reports ask the
  coordinator to confirm. First, the restored-green run is recorded once per replay (before the first mutant and after the
  last) and not after every mutant, because a byte-identical restore, which the ledger records per mutant, returns the tree
  whose baseline run was green, and a run per mutant would double the number of builds. Second, the independent replay
  exists for the baseline on `v0.7.0` (87 of the 91 mutants of slice 2 ran on three other machines, and 86 of those also on
  the lane's); every replay of a final tree, in slice 2 and in slice 3, ran on the lane's machine only, because no other
  machine was available in time, and an independent replay of the final trees is open. An equivalence claim
  (`pa-graph-evidence-wall-clock`) additionally needs an independent review and a pinning test wherever one can be
  written.
- **OPEN-71-03, seam freeze: approved.** Section 3 is frozen, section 4 is the approved inventory of limits that need not
  survive, and section 5 holds the additive shapes. R0 implemented `base_rows`, `affected_ids` and `ordered_ids` as its
  slice 3, ahead of E4, because the exact scan needs an ID source that does not materialize: crate-private, under the
  seam's test gate, with `effective_ids` unchanged, the tests and mutants that section 5 lists (section 7) and an
  independent review. `binding` and `for_each_vector_block` stay with the ANN lane.
- **OPEN-71-04, J's moved tests: ratified.** The HNSW and snapshot names (`compact_hnsw_rerank_uses_pinned_generation`,
  `compact_ann_snapshot_rejects_other_generation`, `compact_rebuild_snapshot_survives_drop_and_refused_checkpoint`) move
  to P3c and read "moved to P3c, required before any compact ANN activation" in the J row of the P3 design and in the
  P3c test plan, so that no gate disappears. Slice 4 proves the generic pin and the refusal with the tests that exist
  (`compact_metadata_text_indexes_verify_overlay`, `compact_ann_paths_remain_refused_with_unchanged_state`), and
  `compact_server_materializes_only_results` stays with C4.
- **OPEN-71-14, campaign sizes: approved.** Keep the inventory's minima and every historical survivor, count a mutant only
  when it compiles, runs, fails a named test and restores byte-identically, and take a baseline run on the previous tag so
  that a gap closed is measured and not claimed. R0 shows that 74 new mutants over one seam, and 34 more over its
  cursors, are practical.

R0's own choices in slice 3 are not coordinator decisions. The slice report asks the coordinator to confirm them:

- **Slice 3 shapes.** `base_rows` returns a named cursor (`BaseRows`) and `ordered_ids` a named cursor (`OrderedIds`)
  rather than the approved `impl Iterator` returns, so that the merge holds the first by value without boxing and a
  later lane can name both. `affected_ids` keeps its approved `impl Iterator` return and takes no `S` (section 5), and
  none of the cursors seeks (section 4). The shapes stay additive and crate-private. A later lane that needs a different
  shape changes it through an amendment to this page.

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
