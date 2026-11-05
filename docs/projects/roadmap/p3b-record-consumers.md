# P3b slice 4, lane A4: record consumers over compact records

| | |
|---|---|
| Status | Integrated behind the test gates on `mahpate-r071-a4`. Nothing compact becomes active: every compact arm below is `cfg(test)`, and no default, file format or wire shape changes. Replaces the parked lane A (`origin/mahpate-p3b-consumers` @ `834a0a6`) |
| Scope | Key-value reads, hybrid search, the query language, grouped search and facets, the record iterator and browse, named vectors and re-embedding, graph evidence, and the exact-scan draft |
| Out of scope | Named ANN and every other HNSW, snapshot, rebuild or disk-ANN path (they stay refused on compact and belong to [P3c](p3c-compact-ann.md)); the exact scan beyond the lane A draft (lane E4 owns it); the seam itself (`src/compact/source.rs`, lane R0) |
| Reads with | The [seam freeze](p3b-seam-freeze.md) (the frozen semantics CLK, PRE, ERR, ACC and IDN are cited below), the [survivor ledgers](p3b-survivor-ledgers.md#lane-a) and the [P3 design](p3-compact-vectors.md), slice 4 and table J |

## 1. What was integrated, and how

Lane A's four patches that were not in `v0.7.0` are cherry-picked in order with `git cherry-pick -x`, never merged, onto
the lane B read seam (`250a98b`, through lane B4). The key-value hunks are held back and rebuilt as the last group.

| Lane A patch | Form in A4 |
|---|---|
| `0206205` Add compact record consumers | Cherry-pick without the key-value hunks |
| `60d65f9` Align compact KV dispatch test with lane B | In the key-value group |
| `c859d2d` Use bounded vector workspace for compact exact scans | Cherry-pick, unchanged: a **draft**. Lane E4 becomes its only owner and refines it (one serial 4 MiB vector workspace, exact distance bits, validation, statistics, captured clocks). A4 does not extend it |
| `834a0a6` P3b slice 4 lane A fix-forward | Cherry-pick without the key-value hunks |

The key-value group sits on the bounded visitor of lane K71 (`KvView::visit_at`, candidate set of `limit + 1` keys):
K71's two `src/kv.rs` commits (`6167835`, `7592127`) are cherry-picked first, so that a lane that integrates K71 first
can skip them, then one commit adds the compact arm as a single dispatch point in that visitor and one commit adds the
tests. There is no second list implementation.

A4 changes two things in production code, both of them in the test-gated compact path:

1. `src/kv.rs`: one `acquire` (health for a compact collection, residency for a resident one), one `Rows` iterator
   (`compact_iter` or the resident map) and one `probe`/`entry` through `CollectionRecords::get_physical`. Lane A's
   separate compact `probe_compact`, `entry_compact` and `list_at` are gone, and with them lane A's compact list, which
   lacked the `v0.7.0` rule "a key is listed only through the record that `get` resolves for it". The redundant health
   check in `Database::kv_view` is gone: `Database::resolve` already makes it.
2. `src/vectors.rs`: the compact arm of `reembed_pending` checked its limit after it had read the next row. A call with
   limit `n` decoded the row after the `n`th hit and a call with limit 0 decoded the first row. It now checks first.

All other production code is lane A's, byte for byte. A4 adds 62 tests in eight sibling modules under
`src/compact/source/overlay_tests/` (`consumer_{cursor,group,exact,hybrid,query,vectors,graph,kv}.rs`) and the key-value
fixture `src/compact/catalog/tests/kv_windows.rs`.

## 2. The rules the tests check

Every consumer is compared with the resident store opened from the same image with source installation off, at the
same clock, bit for bit where floats are involved, and against hand-computed literals (the oracle is never the only
check, because both forms share code in places). A test that has to look at the work a call did uses the decode
counter of the generation or the materialized cells of a row.

| Consumer | Rules (frozen semantics in brackets) | Tests |
|---|---|---|
| `KvView::{get_at, list_at, visit_at}`, the probe behind `KvStore` (`src/kv.rs`) | Health, then dimension, then arguments, then reads (ERR-8). The probe window is exactly `KV_PROBE_SLOTS` (16) slots; the first slot that decodes to the key in its namespace wins, so look-alikes inside the window, behind a holder or in another namespace are passed over and those outside it are not found. A listing offers a key only through the record `get` resolves, is in key order, strictly after the cursor, and its limit counts offered entries only. A deadline equal to the clock hides an entry (CLK-2). A damaged base row is an error, never an absent key (ERR-4) | `compact_kv_point_and_prefix_match_resident` (table J, both source precisions), `consumer_kv` (6) |
| `search_hybrid_at`, `search_hybrid_compact`, `search_hybrid` (`src/hybrid.rs`) | One captured clock for both legs (CLK-1, CLK-7); `search_hybrid` samples the wall clock once (the resident correction). Health, then validation in the resident order, with the declared text fields. The vector leg keeps the full candidate depth; the visible set of the text leg is the effective rows that pass the deadline and the filter; fusion scores, distances and BM25 equal the resident values bit for bit; equal fused scores break by ascending ID; the fused list is cut to `k`. The approximate leg stays refused (`UnsupportedFeature(FLAG_COMPACT)`) | `consumer_hybrid` (15) and the lane A tests in `overlay_tests.rs` |
| `execute`, `execute_compact` (`src/query_language.rs`) | Health first, and an invalid query is refused before any row is decoded. The vector leg has the full candidate depth, `ORDER BY` compares each row with the other, `LIMIT` is applied, groups and facets count the effective rows | `consumer_query` (5) |
| `CollectionRef::{search_grouped, facets}` (`src/collection.rs`, `src/records.rs`) | The nearest hit leads each group, group and per-group limits, typed value counting, the AND filter over overlay rows, validation before any row is decoded, health first, base-row read errors returned | `consumer_group` (7) |
| `compact_iter`, `CompactRecordIter`, `compact_browse` (`src/records.rs`) | A cursor seeks by binary search over the base rows and merges the overlay in ascending ID order; a tombstone or an expired winner is skipped and never falls through (CLK-4, PRE-1); pages chain like the resident ones at every clock; a snapshot keeps the state it captured; read errors are items, never skipped rows | `consumer_cursor` (8) |
| `record_vectors`, `search_named_at`, `reembed_pending(_checked)` (`src/vectors.rs`) | A named vector is visible only while its primary row is visible at the one clock; a poisoned cell or a tampered block is an error; no base row that the overlay shadows is read; re-embedding lists the effective rows without the target version in ascending order, within its limit and without decoding a row beyond it; named ANN stays refused with nothing built | `compact_named_vector_visibility_matches_resident` (table J, `full` and `half`), `compact_named_vector_search_propagates_primary_read_error`, `consumer_vectors` (11) |
| `GraphRef::retrieve` evidence reads (`src/property_graph/retrieve.rs`) | Pivots and evidence are read at the one retrieval clock; only the requested evidence collections are read and only the attached rows that are projected are decoded; errors propagate; at most 64 unmapped pivots are listed and all are counted | `consumer_graph` (9) |
| `Database::search_in_at`, compact arm; `exact_search_compact` (`src/lib.rs`) | Draft. Hits, order and statistics equal the resident ones; a declared posting narrows the scan only when it holds at most half of the rows | `consumer_exact` (1) and the lane A tests |

## 3. Table J obligations that exist only on lane A

| Test | What it covers |
|---|---|
| `compact_kv_point_and_prefix_match_resident` | Runs on both source precisions through a rewrite, an overlay-only key, a deadline-only write, the deletion of a holder and of a displaced key, at every clock around the deadlines: expiry equality, absent and tombstoned winners, namespace and cursor ordering, against the resident store and against hand-computed lists |
| `compact_named_vector_visibility_matches_resident` | Runs for the float32 (`full`) and the float16 (`half`) collection |
| `compact_named_vector_search_propagates_primary_read_error` | The primary-read-error regression; `consumer_vectors` adds the poisoned-cell and tampered-block cases for the other consumers |

## 4. The survivors of lane A, measured

Lane A's ledger lists 47 mutants, 26 of them survivors. Each was re-created from its recorded description and anchored
on the tree after the cherry-picks (`tests/compact_a4_mutants.json`, `first_replay` is the first replay on that tree,
before the A4 tests). On that tree 40 of the 47 were already killed and 7 survived: `pa-exact-cap-quarter`, `pa-graph-evidence-wall-clock`, `pa-group-clock-zero`, `pa-kv-probe-reads-base-slot-only`, `pa-kv-view-health-removed`, `pa-ql-clock-zero`, `pa-ql-order-key-reads-b`. Three of them, `pa-group-clock-zero`, `pa-ql-clock-zero` and `pa-exact-cap-quarter`, are recorded as killed in lane A's ledger; their tests used far-future deadlines only (and a quarter of the rows as the index cap satisfied the same cases as half). `pa-kv-probe-reads-base-slot-only` survived because the fixture of the named test never produced an image in which a key is displaced; `pa-ql-order-key-reads-b` because the named test does not fail on it; `pa-kv-view-health-removed` because the check it removed was dead code (`Database::resolve` checks health first), so the line is gone and the mutant is re-anchored on `resolve`; and `pa-graph-evidence-wall-clock` is lane A's equivalence claim (section 6). Every one of the 47 is killed on the final tree, by the tests in the last column; the 26 survivors of lane A's ledger are also replayed on a second machine.

| Mutant | File | Lane A's bucket | Replay on the tree after the cherry-picks | A4 | Closing tests |
|---|---|---|---|---|---|
| `pa-exact-cap-whole` | `lib.rs` | single replay | killed by `compact_exact_search_candidate_and_stats_match_declared_index` | killed on both machines | `compact_exact_search_candidate_and_stats_match_declared_index`, `compact_exact_search_uses_declared_postings_up_to_half_of_the_rows`, `hybrid_filter_applies_to_both_legs_and_the_vector_leg_reports_its_work` |
| `pa-hybrid-resident-default-clock-zero` | `hybrid.rs` | single replay | killed by `hybrid_resident_default_clock_honors_ttl` | killed on both machines | `hybrid_resident_default_clock_honors_ttl` |
| `pa-facets-compact-conditions-ignored` | `records.rs` | bundle replay | killed by `compact_grouped_search_and_facets_match_resident` | killed on both machines | `compact_grouped_search_and_facets_match_resident`, `compact_grouped_search_and_facets_apply_the_and_filter_to_overlay_rows` |
| `pa-group-depth-one` | `records.rs` | bundle replay | killed by `compact_dispatch_grouped_facets_match_nonempty_resident_rows`, `compact_grouped_search_and_facets_match_resident` | killed on both machines | `compact_grouped_search_and_facets_match_resident`, `compact_grouped_search_and_facets_apply_the_and_filter_to_overlay_rows`, `compact_grouped_search_and_facets_return_base_read_errors` |
| `pa-hybrid-compact-health-removed` | `hybrid.rs` | bundle replay | killed by `compact_hybrid_rejects_poisoned_text_only_search` | killed on both machines | `compact_hybrid_rejects_poisoned_text_only_search`, `hybrid_health_precedes_every_validation_and_every_valid_query` |
| `pa-hybrid-compact-k-not-truncated` | `hybrid.rs` | bundle replay | killed by `compact_hybrid_text_leg_matches_resident_after_overlay_resolution` | killed on both machines | `compact_hybrid_text_leg_matches_resident_after_overlay_resolution`, `hybrid_deadline_only_overlay_write_applies_its_deadline_to_both_legs`, `hybrid_disabled_legs_are_never_evaluated` |
| `pa-hybrid-compact-tie-break-desc` | `hybrid.rs` | bundle replay | killed by `compact_hybrid_text_leg_matches_resident_after_overlay_resolution` | killed on both machines | `hybrid_deadline_only_overlay_write_applies_its_deadline_to_both_legs`, `hybrid_equal_fused_scores_break_ties_by_ascending_id_across_the_legs`, `hybrid_equal_fused_scores_of_rows_in_both_legs_break_ties_by_ascending_id` |
| `pa-hybrid-undeclared-text-fields-allowed` | `hybrid.rs` | bundle replay | killed by `compact_hybrid_requires_declared_text_fields` | killed on both machines | `compact_hybrid_requires_declared_text_fields`, `hybrid_disabled_legs_are_never_evaluated`, `hybrid_validation_matches_resident_in_check_order` |
| `pa-hybrid-visible-conditions-ignored` | `hybrid.rs` | bundle replay | killed by `compact_hybrid_text_leg_matches_resident_after_overlay_resolution` | killed on both machines | `compact_hybrid_text_leg_matches_resident_after_overlay_resolution`, `hybrid_filter_applies_to_both_legs_and_the_vector_leg_reports_its_work` |
| `pa-iter-overlay-cursor-included` | `records.rs` | bundle replay | killed by `compact_browse_cursor_skips_tombstones_and_expiry` | killed on both machines | `compact_browse_cursor_skips_tombstones_and_expiry`, `compact_browse_pages_chain_like_the_resident_records_at_every_clock`, `compact_iter_overlay_around_the_base_matches_resident` |
| `pa-iter-search-high-short` | `records.rs` | bundle replay | killed by `compact_browse_cursor_skips_tombstones_and_expiry` | killed on both machines | `compact_browse_cursor_skips_tombstones_and_expiry`, `compact_browse_pages_chain_like_the_resident_records_at_every_clock`, `compact_iter_overlay_around_the_base_matches_resident` |
| `pa-iter-search-step-two` | `records.rs` | bundle replay | killed by `compact_browse_cursor_skips_tombstones_and_expiry` | killed on both machines | `compact_browse_cursor_skips_tombstones_and_expiry`, `compact_browse_pages_chain_like_the_resident_records_at_every_clock`, `compact_iter_overlay_around_the_base_matches_resident` |
| `pa-kv-get-key-validation-removed` | `kv.rs` | named test only | killed by `compact_kv_point_and_prefix_match_resident` | killed on both machines | `compact_kv_point_and_prefix_match_resident`, `compact_kv_view_checks_health_then_dimension_then_arguments` |
| `pa-kv-list-after-inclusive` | `kv.rs` | named test only | killed by `compact_kv_point_and_prefix_match_resident` | killed on both machines | `compact_kv_point_and_prefix_match_resident`, `compact_kv_list_offers_only_the_record_get_resolves` |
| `pa-kv-list-limit-plus-one` | `kv.rs` | named test only | killed by `compact_kv_point_and_prefix_match_resident` | killed on both machines | `compact_kv_point_and_prefix_match_resident`, `compact_kv_list_offers_only_the_record_get_resolves` |
| `pa-kv-list-namespace-ignored` | `kv.rs` | named test only | killed by `compact_kv_point_and_prefix_match_resident` | killed on both machines | `only_candidates_that_can_be_entries_are_probed` |
| `pa-kv-list-namespace-validation-removed` | `kv.rs` | named test only | killed by `compact_kv_point_and_prefix_match_resident` | killed on both machines | `compact_kv_point_and_prefix_match_resident`, `compact_kv_view_checks_health_then_dimension_then_arguments` |
| `pa-kv-list-zero-limit-allowed` | `kv.rs` | named test only | killed by `compact_kv_point_and_prefix_match_resident` | killed on both machines | `compact_kv_point_and_prefix_match_resident`, `compact_kv_view_checks_health_then_dimension_then_arguments` |
| `pa-kv-probe-reads-base-slot-only` | `kv.rs` | named test only | survived | killed on both machines | `compact_kv_point_and_prefix_match_resident`, `compact_kv_list_offers_only_the_record_get_resolves`, `compact_kv_probe_window_is_exactly_sixteen_slots` |
| `pa-kv-view-health-removed` | `lib.rs` | named test only | survived | killed on both machines | `compact_kv_point_and_prefix_match_resident`, `compact_kv_view_checks_health_then_dimension_then_arguments` |
| `pa-named-search-ignores-clock` | `vectors.rs` | named test only | killed by `compact_named_vector_visibility_matches_resident` | killed on both machines | `compact_named_vector_visibility_matches_resident`, `compact_named_search_matches_resident_at_every_deadline_boundary` |
| `pa-ql-limit-not-applied` | `query_language.rs` | named test only | killed by `compact_query_language_matches_resident_after_overlay_resolution` | killed on both machines | `compact_query_language_matches_resident_after_overlay_resolution`, `query_clauses_match_resident_before_and_after_overlay_mutations`, `query_named_vectors_match_resident_on_the_full_collection` |
| `pa-ql-order-key-reads-b` | `query_language.rs` | named test only | survived | killed on both machines | `query_clauses_match_resident_before_and_after_overlay_mutations` |
| `pa-ql-vector-depth-one` | `query_language.rs` | named test only | killed by `compact_query_language_matches_resident_after_overlay_resolution`, `compact_query_language_vector_leg_uses_full_candidate_depth` | killed on both machines | `compact_query_language_vector_leg_uses_full_candidate_depth`, `query_clauses_match_resident_before_and_after_overlay_mutations`, `query_hides_expired_and_deleted_rows_in_every_output_and_shows_the_rest` |
| `pa-reembed-limit-never-breaks` | `vectors.rs` | named test only | killed by `compact_named_vector_visibility_matches_resident` | killed on both machines | `compact_named_vector_visibility_matches_resident`, `compact_reembed_pending_decodes_no_row_beyond_its_limit`, `compact_reembed_pending_lists_the_effective_rows_without_the_target_version` |
| `pa-graph-evidence-wall-clock` | `property_graph/retrieve.rs` | equivalent claim | survived | killed on both machines | `compact_graph_evidence_is_read_at_the_retrieval_clock_even_if_an_attached_row_had_a_deadline` |
| `pa-ann-resident-guard-removed` | `vectors.rs` | recorded as killed | killed by `compact_installed_collection_method_matrix` | killed | `compact_named_ann_stays_refused_and_leaves_the_state_unchanged`, `compact_named_vector_consumers_check_health_before_anything_else` |
| `pa-exact-cap-quarter` | `lib.rs` | recorded as killed | survived | killed | `compact_exact_search_uses_declared_postings_up_to_half_of_the_rows`, `hybrid_filter_applies_to_both_legs_and_the_vector_leg_reports_its_work`, `hybrid_validation_matches_resident_in_check_order` |
| `pa-exact-compact-arm-off` | `collection.rs` | recorded as killed | killed by `compact_dispatch_grouped_facets_match_nonempty_resident_rows`, `compact_dispatch_unqualified_scans_and_derived_reads_refuse` and 11 more | killed | `compact_exact_search_uses_declared_postings_up_to_half_of_the_rows`, `an_attached_record_never_carries_a_deadline_on_compact_or_resident_records`, `compact_graph_access_and_retrieval_answer_poisoned_on_an_unhealthy_handle` |
| `pa-graph-compact-pivot-state-off` | `property_graph/retrieve.rs` | recorded as killed | killed by `compact_graph_retrieve_projects_effective_row_at_one_clock` | killed | `an_attached_record_never_carries_a_deadline_on_compact_or_resident_records`, `compact_graph_access_and_retrieval_answer_poisoned_on_an_unhealthy_handle`, `compact_graph_evidence_decodes_only_the_attached_rows_it_projects` |
| `pa-graph-revalidate-wall-clock` | `property_graph/retrieve.rs` | recorded as killed | killed by `compact_graph_retrieve_projects_effective_row_at_one_clock` | killed | `compact_graph_evidence_is_read_at_the_retrieval_clock_even_if_an_attached_row_had_a_deadline`, `compact_graph_retrieval_uses_one_explicit_clock_for_pivots_and_evidence` |
| `pa-group-clock-zero` | `records.rs` | recorded as killed | survived | killed | `compact_grouped_search_and_facets_apply_the_and_filter_to_overlay_rows`, `compact_grouped_search_follows_the_nearest_hit_and_the_group_limits` |
| `pa-group-record-arm-off` | `records.rs` | recorded as killed | killed by `compact_dispatch_grouped_facets_match_nonempty_resident_rows`, `compact_dispatch_unqualified_scans_and_derived_reads_refuse` and 2 more | killed | `compact_grouped_search_and_facets_apply_the_and_filter_to_overlay_rows`, `compact_grouped_search_and_facets_return_base_read_errors`, `compact_grouped_search_and_facets_validate_before_materializing_rows` |
| `pa-hybrid-compact-dispatch-off` | `hybrid.rs` | recorded as killed | killed by `compact_graph_retrieve_projects_effective_row_at_one_clock`, `compact_hybrid_requires_declared_text_fields` and 5 more | killed | `an_attached_record_never_carries_a_deadline_on_compact_or_resident_records`, `compact_graph_access_and_retrieval_answer_poisoned_on_an_unhealthy_handle`, `compact_graph_evidence_decodes_only_the_attached_rows_it_projects` |
| `pa-iter-cursor-lt` | `records.rs` | recorded as killed | killed by `compact_browse_cursor_skips_tombstones_and_expiry` | killed | `compact_browse_pages_chain_like_the_resident_records_at_every_clock`, `compact_iter_overlay_around_the_base_matches_resident`, `compact_iter_read_errors_are_items_and_browse_returns_them` |
| `pa-kv-list-compact-arm-off` | `kv.rs` | recorded as killed | killed by `compact_kv_point_and_prefix_match_resident` | killed | `compact_kv_list_offers_only_the_record_get_resolves`, `compact_kv_probe_window_is_exactly_sixteen_slots`, `compact_kv_reads_report_base_row_errors` |
| `pa-kv-list-expiry-dropped` | `kv.rs` | recorded as killed | killed by `compact_kv_point_and_prefix_match_resident` | killed | `compact_kv_list_offers_only_the_record_get_resolves` |
| `pa-kv-list-sort-desc` | `kv.rs` | recorded as killed | killed by `compact_kv_point_and_prefix_match_resident` | killed | `compact_kv_list_offers_only_the_record_get_resolves` |
| `pa-kv-list-version-zero` | `kv.rs` | recorded as killed | killed by `compact_kv_point_and_prefix_match_resident` | killed | `compact_kv_list_offers_only_the_record_get_resolves` |
| `pa-kv-store-resident-guard-removed` | `kv.rs` | recorded as killed | killed by `compact_dispatch_mutable_acquisition_keeps_other_refusals`, `compact_installed_database_and_snapshot_method_matrix` | killed | `compact_dispatch_mutable_acquisition_keeps_other_refusals` |
| `pa-kv-view-dimension-check-removed` | `kv.rs` | recorded as killed | killed by `compact_dispatch_mutable_acquisition_keeps_other_refusals`, `compact_installed_database_and_snapshot_method_matrix` | killed | `compact_kv_view_checks_health_then_dimension_then_arguments` |
| `pa-ql-clock-zero` | `query_language.rs` | recorded as killed | survived | killed | `query_clauses_match_resident_before_and_after_overlay_mutations`, `query_hides_expired_and_deleted_rows_in_every_output_and_shows_the_rest`, `query_named_vectors_match_resident_on_the_full_collection` |
| `pa-ql-compact-dispatch-off` | `query_language.rs` | recorded as killed | killed by `compact_installed_restricted_method_guards_refuse`, `compact_query_language_matches_resident_after_overlay_resolution` and 2 more | killed | `query_clauses_match_resident_before_and_after_overlay_mutations`, `query_errors_match_resident_and_precede_row_materialization`, `query_hides_expired_and_deleted_rows_in_every_output_and_shows_the_rest` |
| `pa-ql-facet-first-count` | `query_language.rs` | recorded as killed | killed by `compact_query_language_matches_resident_after_overlay_resolution` | killed | `query_clauses_match_resident_before_and_after_overlay_mutations`, `query_hides_expired_and_deleted_rows_in_every_output_and_shows_the_rest`, `query_named_vectors_match_resident_on_the_full_collection` |
| `pa-ql-visible-metadata-dropped` | `query_language.rs` | recorded as killed | killed by `compact_query_language_matches_resident_after_overlay_resolution` | killed | `query_clauses_match_resident_before_and_after_overlay_mutations`, `query_hides_expired_and_deleted_rows_in_every_output_and_shows_the_rest`, `query_named_vectors_match_resident_on_the_full_collection` |
| `pa-reembed-payload-dropped` | `vectors.rs` | recorded as killed | killed by `compact_named_vector_visibility_matches_resident` | killed | `compact_named_vector_consumers_read_no_base_row_that_the_overlay_shadows`, `compact_reembed_pending_lists_the_effective_rows_without_the_target_version` |
| `pa-reembed-target-match-inverted` | `vectors.rs` | recorded as killed | killed by `compact_named_vector_visibility_matches_resident` | killed | `compact_named_vector_consumers_propagate_a_poisoned_cell`, `compact_named_vector_consumers_read_no_base_row_that_the_overlay_shadows`, `compact_reembed_pending_decodes_no_row_beyond_its_limit` |

## 5. The A4 mutants

A4 adds 66 mutants to the 47 of lane A, 113 in all. Every record in `tests/compact_a4_mutants.json` is one anchored replacement that was replayed with the tool of section 8: 70 on the machine that wrote the tests and 71 on a second machine, all killed, with the evidence of the 0.7.1 acceptance form (the anchor matched once, the mutated tree compiled, the named tests failed, the SHA-256 of the file before, mutated and restored, the unmodified selectors green before the first and after the last mutant of the replay, one mutant per build). `expect` names tests that failed on every replay.

| Group | Mutants | Files | Killed here | Killed on a second machine |
|---|---|---|---|---|
| A4 key-value tests | 13 | `kv.rs` | 13 | 0 |
| A4 exact scan and hybrid lead tests | 10 | `hybrid.rs`, `lib.rs`, `vectors.rs` | 10 | 2 |
| A4 hybrid and query-language tests | 43 | `hybrid.rs`, `query_language.rs` | 0 | 43 |

## 6. Findings and limits that remain

Gaps in lane A's own evidence, now closed:

- The fixture of lane A's key-value test never displaced a key. Its in-memory edit of a slot never reaches the image
  (the image is rebuilt from the backing file), so `pa-kv-probe-reads-base-slot-only` survived the named test.
  `consumer_kv` builds the image from committed records only and gives every probe window each kind of neighbour.
- `compact_hybrid_ties_order_by_id`, the test that lane A's ledger names for fused-score ties, puts two rows at different
  ranks in both legs, so it never produces a tie. The mutant `pa-hybrid-compact-tie-break-desc` is killed by
  `compact_hybrid_text_leg_matches_resident_after_overlay_resolution` and by the tie tests of `consumer_hybrid`, which
  build ties on purpose (a vector-only and a text-only hit at one rank; two rows that swap ranks between the legs, cut
  at every `k`).
- Lane A recorded `pa-group-clock-zero`, `pa-ql-clock-zero` and `pa-exact-cap-quarter` as killed. On the tree after the
  cherry-picks they survived: their tests used far-future deadlines only, and an index cap of a quarter of the rows
  satisfied the same cases as half of them. They are killed by the new tests: `pa-group-clock-zero` by `compact_grouped_search_and_facets_apply_the_and_filter_to_overlay_rows`, `compact_grouped_search_follows_the_nearest_hit_and_the_group_limits`; `pa-ql-clock-zero` by `query_clauses_match_resident_before_and_after_overlay_mutations`, `query_hides_expired_and_deleted_rows_in_every_output_and_shows_the_rest`; `pa-exact-cap-quarter` by `compact_exact_search_uses_declared_postings_up_to_half_of_the_rows`, `hybrid_filter_applies_to_both_legs_and_the_vector_leg_reports_its_work`.
- The compact `reembed_pending` decoded one row beyond its limit (fixed, section 1).
- `pa-ql-order-key-reads-b` (`ORDER BY` compares a row with itself) survived the test that lane A names for it. The clause
  table of `consumer_query` sorts by metadata fields, with rows that lack the field, and kills it.

The equivalence claim of lane A, `pa-graph-evidence-wall-clock` ("evidence is read at the wall clock, not the retrieval
clock", with the argument that a graph-attached record cannot expire), is not a code-level equivalence: a test that
injects the forbidden state through `Collection::apply_committed` kills it
(`compact_graph_evidence_is_read_at_the_retrieval_clock_even_if_an_attached_row_had_a_deadline`). It is an invariant:
no public path gives an attached record a deadline. `an_attached_record_never_carries_a_deadline_on_compact_or_resident_records`
pins it (`put_with_expiry` of an attached record is refused with `GraphAttachedRecordExpiry` and writes nothing; every
other write path leaves no deadline, enumerated with an exhaustive match on `Mutation` so a new variant fails the
test; attaching a record that has a deadline is `GraphRecordNotAttachable`). The independent review should read that
enumeration. `pa-kv-view-health-removed` was the other equivalent mutant: the check it removed was dead code, because
`Database::resolve` checks health first, so A4 removed the line and the mutant is re-based onto `resolve`.

Limits that A4 leaves as they are, each with the owner that can change it:

- The approximate leg of hybrid search samples its own wall clock (`search_ready_with_options_in`); compact hybrid
  refuses that leg, so only the resident path is affected (freeze note, section 4).
- `GraphRef::retrieve` validates its limits and ceilings before it checks health, so a handle that becomes unhealthy
  after `graph()` (a failed flusher) answers `GraphLimitExceedsCeiling` for an oversize request, not `Poisoned` (ERR-8).
  It is the resident behaviour too and a fix changes the default error precedence, so it is not made here.
- `compact_browse` and facets read one base row before they check the deadline or the filter, and `CompactRecordIter`
  decodes a row before its caller can look at it (E4, with the exact scan).
- `search_grouped` runs the exact search before its zero-limit shortcut, as the resident path does.
- The arm of the compact hybrid search and of `execute_compact` that answers `IndexNotReady` for an unavailable text
  index has no test: `TextIndex.limit` and `available` are private. It needs the test constructor
  `TextIndex::with_posting_limit` that lane B4 adds in `b42dd99`.
- A compact collection can only lose named vectors (every put or delete drops the cell and `put_vectors` is refused), so
  the tests install cells directly through `state_mut`.
- The guard that bounds a re-embedding job counts `len()`, which includes hidden rows; the resident path does the same.

## 7. Hand-offs

- **E4** owns the exact scan from here: the draft reads visibility from the columns (CLK-6), materializes a cell per
  visited row and ignores summary candidates, so its statistics can differ from the resident ones once summaries exist.
  `consumer_exact` and the lane A tests are its parity harness.
- **K71**: the first two commits of the key-value group are K71's `src/kv.rs` commits. If K71 is integrated first, skip
  them; the dispatch commit applies on K71's tip. The rule "list only the record `get` resolves" is pinned by
  `compact_kv_list_offers_only_the_record_get_resolves` and by K71's `only_candidates_that_can_be_entries_are_probed`.
- **R0** (slice 3: `base_rows`, `affected_ids`, `ordered_ids`) changes `src/compact/source.rs` and the effective block of
  `src/records.rs`. The mutants on `src/records.rs` are replayed on the tree they were written on; after R0 lands,
  `verify` reports them stale and they are replayed again.
- **B4, slice S3** (the declared-index differential) runs the compact exact search of A against B's index rebuild; the
  unavailable-index arm above needs B4's test constructor.
- **P3c**: named ANN and the approximate hybrid leg stay refused on compact records.

## 8. Replaying the evidence

`tests/compact_a4_mutants.json` is a list in the format of `tests/compact_mutant_replay.py` (lane B4's tool). Each
record is one anchored replacement `{id, origin, desc, file, old, new, cmd, expect}`; `runs.<machine>` holds the evidence
of one replay: the anchor matched exactly once, the mutated tree compiled, the named tests failed, the SHA-256 of the
file before, mutated and restored (over LF-normalized content, and the raw bytes before and after), the unmodified
selector green before the first and after the last mutant, one mutant per build. A compile error, a crash, a timeout or a
run that executes no test never counts.

```text
python tests/compact_mutant_replay.py check tests/compact_a4_mutants.json
python tests/compact_mutant_replay.py run tests/compact_a4_mutants.json OUT.json --machine NAME --ids ID,ID
python tests/compact_mutant_replay.py merge tests/compact_a4_mutants.json OUT.json
python tests/compact_mutant_replay.py verify tests/compact_a4_mutants.json --min-machines 2
```

`verify` re-checks every recorded run against the file in the tree: a run replayed on an older file is stale and must be
replayed again. Tests must not sleep or read the wall clock; every test here passes an explicit clock.
