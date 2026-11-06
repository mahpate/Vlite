# P3b read seam freeze (lane R0, written during 0.7.1 and carried to 0.7.2)

Status: written at the start of 0.7.1 from `v0.7.0` (`02ef3e5f`) and approved on 2026-11-05 (the decisions OPEN-71-01 to OPEN-71-04
and OPEN-71-14, stated in section 8, were taken by the coordinator under the owner's delegation; the OPEN-71 identifiers are
those of the storage readiness inventory behind the [0.7.1 plan](handover-0.7.0.md#071)). The same day the owner moved the
compact work (lanes R0, B4, A4, E4 and C4) to 0.7.2: 0.7.1 shipped without it ([handover-0.7.1.md](handover-0.7.1.md)), so
where this page says that a lane integrates its work, it means the integration tree for 0.7.2 and not the tag `v0.7.1`. The
tree that section 7.3 and the T paragraph of section 1 name is `mahpate-int072-c1` at `2d405125`, called T below; sections 7.1
and 7.2 predate it, and the "integrated tree" they name is R0's own `faa3b32e`, which is not T.
Sections 3 and 4 describe what the code does today and pin it with tests. Section 3 is frozen for the lanes that follow it:
B4 and A4, which worked from it, E4, which consumed it, and C4, L5 and the ANN, P4 and P5 lanes. Section 4 is the approved inventory of deviations and limits: it
is not frozen, and a lane may remove any of them. Section 5 gives additive shapes, approved as additive: lane R0's
slice 3 implemented `base_rows`, `affected_ids` and `ordered_ids` (crate-private, under the seam's test gate; statements
BAS, AFF and ORD in section 3.7), and `binding` and `for_each_vector_block` stay with the ANN lane. R0's own work changes
no behavior, file format or wire, activates no compact writer, float16 or temporal copy-out, and supports no size or
speed claim; the few changes that lane A4 makes to resident behavior, which a release build compiles, are named in CLK-7
and section 4 and listed on A4's page. Companion pages: the [survivor ledgers](p3b-survivor-ledgers.md) of lanes A and B, the [record
consumers of lane A4](p3b-record-consumers.md), the [bounded exact scan of lane E4](p3b-exact-scan.md), the
[P3 design](p3-compact-vectors.md) and the [P3c design](p3c-compact-ann.md).

## 1. Baseline

| Item | State at `v0.7.0` |
|---|---|
| Compiled in release builds | `compact::catalog` (the precision catalog kinds, without `payload` and `column`), `compact::half`, the in-memory part of `compact::stats`, and `ImageCheck` in the load, feed and export paths. Compact blocks of a critical or unassigned kind refuse with `UnsupportedFeature(FLAG_COMPACT)`. |
| Compiled only under `cfg(test)` | `compact::source`, `compact::graph`, `compact::vectors`, `catalog::payload`, `catalog::column`, `stats::codec`, `CollectionRecords::Compact`, `RecordOverlay`, `OverlayEntry`, and every compact dispatch arm in `lib.rs`, `collection.rs`, `rerank.rs`, `quality.rs`, `metadata_index.rs` and `text_index.rs`. |
| Gates | Three thread-local test switches (`compact::Activation`, `catalog::payload::Activation`, `compact::source::Activation`), all off by default. No setting, option or file content turns them on. |
| Opening a compact image | `format::load_with` still builds the resident `RecordMap`; `Database::open_backed` then installs the generation and an overlay over it (`compact_from_replay`). The seam does not bound memory at open. |
| Production compact writer | None. Compact images exist only as test fixtures written by `maintenance::write_v5_image`. |
| Lane B (derived indexes, read seam) | Integrated: seam, effective rows, metadata, text and rerank consumers. Its fix-forward `250a98bc` is not in the tag; T carries it (B4: cherry-picked, with its 18 replay obligations replayed in the ledgers of R0 and B4 on those lanes' own trees, and B4's slice 3, a differential of the declared-index scan that runs the compact arm against the resident arm and a model of the contract, with 11 mutants of its own). |
| Lane A (record consumers) | Not in the tag: four patches parked at `834a0a6f`. T carries them (A4, accepted at `4670d858`): cherry-picked, never merged, with the key-value hunks and `60d65f99` rebuilt last on K71's bounded visitor, 68 consumer tests in nine modules and two resident graph tests, and 217 mutants killed on A4's own head ([lane A4](p3b-record-consumers.md)). The compact arms are test-gated, but A4 also makes a few changes to resident code that a release build compiles (CLK-7, section 4); they ship with the release that integrates A4, which is not 0.7.1. Lane E4 changes the ID walk of the exact-scan patch `c859d2d5` (next row). |
| Lane E4 (bounded exact scan) | Not in the tag. On T the loop of `exact_search_compact_with_workspace` walks `ordered_ids` where the draft walked `effective_ids` (one line of `src/lib.rs`, compiled only under `cfg(test)`), and E4 adds five tests (`src/compact/source/overlay_tests/exact_scan.rs`) and 24 mutants (`tests/compact_e4_mutants.json`). [Its page](p3b-exact-scan.md) states the contract, the one allowed difference in `records_scanned` (section 4 of this page) and the hazards for the promotion. Its mutant evidence is E4's to add: the ledger carried no recorded runs when T was cut, so `verify` lists all 24 as open ([the ledgers on T](p3b-survivor-ledgers.md#the-ledgers-on-the-integration-tree)). |
| Lane C (server and MCP consumers), slice 5 (lifecycle) | Not started. |

The table is the state of the tag, except for the lane A, B and E4 rows, which say what T adds to it. T is `mahpate-int072-c1`
at `2d405125`: `main` after 0.7.1, with a few test-only commits picked, lane R0's branch merged (all of it, up to
`6635a454`, without a conflict), lane B4 merged, A4's own commits, B4's slice 3 and E4's four commits cherry-picked. The
coordinator reports for T: formatting and the strict lint (workspace, all targets, all features) clean, 1,216 library tests
passing by default and 1,688 with all features, the documentation guards, `scripts/api_surface.py` and
`scripts/shadow_copy_check.py --integration` passing (it needs a `--lane` range for what 0.7.1 changed and `--kv-base` for
K71's visitor; with its default arguments it fails on the functions that 0.7.1 changed). R0 measured on T that all 91 and 34 anchors of its ledgers match
exactly once and that the selector of section 7 (204 tests there) is green; section 7.3 holds the replays of its 52 mutants of
`records.rs` and `lib.rs` on T (all killed, on the lane's machine and on separate machines, and accepted by `verify --strict`).
T is frozen while the evidence is taken: a change to anything that cargo builds changes the
fingerprint of the sources (`python tests/compact_mutant_replay.py sources`) and makes the replays of every lane stale
under `verify --strict`.

Between the 0.7.0
candidate that the readiness inventory used (`5cc496d4`) and the tag, only `src/kv.rs`,
`src/lib.rs` and `src/property_graph/retrieve.rs` changed among the seam's files, and none of those changes is in
`src/compact`, `src/records.rs` or `src/collection.rs`. The seam code is therefore identical in the two.

## 2. The seam in one table

The seam is three concrete pieces and no trait. `CompactView` answers physical questions about one collection of one
pinned generation. `CollectionRecords::Compact` answers effective questions by putting an overlay in front of a view.
`ReadWorkspace` holds the operation-local scratch. P3c's `ReadAccess` is a name for these three together. The functions
marked new were added by R0's slice 3; every other function is the one at `v0.7.0`, unchanged.

| Piece | Functions | Meaning |
|---|---|---|
| `CompactGeneration` (`compact/source.rs`) | `Capture::{push, finish}`, `new`, `view`, `has_collection`, `decode` | One immutable, authenticated image: owned or mapped bytes, the block directory, one lazy `Record` cell per row, the cipher context. Built once, after the whole image validated. |
| `CompactView` | `generation_id`, `len`, `first_id`, `contains_id`, `row_of`, `id`, `revision`, `expiry`, `base_rows` (new), `get`, `get_at`, `get_physical`, `read_vector`, `read_metadata`, `read_payload` | Physical rows of one collection, in ascending ID order. `get` and `get_at` apply a deadline, every other accessor is physical. `base_rows` lists every physical row from the resident directory. `get`, `get_at` and `get_physical` decode a row into its cell once and serve the cell afterwards. The `read_*` methods copy out of one authenticated frame on every call and never create a cell. |
| `ReadWorkspace` | `new`, `accounting`, `body` (private) | At most 4 MiB of decoded vectors, the latest authenticated plaintext frame and the counters. One per operation, serial, never shared and never persisted. |
| `CollectionRecords` (`records.rs`) | `len`, `is_empty`, `first_id`, `contains_key`, `get_at`, `get_physical`, `apply_committed`, `compact_iter`, `compact_overlay_revision`, `compact_from_replay`, `pin_effective`, `effective_ids`, `effective_vector`, `effective_metadata`, `effective_payload`, `effective_visible`, `source_identity`, `source_accounting`, `overlay_logical_bytes`, `affected_ids` (new), `ordered_ids` (new) | The effective view: overlay first, then the base. `pin_effective` clones the enum, which pins the generation and the overlay root together. `get_at`, `get_physical` and `compact_iter` hand out records, so they materialize base rows into cells; the `effective_*` accessors never do. `affected_ids` lists the overlay entries and `ordered_ids` the effective IDs, neither reads a block. |
| Consumers (test-gated) | `Collection::rebuild_derived`, `MetadataIndexes::rebuilt_effective`, `TextIndex::rebuilt_effective`, `rerank_effective`, `quality::effective_memory`, the compact arm of `Database::set_collection_settings` (now in `set_live_collection_settings`, section 6) | Read through the effective view with a 4 MiB workspace and, where they test visibility, an explicit clock. |

## 3. Frozen semantics

Each statement has an ID so that a test, a mutant and a later change can cite it. A change to a statement needs an
amendment to this page, a test that cites the new text and an updated mutant ledger. The tests named here are the
pinning ones; `freeze_*` tests live in `src/compact/source/freeze_tests.rs`, and the two tests of ERR-8 are lane B's, in
`src/compact/source/seam_tests.rs` and its `seam_tests/survivor_tests.rs`. A dagger marks a detail that was verified
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
| CLK-7 | A public call that takes no clock samples the wall clock once on entry and passes it down, and a `ReadSnapshot` uses the clock it captured.† Since A4 the hybrid search follows the rule for its text leg and its exact vector leg, and `GraphRef::retrieve` reads its pivot and evidence records at one clock, the one that `with_clock` set or else one sample of the wall clock. Four resident arms are known exceptions that sample per candidate or per call: query-language execution, `reembed_pending`, `ann_quality` and `search_ready_with_options_in`, which serves the approximate leg of the hybrid search and of the pivot of `GraphRef::retrieve` (section 4). Other callers that sample per candidate were not audited. The tests pin the explicit-clock half only: one clock passed down gives one answer. | `compact_browse_cursor_skips_tombstones_and_expiry`; `compact_exact_scan_is_bit_identical_at_same_clock`; `hybrid_resident_default_clock_honors_ttl` and `retrieve_reads_the_pivot_records_at_the_read_clock` (A4), with A4's mutants `a4-retrieve-resident-pivot-search-wall-clock` and `a4-retrieve-resident-revalidation-wall-clock` |

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
| ERR-8 | The compact arm of `Database::set_collection_settings` refuses in this order: the handle's health (`Poisoned`), the collection name,† the settings (`InvalidCollectionSettings`), a read-only handle (`ReadOnly`), and only then it reads the effective rows. A call that validation or the read-only check refuses writes nothing and changes no setting. This is the order of the resident arm, which refuses a read-only handle at its commit.† At `v0.7.0` the compact arm had no read-only check of its own and refused a read-only handle only at the commit, after its scan; lane B's `250a98bc` adds the check between validation and the scan. On T the arm sits in `set_live_collection_settings(id, ..)`, which 0.7.1's request guard split out of `set_collection_settings` (after `resolve`) so that the server can enter it by ID (`set_settings_by_id`, after `live_collection`); both entry points begin with the health check, so the order above holds for each.† | `compact_settings_check_the_handle_health_before_every_other_gate`; `compact_read_only_settings_preserve_error_precedence` |

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

Added by R0's slice 3 and frozen from the day it landed: `CompactView::base_rows`, `CollectionRecords::affected_ids`
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

These are true at `v0.7.0` with the changes of lanes R0, B4, A4 and E4 on T (the rows that name slice 3, A4 or E4 describe
T). They are not statements to preserve; the lane named in each row is expected to change them. The lanes are those of the
0.7.0 plan ([handover-0.7.0.md](handover-0.7.0.md#071): L5 is the effective-source lifecycle, P5-71 the persisted summaries);
none of them has a release yet ([handover-0.7.1.md](handover-0.7.1.md)). A lane that is done is not named: E4's one
production change is the loop of the exact scan (section 6), and it left the rows on record walks, refused reads and record cells
as they were, so those rows name L5.

| Where | What | Lane |
|---|---|---|
| `query_language.rs::execute`, `vectors.rs::reembed_pending`, `quality.rs::ann_quality`, `lib.rs::search_ready_with_options_in` | The resident arms of query-language execution (`collection.search`, then `get` per hit and per text candidate) and of `reembed_pending` (`self.get(id)` per row) sample the wall clock for every row, so each row is judged at its own sample. `ann_quality` samples once per sampled query (`quality.rs:227`). `search_ready_with_options_in` takes no clock and samples its own: it is the approximate vector leg of the hybrid search and of the pivot of `GraphRef::retrieve`, which therefore still judges that leg at the wall clock while the rest of the call reads at the handle's clock. A4 moved the text leg and the exact vector leg of `search_hybrid`, which this row listed at `v0.7.0`, and the pivot and evidence reads of `GraphRef::retrieve`, which it did not list, to one clock, and the compact arms of `execute` and `reembed_pending` read one clock. `ann_quality` and `search_ready_with_options_in` read the resident map, so they refuse compact records. | a clock-taking `execute_at` and `reembed_pending` (not scheduled); the ANN lane for the other two, which flips `retrieve_reads_the_approximate_pivot_leg_at_the_wall_clock` when it gives `search_ready_with_options_in` a clock |
| `CompactView::base_rows` and `CollectionRecords::ordered_ids` (slice 3) | Neither seeks: both start at the first row, so a walk that resumes after an ID pays for every row before it. `compact_iter(after)` does seek, by a binary search over the base IDs (lane A's patch, in `records.rs`). `Affected` carries the overlay's revision and deadline, not its metadata or payload. E4's scan starts at the first ID and asked for no seek. Its page (section 6) recommends, for the promotion, an additive cursor that yields the base rows and the overlay entries together, because `ordered_ids` drops the row index and the deadline that the base cursor already holds; it made no request for it to R0. | a later lane that resumes a walk or wants the merged cursor (an additive amendment to this page, section 8) |
| `records.rs::effective_ids` | Builds a full ID set and vector. The 4 MiB workspace bounds vector scratch only, not ID memory. `ordered_ids` (slice 3) lists the same IDs lazily, and E4's exact scan walks it. The rebuilds of the derived indexes (`MetadataIndexes::rebuilt_effective` and `TextIndex::rebuilt_effective`) and some tests still call `effective_ids`. | its callers move to `ordered_ids` one by one (the derived-index rebuilds are next); `effective_ids` stays until the last one has moved |
| Record walks and the exact scan | Browse (`compact_browse`, also on a `ReadSnapshot`), facets, the text leg of the hybrid search, query-language execution, the key-value listing and `reembed_pending` walk `compact_iter`. It decodes each row it visits into a cell, which is never evicted (record cells, below), before the caller looks at the deadline, and each caller reads deadlines from the resident map (CLK-6). Browse stops after one page and `reembed_pending` at its limit; the others visit every row. The exact scan (last row of this table) is not a record walk: it takes visibility from the view and overlay columns and creates no cell. E4's `compact_exact_scan_scores_without_materializing_record_cells` pins that a scan through the helper, the collection state, the database search, a snapshot and the handle's filtered search, in both float precisions, plain and encrypted, leaves the decode counter and the initialized cells as they were before it (none, for an untouched generation). A4's page still says that the draft materializes a cell per visited row (its section 7); it does not. | L5 (it may rebuild `compact_iter` on `ordered_ids` and point reads) |
| Scan statistics of a filtered exact search (`Database::search_in_at`) | The resident arm narrows its scan with the declared postings: when the most selective served condition holds at most half of the rows, its IDs (intersected with those of the other served conditions within that bound) are the candidates, counted in `index_candidates`. When that leaves no candidates (no posting serves a condition of the filter, or the best one holds more than half of the rows), it falls back to the block summaries of the declared fields (P5: blocks of 1,024 live rows, used when at least one block is pruned and at most half the rows remain). The compact arm takes the postings in the same way and passes them to the exact scan, but it has no block summaries, so without candidates it scans every effective row and lets the filter decide. The hits, their order, `distance_evaluations` and `index_candidates` are equal on the two paths. `records_scanned` is equal too, except for a filter that block summaries could prune and no posting narrows, where the resident arm counts fewer rows. A block is pruned when its summary proves that no row of it can match: per covered field a block keeps the distinct string and point values (up to 32 values and 1,024 bytes, past which it can no longer prune an equality), the interval of its integers and of its floats and the booleans seen, so an equality value that is not among them or outside the interval, or a range that misses the interval, prunes the block. That happens in several blocks when a filter matches rows of only a few of them, and in a single block when its summary shows that no row matches (`Summaries::candidates` over `View::survivors` in `src/compact/stats.rs`). On T, B4's `compact_scan_examines_every_row_where_the_resident_block_summaries_prune` pins the difference over 2,500 rows in three blocks, with six range filters on a field declared for equality only (so that no posting serves them): the compact arm scanned all 2,500 rows each time and the resident arm 1,024, 1,024, 452, 2,500, 2,500 and 1,024, with equal hits, evaluations and candidates. The difference needs neither a second block nor a missing posting, which B4's test and E4's page (section 4: "above one 1,024-row block" and "where no posting serves the filter") do not say: section 7.4 records R0's probes of a single block and of a posting that holds more than half of the rows. No test on T pins those two cases; they belong next to B4's test, and are not added now because T is frozen. A snapshot search (`ReadSnapshot::search_with_stats_at`) passes no candidates on either path and reports equal statistics.† E4's page records the difference as the one allowed, a decision of the release coordinator that moves the compact summary fallback to P5-71 (persisted summaries). No result changes. P3's claim of equal scan statistics holds for a filter that a declared posting narrows and for one on fields that are not declared; B4's differential sweeps it over five declarations and every kind of operation. | P5-71 (it flips B4's test above, which asserts that the compact arm scans every row, and adds the two cases of section 7.4) |
| Expiry and revision readers | The record walks above read the resident expiry map (the key-value listing also reads the revisions of its entries from the resident map), and the exact scan, `get_at` and `effective_visible` (and so `rerank_effective`) read the view and overlay columns (CLK-6). `revision_in`, which serves `revision()`, the server and graph retrieve, answers from the overlay when it holds an entry for the ID and from the resident map otherwise, and a conditional write (`put_with_expiry_in`, `put_in`, `delete_in`, `update_metadata_in` and `apply_reembed_batch_checked`) checks the resident map alone.† A lane may move a reader to the other representation if it keeps one clock per operation and the agreement of the two. | L5 |
| `Database::open_backed` | The loader decodes every record into a resident map before the generation is installed. | L5 |
| `CollectionRecords::len`, `first_id` | `len` walks the overlay on every call, and `first_id` scans the base from row 0 past tombstoned rows and then the overlay. | not scheduled |
| Refused reads are not atomic | `read_vector` counts a miss, advances the logical clock and evicts to make room before it reads the frame, so a read that then fails can leave a block evicted. A read refused by the scratch budget keeps the previous plaintext, and an encrypted read that fails after authentication (an extent or decode error, or the shape refusal on a hit) keeps that frame's plaintext until the next read. A consumer must not rely on a rollback. E4's `compact_exact_scan_workspace_boundaries_preserve_explicit_error_accounting` pins the scratch-budget refusal of the exact scan (it is decided before any cache counter moves), and `freeze_a_refused_read_leaves_the_rest_of_the_workspace_as_it_was` pins a failure while the frame is read in a workspace that has room, where the refused block is not cached, the cached one stays and nothing is evicted; no test pins a failure that follows an eviction. | L5 |
| Plain frames | A read of a plain generation compares only the footer's stored checksum field with the one recorded at capture. It does not recompute the checksum over the body, so a mapped plain frame whose bytes change under an unchanged footer is served as it is. An encrypted frame is authenticated in full on every workspace read and on the first decode of a cell. | L5 |
| Record cells | A materialized cell is never read or authenticated again and never evicted, and every committed write to a base ID creates that row's cell, because `Collection::apply_committed` reads the previous record through `get_physical`. Cell decoding has no frame-size or body bound and does not check a vector extent's width. The exact scan creates no cell. | L5 |
| Compact exact scan (`c859d2d5` from lane A, with E4's loop) | `exact_search_compact_with_workspace` walks `ordered_ids` and skips the IDs outside the candidate set (the declared postings that `search_in_at` passes, when a posting narrows the scan); it still tests every ID against the set, so a small candidate set costs a walk over all rows. For every other ID it takes visibility from the view and overlay columns and then reads the row's metadata, even when the filter is empty (decoded for a base row, borrowed for an overlay put; for an encrypted base row that is one authentication of the whole frame for the metadata and another for the vector). It reads the vector of every row that matches through the 4 MiB workspace, which decodes a block whole, and scores one row per call; it has no block summaries. E4's page (section 6) lists these as hazards for the promotion. | L5, P5-71 |

## 5. Additions for P3c

[P3c section G](p3c-compact-ann.md#g-integration-seams-and-consumers) lists five things the ANN needs that the seam
lacks. This section gives them concrete, additive shapes, which the owner approved as additive on 2026-11-05
(OPEN-71-03). Existing signatures do not change. Lane R0's slice 3 implemented `base_rows`, `affected_ids` and
`ordered_ids` with the signatures below, crate-private and under the seam's test gate (statements in section 3.7);
`binding` and `for_each_vector_block` stay with the ANN lane, which has not started.

| P3c name | At `v0.7.0` | Shape |
|---|---|---|
| `len`, `contains_id`, `row_of`, `id_at`, `revision_at`, `expiry_at` | `CompactView::{len, contains_id, row_of, id, revision, expiry}` | Frozen as they are. |
| `vector_at`, `metadata_at` | `CompactView::{read_vector, read_metadata}` and the effective accessors | Frozen as they are. |
| `base_vector_at` | `CompactView::read_vector` already ignores the overlay and deadlines and returns the base row's canonical `f32` values | Frozen as it is; no new code. |
| `ordered_visible` | `compact_iter` | A4 gave it a binary-search seek; the signature stayed. |
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
  derived-index rebuild and resident parity until its last caller moves. E4's exact scan is the first caller that moved
  (`exact_search_compact_with_workspace` walks it). It is an ID-only merge beside `compact_iter`,
  which stays the record cursor of browse, facets, the hybrid text leg, query-language execution, the key-value listing
  and `reembed_pending` (section 4). The slice 3 tests pin the two against each other (the IDs of
  `compact_iter` equal `ordered_ids`, at sampled seek points too), so that scan and browse cannot drift apart, and
  a later lane may rebuild `compact_iter` on `ordered_ids` and point reads. It returns a named cursor (`OrderedIds`)
  and, like `base_rows`, starts at the first row.
- **`for_each_vector_block` (bounded batch).** Decodes one block through the same path as `read_vector`: it
  authenticates every visit, accounts the block under ACC-4, evicts under ACC-7 and never creates a cell. The visitor
  borrows one block at a time, so the live decoded bytes stay within the workspace budget. An error from the visitor or
  from a read stops the walk and is returned unchanged. The ANN's two-pass training and batch scoring through
  `exact_kernels::Scorer` use it instead of one `read_vector` call per row. E4's exact scan keeps one `read_vector` per
  row through the workspace cache, as `c859d2d5` does, and can adopt the walker later (E4's page, section 6, lists the
  batch of one among the hazards of the promotion).

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
| `CompactView` read and physical methods | frozen; `base_rows` added in slice 3 (additive) | | | no edit: the scan needs no seek (a later lane that wants a seek or a merged cursor adds it through an amendment to this page, section 8) | | | `binding` (additive) |
| Test module declarations at the end of the file | adds `freeze_tests` and `ordered_tests` | | | | | | |

B4, A4 and E4 made no production edit here: B4's only production hunks (in `250a98bc`, cherry-picked) are three lines in
`Database::set_collection_settings` (now `set_live_collection_settings`) and a GPU preflight extraction.

### `src/records.rs`

Methods first, then files; never add a second effective-record iterator.

| Region | R0 | B4 | A4 | E4 | C4 | L5 | ANN |
|---|---|---|---|---|---|---|---|
| `CollectionRecords`, `OverlayEntry`, `RecordOverlay` (types) | frozen | | | | | | |
| Effective block: `pin_effective`, `effective_*`, `source_identity`, `source_accounting`, `overlay_logical_bytes` | frozen; `affected_ids`, `ordered_ids` added in slice 3 (additive) | done: tests only (`effective_rows_tests`, 28 lines of `250a98bc`) | | done: consumes (the scan walks `ordered_ids`); no seek asked for | consumes | | consumes; `effective_ids` retired last |
| `len`, `is_empty`, `first_id`, `contains_key`, `get_at`, `get_physical`, `apply_committed`, `compact_overlay_revision`, `compact_from_replay` | frozen | | | | | | |
| `compact_iter`, `CompactRecordIter` | frozen | | done: the seek, a binary search over the base IDs (lane A's patches) | not used | | | |
| `ReadSnapshot::{browse_at, search_with_stats_at}`, `compact_browse` | frozen | | done: the call into the draft scan (`c859d2d5`: `search_with_stats_at` passes the records instead of an iterator) | no edit needed | | | |
| `CollectionRef::{search_grouped, facets}` compact arms | | | done: consumer arms (`02062058`, with A4's tests) | | | | |

### `src/lib.rs`

One integrator owns the `Database` fields and constructors, `open_backed`, compact dispatch, the checkpoint swap and
`replica_extend`.

| Function | R0 | B4 | A4 | E4 | L5 |
|---|---|---|---|---|---|
| `Database` fields, `open_backed`, `compact_base_matches`, `discard_tail_inner`, `ensure_resident_in`, `ensure_mutable_in`, `ensure_all_resident`, `revision_in`, `apply` | frozen | | | | the one integrator |
| `set_collection_settings` compact arm (it sits in `set_live_collection_settings` since 0.7.1's by-ID entry points) | frozen (ERR-8) | done: the read-only gate (`250a98bc`) | | | |
| `search_in_at` compact arm | frozen | | integrated draft (lane A's patches: the declared postings with a cap of half the rows) | done: no edit needed | |
| `exact_search_compact` (`c859d2d5` adds `exact_search_compact_with_workspace`) | frozen | | draft (`c859d2d5`), cherry-picked unchanged | done: the loop walks `ordered_ids` (one line); a later change goes to the lane named in section 4 (L5, P5-71) | |
| `create_collection_as_occupied` and every non-compact function | out of scope | | | | |
| `start_rebuild_with_options_in`, `poll_rebuild_in`, `maintain_background_hnsw_in`, `search_ready_with_options_in`, `build_hnsw*`, the snapshot and disk entry points | the compact arms refuse with unchanged state (frozen) | | | no edit | reserved for the ANN and F5 lanes |

Other shared surfaces, reserved by function as in the inventory: `collection.rs::{exact_search_candidates,
rebuild_derived}` (B4, done; E4 made no edit here; A4's `c859d2d5` cherry-pick changed the compact call in `exact_search_candidates`, which now
passes the records to `exact_search_compact` instead of an iterator), `rerank.rs`, `metadata_index.rs`, `text_index.rs`, `quality.rs` (B4, then C4 consumes),
`kv.rs`, `hybrid.rs`, `query_language.rs`, `vectors.rs`, `property_graph/retrieve.rs` (A4, done: the changes that a release build compiles are
named on [A4's page](p3b-record-consumers.md) and checked by `scripts/shadow_copy_check.py`; K71's `kv.rs` and
`server/kv.rs` functions shipped in 0.7.1), `hnsw.rs`, `rebuild.rs`, `ann_snapshot.rs`, `disk_ann.rs`, `gpu.rs` (the ANN and F5 lanes;
their compact refusal arms are pinned by `compact_ann_paths_remain_refused_with_unchanged_state`),
`compact/mod.rs::ImageCheck`, `format.rs`, `maintenance.rs`, `directory.rs`, `changes.rs`, `logops.rs` (L5 and the
storage owner).

C4 wires the server to the by-ID entry points that 0.7.1's request guard added (`GuardedCollection` in `server/guard.rs` calls
them): `collection_by_id`, `collection_mut_by_id`, `drop_collection_by_id` and `set_settings_by_id` (`by_id.rs`), `kv_view_by_id`
and `kv_store_by_id` (`kv.rs`) and `build_disk_ann_by_id` (`disk_ann.rs`). None has a compact arm of its own on T, so a guarded
request reaches what the library's call by name reaches. `collection_by_id` and `collection_mut_by_id` return the same
`CollectionRef` and `CollectionMut` as `collection` and `collection_mut`, so a guarded read or write reaches the same compact arms
and refusals as a call by name. `drop_collection_by_id` reaches `drop_live_collection`. `set_settings_by_id` calls `live_collection` and then
`set_live_collection_settings`, the arm that the `set_collection_settings` row above and ERR-8 (section 3.3) cover.
`build_disk_ann_by_id` reaches `build_live_disk_ann`, which refuses a compact collection through `ensure_resident_in`, and
`kv_view_by_id` and `kv_store_by_id` refuse it through `check_kv_collection`. The MCP key-value tool
(`src/bin/tool_support/kv_mcp.rs`) calls the name-based `Database::kv_view` and `kv_store`, where A4's compact arm of `kv_view`
sits (under `cfg(test)`); `kv_store` still refuses compact records.

Lane A4's edits of reserved regions, and what it hands on (A4's page, sections 6, 7 and 9, has the detail):

- In `src/kv.rs` A4 changed functions that the map reserved for K71 (`probe`, `entry`, `get_at`, `visit_at` and `kv_view`) to
  one `acquire`, one `Rows` iterator and one `get_physical`. K71's visitor shipped in 0.7.1, so T carries A4's own two
  `kv.rs` commits (`33a28671` and `71cb3915`) on the visitor of `main` and not A4's copies of K71's first two commits, and
  the shadow-copy gate takes K71's visitor as the baseline of `visit_at` (its `--kv-base` argument).
- The server reaches the key-value store only through the guard's `kv_view` and `kv_store` (`server/guard.rs`), which call
  `Database::kv_view_by_id` and `kv_store_by_id` (`src/kv.rs`, which 0.7.1 added), and neither has a compact arm:
  `check_kv_collection` refuses a compact collection
  through `ensure_resident_in` (`UnsupportedFeature`), so on T a compact collection never reaches the key-value PUT.
  A4's page records a hazard for the day it does: the PUT computes its slot through `kv_view(..).put_slot_at` before it
  takes the writable store, so once a lane gives `kv_view_by_id` the compact arm that `Database::kv_view` has, a compact
  collection would be read and quota-checked before it is refused. Take the store first when `src/server/kv.rs` is touched
  for that (C4).
- The resident arms of seven functions exist twice, a `cfg(test)` branch and a `#[cfg(not(test))]` statement (ten pairs),
  and the compact copies `search_hybrid_compact` and `execute_compact` repeat the validation and fusion of the resident
  ones. `scripts/shadow_copy_check.py` makes the shipped copies a checked fact; an early-return dispatch that writes each
  body once is a follow-up lane before L5.

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
  ones counted.
- **Independent**, `faa3b32e`, the integrated tree of slices 2 and 3 (101 selected). The replayed checkout
  is that commit plus the replay tool and its mutant lists; no other file differs. Separate machines, 3 of
  them and none the lane's, replayed 35 mutants: all 34 that the lane's machine replayed on
  `5c982e99`, and `R0-EF-03`, which an existing test kills on `v0.7.0` and which the lane did not replay on a final
  tree. The selector was green before the first mutant and again after the last of every shard. All 35 are
  killed and every file restored byte-identically, and every killer that the ledger names for the 34
  fails again, so slice 3's additions to `source.rs` and `records.rs` did not weaken them. A mutant that counts on a
  final tree therefore has a kill on a machine that is not the lane's (the coordinator's rule, section 8). The other
  56 mutants, which an existing test kills on `v0.7.0` and which three other machines replayed there, were not replayed on
  R0's final trees. No existing test changed in R0's trees since `v0.7.0` (the changes are the new `freeze_tests.rs` and
  `ordered_tests.rs`, their module declarations and the test-gated cursors of 7.2), so the test that killed each of them
  on `v0.7.0` was still there. 20 of the 56 (the ones in `src/records.rs`) are replayed on T in 7.3. The other 36 (35 in
  `src/compact/source.rs` and 1 in `src/quality.rs`) have no run on a final tree, and replaying them there is open.

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

Seven of lane B's 18 replay obligations are mutations of this seam (the [ledgers](p3b-survivor-ledgers.md#lane-b)). R0
re-created each one as `R0-AU-08`, `R0-AU-13`, `R0-AU-14`, `R0-EF-02`, `R0-EX-05`, `R0-EX-06` and `R0-EX-07`; all seven
survive on `v0.7.0` and all seven are killed with the freeze tests. B4's ledger names them, with the 17 mutants of the
earlier seam campaign, as `reused`, and its `verify` mode checks that this ledger holds each of the 24 as a restored kill.
B4 also re-derived `R0-AU-01`, `R0-AU-02` and `R0-AU-12` from lane B's report (as `x-seam-frame-kind-check-removed-body`,
`x-seam-frame-kind-check-removed-decode` and `x-rec-vector-error-swallowed`, with the same replacements), so those three
are killed in both ledgers.

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
not that the statements are complete, and it supports no size or speed claim. When R0 and B4 were put together, every
anchor of the ledgers of 7.1 and 7.2 matched exactly once (B4's `python tests/compact_mutant_replay.py check <ledger>`
accepts both). A lane that changes the lines around one re-bases it and replays the mutant, and does not trust the
recorded result. `R0-EF-03` was re-based once, when slice 3 inserted the ordered cursors next to its old anchor: the
replacement is the same and gives the recorded mutated bytes, so its recorded hashes hold.

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
killed, every file restored byte-identically. Separate machines, 3 of them and none the lane's, then
replayed all 34 on `faa3b32e`, the integrated tree, whose source files are those of `f167cf3d`
(101 selected, green before the first mutant and again after the last of every shard): all 34 are
killed, every file restored byte-identically, and every killer that the ledger names for them in the lane's replay
fails again. Every mutant of slice 3 therefore has a kill on a machine that is not the lane's (the coordinator's rule,
section 8). The independent reviews of the first version of the tests
found four mutants that survived it: `S3-OR-12`, `S3-OR-14` and `S3-BR-13`, which decode and cache record cells that no
cursor may touch, and `S3-OR-13`, which lists the ID of a tombstone while a larger base row is pending. They are in the
list, the tests were strengthened until each is killed, and the whole list was replayed again on the final tests.

Not in the list, because no test can tell them from the code they replace:

- `self.next >= self.end` in `BaseRows::next` changed to `>` or removed, because `row_at` answers `None` at the end;
- `self.source?` in `BaseRows::next` changed to `.expect(..)`, because `end` is 0 whenever there is no source;
- `base_id == Some(overlay_id)` in the merge changed to `<=`, because that arm only sees `None` or an equal ID, and
  `next` on an exhausted `Peekable` does nothing.

These three guards are redundant defensive code. Deleting the `Clone` derive or the `FusedIterator` implementation of
`BaseRows` is a compile error and does not count as a kill; the trait set is checked at compile time by
`ordered_base_rows_list_every_physical_row_in_ascending_id_order`. Claims marked with a dagger in section 3.7 (no
allocation, no frame read or authentication) cannot be observed by a test through the seam and have no mutant.

### 7.3 The mutants of `records.rs` and `lib.rs` on the integration tree

7.1 and 7.2 replayed their mutants on R0's own trees. B4, A4 and E4 also change `src/records.rs` and `src/lib.rs`, and `verify` of
the replay tool calls a recorded run stale when the file it ran on differs from the file on the tree it checks, so the recorded
runs of the mutants of those two files say nothing about T until they are replayed there. The list of this section is the 52
mutants that mutate `src/records.rs` (50) or `src/lib.rs` (2): 31 of 7.1 (29 and 2) and the 21 of 7.2. It is
[`tests/compact_r0_integrated_mutants.json`](../../../tests/compact_r0_integrated_mutants.json), in the form of B4's ledger:

- The definitions (`id`, `desc`, `file`, `old`, `new`, `cmd`) are copied unchanged from the two older ledgers, and `check`
  confirms that every anchor matches exactly once on T.
- `expect` is derived by a script and not typed: the tests that failed in every record that the older ledger holds for the
  mutant on its latest recorded tree (7.2: `result` and `independent`; 7.1 on a final tree: `final` and `independent`; the
  re-based `R0-EF-03`: `independent`; the 20 mutants of 7.1 that only have a kill on `v0.7.0`: `baseline` and
  `baseline_lane`). No intersection is empty. A run counts only when it fails at least those tests (`expect_met`), so a
  mutant that some other, newer test happens to kill does not count.
- The selector is the one of 7.1: 204 tests on T, green before the first mutant and again after the last.
- Every mutant needs two clean kills (`two_machines`): one on the lane's machine and one on a machine that is not the lane's
  (the coordinator's rule, section 8). A kill is clean when the anchor matched once, the tree compiled, the selector ran, the
  expected tests failed and the file was restored byte-identically (SHA-256 over the bytes and over the LF-normalized text).

The 20 of the 52 that 7.1 only killed on `v0.7.0` (the baseline-only mutants of `records.rs`) had never run on a tree that
carries R0's tests; this is their first replay on a final tree. The other 36 baseline-only mutants of 7.1 are in
`src/compact/source.rs` (35) and `src/quality.rs` (1), which are byte-identical on T and on R0's last commit, and are not in
this list.

Every run is of the sources of T: `python tests/compact_mutant_replay.py sources` prints the fingerprint
`eaf4962586c77b556471e9c4b9c4b8addcff449b019309157d4f158279c7b549` (600 files) on T and on each machine's checkout.

| Run | Mutants | Clean kills | Not clean | The 204-test selector, before / after |
|---|---:|---:|---:|---|
| The lane's machine: one process, one mutant per build | 52 | 52 | 0 | green / green |
| Separate machines: three shards (18, 17 and 17 mutants) on two machines, one mutant per build | 52 | 52 | 0 | green / green in each shard |

By origin of the mutant:

| Mutants | Count | Clean kills, lane's machine | Clean kills, separate machines |
|---|---:|---:|---:|
| Slice 3 (7.2), `records.rs` | 21 | 21 | 21 |
| Slice 2 with a kill on a final tree: `records.rs` (8) and `lib.rs` (2) | 10 | 10 | 10 |
| Slice 2 with a re-based anchor: `R0-EF-03` | 1 | 1 | 1 |
| Slice 2 with a kill on `v0.7.0` only: `records.rs` | 20 | 20 | 20 |
| **Total** | **52** | **52** | **52** |

`python tests/compact_mutant_replay.py verify tests/compact_r0_integrated_mutants.json --strict`, run on T, accepts all 52
mutants: 104 valid runs, every one made on exactly the sources of T (52 accepted, 0 stale, 0 open), and `check` finds every
anchor once. The checkout of a separate machine was T plus one commit that adds the replay list and its instructions outside the
fingerprint, so the `head` in the `environment` of those runs is not T's commit, while their `sources_sha256` is T's.

What this does and does not establish. It establishes that the tests which killed these 52 mutants on R0's trees (and on
`v0.7.0` for the 20) still kill them on T, after B4's, A4's and E4's changes to the two files and their tests, on two machines.
It does not replay the other 73 mutants of R0's two ledgers (72 in `src/compact/source.rs` and 1 in `src/quality.rs`): those files
are byte-identical on T and on R0's last commit, and the independent runs of 7.1 and 7.2 on `faa3b32e` (separate machines,
37 of the 73) were made on that same file. The 36 baseline-only mutants among them have no run on a final tree on any machine
yet. It replays none of the other lanes' mutants.

A first attempt of one shard, on another machine, stopped before it applied any mutant: its offline build used an empty Cargo
cache and could not resolve a crate. That is an infrastructure failure and no result. The shard (17 mutants) ran again with the
machine's own Cargo cache, and nothing of the first attempt is in the ledger.

### 7.4 Probes of the scan statistics on T

Section 4 describes a difference in `records_scanned` between the compact and the resident exact search. B4's test pins it over
2,500 rows in three blocks, and E4's page (section 4) says that it exists "above one 1,024-row block" and "where no posting
serves the filter". The probes below test how far that goes. They are throwaway tests that R0 added to a scratch checkout of T
and built there (T itself was not changed and the probes are in no commit). They use B4's harness
(`src/compact/source/seam_tests/index_differential.rs`): one image opened as a compact source and as resident records, and the
same exact search (`search_in_at`, squared L2, `k` = 5, one clock) on both. In every probe the hits, `distance_evaluations`
and `index_candidates` were equal on the two arms, and only `records_scanned` differed.

**A single block.** 800, 1,024 and 1,025 rows with `n` = the row's ID, the collection declaring `n` for equality only (so that
no posting serves a range on it), and one range filter on `n`. `records_scanned`, compact arm / resident arm:

| Range on `n` | 800 rows | 1,024 rows | 1,025 rows |
|---|---|---|---|
| `10..=20` (11 rows) | 800 / 800 | 1,024 / 1,024 | 1,025 / 1,025 |
| every row | 800 / 800 | 1,024 / 1,024 | 1,025 / 1,025 |
| one row of the first block (`500..=500`) | 800 / 800 | 1,024 / 1,024 | 1,025 / 1,025 |
| the last row alone | 800 / 800 | 1,024 / 1,024 | 1,025 / 1 |
| above the interval of `n` (no row) | 800 / 0 | 1,024 / 0 | 1,025 / 0 |
| below the interval of `n` (no row) | 800 / 0 | 1,024 / 0 | 1,025 / 0 |

A range that misses the whole interval of a block prunes that block, and a single block is enough: the resident arm scans no
row where the compact arm scans every one. A range inside the first block of 1,025 rows prunes the one-row second block, but
the 1,024 rows that remain are more than half of the rows, so the summaries give no candidates and both arms scan every row; a
range that only the last row matches prunes the first block and leaves one row.

**A posting over the cap.** 2,500 rows with `n` = the row's ID, `tag` = `alpha` for the 1,500 rows whose ID modulo 10 is below
6 and `beta` for the other 1,000, and `tag` and `n` both declared for equality. The cap is half of the rows, 1,250. The posting
of `alpha` holds more than the cap, so it yields no candidates on either arm (`index_candidates` is 0), and the resident arm
falls back to the summaries of `n`:

| Filter | `records_scanned`, compact / resident | `index_candidates` | `distance_evaluations` |
|---|---|---|---|
| `tag = alpha` and `n` in `10..=20` | 2,500 / 1,024 | 0 | 7 |
| `tag = alpha` and `n` in `2400..=2499` | 2,500 / 452 | 0 | 60 |
| `tag = alpha` | 2,500 / 2,500 | 0 | 1,500 |
| `tag = beta` and `n` in `10..=20` | 1,000 / 1,000 | 1,000 | 4 |

The posting of `beta` holds 1,000 rows, within the cap, so both arms scan its rows and the statistics are equal. A posting that
serves a condition of the filter therefore does not by itself guarantee equal statistics: it must leave candidates.

What the probes do and do not show. They show that the difference needs neither a second block nor a missing posting, so the
lane that persists the summaries (P5-71) flips B4's test and should add these two cases next to it. They are not added now,
because a test is source that cargo builds and T is frozen. They do not change the decision of section 8 that the difference is
allowed (E4's amendment moves the compact summary fallback to P5-71), they pin nothing, they ran once, and they measure T only.

## 8. Decisions

Decided on 2026-11-05 by the coordinator under the owner's delegation, as recommended.

- **OPEN-71-01, activation: private integration only.** Nothing in 0.7.1 turns the compact seam on, and 0.7.1 shipped without
  the compact work (section 1). The production compact writer, float16 and temporal copy-out stay opt-in and closed, no
  setting, option or file content enables a gate, and every statement on this page describes private, test-gated code
  (section 1), except the few resident-visible changes of lane A4 that CLK-7 and section 4 name. What the release that
  integrates these lanes activates is that release's decision ([handover-0.7.1.md](handover-0.7.1.md), 0.7.2 plan, item 2).
- **OPEN-71-02, survivor evidence: approved.** The evidence form in the [ledgers](p3b-survivor-ledgers.md#evidence-that-would-settle-open-71-02)
  is the acceptance form: one record per mutant with before and after SHA-256, the failing test names and a restored-green
  run, replayed independently. R0's ledgers have that shape with one qualification, which the coordinator accepted on
  2026-11-05: the restored-green run is recorded once per replay (before the first mutant and after the last) and not
  after every mutant, because a byte-identical restore, which the ledger records per mutant, returns the tree whose
  baseline run was green, and a run per mutant would double the number of builds. The coordinator did not accept a final-tree
  replay on the lane's machine alone: a mutant that counts on a final tree needs a kill on a machine that is not the lane's.
  R0's ledgers meet that rule. The baseline on `v0.7.0` ran on three other machines for 87 of the 91 mutants of slice 2
  (86 of those also on the lane's), and every mutant that counts on a final tree, the 34 of slice 2 that the lane's machine
  replayed, `R0-EF-03` and all 34 of slice 3, was replayed again on separate machines on the integrated tree (7.1 and
  7.2): 69 mutants, all killed. On T, the 52 mutants of `records.rs` and `lib.rs` (31 of slice 2 and 21 of slice 3) were
  replayed again, on the lane's machine and on separate machines, all killed, and `verify --strict` accepts the ledger of that
  replay (7.3). An equivalence claim
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
  cursors, are practical. The coordinator accepted R0's reading of the minimum of 12 on 2026-11-05: the 28 gaps count by the
  approved definition, 12 of them are reachable without stored-byte or directory edits and 10 from states the product can
  produce, all stated in 7.1.
- **Scan statistics: a known difference, not a defect.** The difference in `records_scanned` between the compact and the
  resident filtered exact search for a filter that block summaries could prune and no declared posting narrows, which section 4
  describes, is not a defect of the code that 0.7.1 shipped (it shipped without the compact work). E4's amendment, a decision
  of the release coordinator, moves the compact summary fallback to P5-71 ([E4's page](p3b-exact-scan.md), section 4). The
  difference is not limited to collections above one block (section 4).
- **Amendment of CLK-7 after the freeze.** A4 changed the one statement of section 3 that its work touches. CLK-7 now says
  that the exact vector leg and the text leg of the hybrid search and the pivot and evidence reads of `GraphRef::retrieve`
  read at one clock, and its list of resident arms that sample per candidate or per call changed: the hybrid text leg left
  it, and `reembed_pending`, which a reading of the resident arms found to sample per row (A4's page reports it too), joined
  it. The first sentence of CLK-7 and every other statement of section 3 are unchanged. The rule of section 3 is met: this
  amendment, the tests that CLK-7 now names and A4's mutants of the new behavior (`pa-hybrid-resident-default-clock-zero`,
  `a4-retrieve-resident-pivot-search-wall-clock` and `a4-retrieve-resident-revalidation-wall-clock`).

R0's own choices in slice 3 were not coordinator decisions. The coordinator accepted them on 2026-11-05 as coded and
reviewed, by three independent reviewers:

- **Slice 3 shapes.** `base_rows` returns a named cursor (`BaseRows`) and `ordered_ids` a named cursor (`OrderedIds`)
  rather than the approved `impl Iterator` returns, so that the merge holds the first by value without boxing and a
  later lane can name both. `affected_ids` keeps its approved `impl Iterator` return and takes no `S` (section 5), and
  none of the cursors seeks (section 4). The shapes stay additive and crate-private. E4 consumed `ordered_ids` as coded and
  asked for no change; a later lane that needs a different shape changes it through an amendment to this page.

## 9. Verification

```text
cargo test --locked --offline --lib -- compact::source records:: metadata_index::tests::compact rerank::
cargo test --locked --offline --all-features --lib -- compact
cargo fmt --all -- --check
cargo clippy --locked --offline --workspace --all-targets --all-features -- -D warnings
python tests/compact_method_matrix.py
python tests/compact_mutant_replay.py check tests/compact_seam_mutants.json
python tests/compact_mutant_replay.py check tests/compact_ordered_cursors_mutants.json
python tests/compact_mutant_replay.py check tests/compact_r0_integrated_mutants.json
python tests/compact_mutant_replay.py verify tests/compact_r0_integrated_mutants.json --strict
```

The mutant list replays with any runner that applies one anchored replacement at a time (LF-normalized match, line
endings preserved on write), runs `cmd`, classifies the outcome and restores the file; the ledger records the
selector, the failing tests and the SHA-256 of the file before, during and after. Its records have the shape
`{id, desc, file, old, new, cmd}` plus the evidence, so B4's `tests/compact_mutant_replay.py` reads them: its `check`
mode (the first three commands above) confirms that every anchor matches once.

B4's `verify` mode does not read the evidence of the first two ledgers: it judges the `runs` records that the lists of B4 and
A4 keep, so on these ledgers it reports every mutant open, with no valid run. It reads the seam ledger for one purpose, to
check that it holds the 24 mutants that B4's list reuses: the seven obligations, each with a final-tree run that must be a
restored kill, and the 17 mutants of the earlier seam campaign, which carry their baseline kill on `v0.7.0` and no
final-tree run.

The third ledger (7.3) has the `runs` shape, so the last command judges it. With `--strict` a mutant is accepted only when both
of its runs (`local` and `second`) are kills with the whole evidence and were made on exactly the sources of the tree that
`verify` runs in. Those are T's: the fingerprint that `python tests/compact_mutant_replay.py sources` prints does not cover
documents or `tests/*.json`, so a commit of documents and ledgers leaves it unchanged. On T it accepts all 52 mutants of the
third ledger (104 valid runs, 0 stale, 0 open).
