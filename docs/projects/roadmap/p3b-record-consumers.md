# P3b slice 4, lane A4: record consumers over compact records

| | |
|---|---|
| Status | Integrated behind the test gates on `mahpate-r071-a4` (accepted at `4670d858`), and checked again on the 0.7.2 integration tree T (`mahpate-int072-c1` @ `2d405125`), where the commits of this lane apply to `main` without a conflict and the replays of the files that changed there were repeated (section 8; the gate, section 9). Nothing compact becomes active: every compact arm below is `cfg(test)`, and no default, file format or wire shape changes. Replaces the parked lane A (`origin/mahpate-p3b-consumers` @ `834a0a6`) |
| Scope | Key-value reads, hybrid search, the query language, grouped search and facets, the record iterator and browse, named vectors and re-embedding, graph evidence, and the exact-scan draft |
| Out of scope | Named ANN and every other HNSW, snapshot, rebuild or disk-ANN path (they stay refused on compact and belong to [P3c](p3c-compact-ann.md)); the exact scan beyond the lane A draft (lane E4 owns it); the seam itself (`src/compact/source.rs`, lane R0) |
| Reads with | The [seam freeze](p3b-seam-freeze.md) (the frozen semantics CLK, PRE, ERR, ACC and IDN are cited below), the [survivor ledgers](p3b-survivor-ledgers.md#lane-a) and the [P3 design](p3-compact-vectors.md), slice 4 and table J |
| Release gate | `python scripts/shadow_copy_check.py` (section 9): the shipped resident code is the code of `v0.7.0`, apart from the named exceptions of this page |

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

The `IndexNotReady` arms of the hybrid search and of `execute_compact` need a text index that a posting limit has made
unavailable. `TextIndex::with_posting_limit` (a `cfg(test)` constructor) is lane B4's `b42dd99`, cherry-picked with `-x`
(it also brings the replay tool and ledger of that commit; the commands of section 8 need the final tool of lane B4, which
lands first, and the commit is skipped when B4 is integrated first).

A4's own changes to shipped code (the code a release build compiles) are listed here; every other shipped function is the
one of `v0.7.0`, or lane B4's, or K71's, and `scripts/shadow_copy_check.py` (section 9) checks that.

1. `src/kv.rs`, the read path that every resident key-value read and write also runs: one `acquire` (health for a compact
   collection, residency for a resident one), one `Rows` iterator (`compact_iter` or the resident map) and one
   `probe`/`entry` through `CollectionRecords::get_physical`. For a resident collection `acquire` is
   `ensure_resident_in`, `rows` is the resident map iterator and `get_physical` is the map lookup, so the behaviour is
   unchanged; the gate pins the text of each of them and of the functions that call them, and `tests/kv.rs` and the
   server key-value suites cover the behaviour. Lane A's separate compact `probe_compact`, `entry_compact` and `list_at`
   are gone, and with them lane A's compact list, which lacked the `v0.7.0` rule "a key is listed only through the record
   that `get` resolves for it". The redundant health check in `Database::kv_view` is gone: `Database::resolve` already
   makes it.
2. `src/vectors.rs`: the compact arm of `reembed_pending` checked its limit after it had read the next row. A call with
   limit `n` decoded the row after the `n`th hit and a call with limit 0 decoded the first row. It now checks first
   (test-gated). `search_named_at` and `search_named_ann_at` carry lane A's two resident changes, below.
3. `src/property_graph/retrieve.rs`, `src/hybrid.rs`, `src/records.rs`: lane A's clock corrections and one skipped-hit
   guard, below, and one line of this lane: the evidence read by collection id (`db.get_at_in`) instead of by name.

Apart from these and K71's two commits (and the `visit_at` rows they add to `docs/STORAGE-FEATURES.md` and
`site/api.html`), the shipped code is lane A's, byte for byte. A4 adds 68 tests in nine sibling modules under
`src/compact/source/overlay_tests/` (`consumer_{cursor,group,exact,hybrid,query,text,vectors,graph,kv}.rs`), the key-value
fixture `src/compact/catalog/tests/kv_windows.rs` and two resident tests in `tests/graph_ttl.rs`
(`retrieve_reads_the_pivot_records_at_the_read_clock`, `retrieve_reads_the_approximate_pivot_leg_at_the_wall_clock`).

### Changes a resident file can see

These are the only differences a resident file can observe. The independent review compared the hybrid, named-vector,
graph and key-value reads of a build of this lane with a build of the base on thousands of generated cases (every clock
around the deadlines, every error order it could name) and ran the resident integration suites and the server
key-value suites; everything away from the changes below was identical.

| Where | What changes | Pinned by |
|---|---|---|
| `GraphRef::retrieve` (`retrieve.rs`) | The pivot search (its exact and text legs), the pivot re-validation and the evidence read at the read clock of the handle (`GraphRef::with_clock`), or at one sample of the current time per call. Before, each leg and each record sampled the wall clock on its own. Without `with_clock` a result can differ only for a record whose deadline passes during the call. An approximate pivot leg still reads the current time (section 6) | `retrieve_reads_the_pivot_records_at_the_read_clock`, `retrieve_reads_the_approximate_pivot_leg_at_the_wall_clock` |
| `CollectionRef::search_hybrid` (`hybrid.rs`) | The exact vector leg and the text leg read at one clock. The text leg sampled the wall clock again for every candidate, so the two legs could disagree at a deadline | `hybrid_resident_default_clock_honors_ttl` (`tests/hybrid.rs`) |
| `CollectionRef::search_named_at` (`vectors.rs`) | A read error of a primary record is returned. Before, the row was dropped and the call answered `Ok`. On a resident file only `Poisoned` can occur, when the flusher thread fails during the scan | `compact_named_vector_search_propagates_primary_read_error` (compact arm of the same code); resident: a race only |
| `CollectionRef::search_named_ann_at` (`vectors.rs`) | A residency check first; `named_records` makes the same check next, so no visible change | the gate |
| key-value reads and writes (`kv.rs`) | `acquire`, `rows` and `get_physical` replace the inline residency check and map lookups; the same checks, the same lookups | the gate, `tests/kv.rs`, the server key-value suites |
| `CollectionRef::search_grouped` (`records.rs`) | A hit without a record is skipped instead of indexing the map; it cannot happen for a resident file, whose hits come from the same map under the same borrow | the gate |

Proposed release-note sentence: *Hybrid search and graph retrieval now check record deadlines against one clock per call
instead of re-reading the wall clock for each leg and row: `search_hybrid` samples the wall clock once for its text leg
and its exact vector leg, and `GraphRef::retrieve` reads its pivot and evidence records at the graph read clock (the
value of `GraphRef::with_clock` when one is set), so results differ only for records whose deadline lies between that
clock and the wall clock or passes during the call; the approximate (HNSW) vector leg still checks deadlines at the wall
clock. A named-vector search whose handle turns unhealthy during the scan now answers `Poisoned` instead of dropping the
remaining rows.* `GraphRef::with_clock` says so in its rustdoc, in `docs/GRAPH.md` and in the API reference.

## 2. The rules the tests check

Every consumer is compared with the resident store opened from the same image with source installation off, at the
same clock, bit for bit where floats are involved, and against hand-computed literals (the oracle is never the only
check, because both forms share code in places). A test that has to look at the work a call did uses the decode
counter of the generation or the materialized cells of a row.

| Consumer | Rules (frozen semantics in brackets) | Tests |
|---|---|---|
| `KvView::{get_at, list_at, visit_at}`, the probe behind `KvStore` (`src/kv.rs`) | Health, then dimension, then arguments, then reads (ERR-8). The probe window is exactly `KV_PROBE_SLOTS` (16) slots; the first slot that decodes to the key in its namespace wins, so look-alikes inside the window, behind a holder or in another namespace are passed over and those outside it are not found. A listing offers a key only through the record `get` resolves, is in key order, strictly after the cursor, and its limit counts offered entries only. A deadline equal to the clock hides an entry (CLK-2). A damaged base row is an error, never an absent key (ERR-4) | `compact_kv_point_and_prefix_match_resident` (table J, both source precisions), `consumer_kv` (7) |
| `search_hybrid_at`, `search_hybrid_compact`, `search_hybrid` (`src/hybrid.rs`) | One captured clock for both legs (CLK-1, CLK-7); `search_hybrid` samples the wall clock once (the resident correction). Health, then validation in the resident order, with the declared text fields, then `IndexNotReady` for a text leg (a positive weight and at least one token) while the text index is unavailable, also for `k = 0` and with the vector leg off; a query without a text leg is not affected. The vector leg keeps the full candidate depth; the visible set of the text leg is the effective rows that pass the deadline and the filter; fusion scores, distances and BM25 equal the resident values bit for bit; equal fused scores break by ascending ID; the fused list is cut to `k`. The approximate leg stays refused (`UnsupportedFeature(FLAG_COMPACT)`) | `consumer_hybrid` (16), `consumer_text` (2) and the lane A tests in `overlay_tests.rs` |
| `execute`, `execute_compact` (`src/query_language.rs`) | Health first, and an invalid query is refused before any row is decoded. The vector leg has the full candidate depth, `ORDER BY` compares the field of one row with the field of the other, `LIMIT` is applied, groups and facets count the effective rows; any `TEXT` clause, with or without a token, answers `IndexNotReady` while the text index is unavailable | `consumer_query` (5), `consumer_text` (2) |
| `CollectionRef::{search_grouped, facets}` (`src/records.rs`; the exact hook they call is in `src/collection.rs`) | The nearest hit leads each group, group and per-group limits, typed value counting, the AND filter over overlay rows, validation before any row is decoded, health first, base-row read errors returned | `consumer_group` (7) |
| `compact_iter`, `CompactRecordIter`, `compact_browse` (`src/records.rs`) | A cursor seeks by binary search over the base rows and merges the overlay in ascending ID order; a tombstone or an expired winner is skipped and never falls through (CLK-4, PRE-1); pages chain like the resident ones at every clock; a snapshot keeps the state it captured; read errors are items, never skipped rows | `consumer_cursor` (8) |
| `record_vectors`, `search_named_at`, `reembed_pending(_checked)` (`src/vectors.rs`) | A named vector is visible only while its primary row is visible at the one clock; a poisoned cell or a tampered block is an error; no base row that the overlay shadows is read; re-embedding lists the effective rows without the target version in ascending order, within its limit and without decoding a row beyond it; named ANN stays refused with nothing built | `compact_named_vector_visibility_matches_resident` (table J, `full` and `half`), `compact_named_vector_search_propagates_primary_read_error`, `consumer_vectors` (11) |
| `GraphRef::retrieve` evidence reads (`src/property_graph/retrieve.rs`) | Pivots and evidence are read at the one retrieval clock; only the requested evidence collections are read and only the attached rows that are projected are decoded; errors propagate; at most 64 unmapped pivots are listed and all are counted; the pivot records are read at the clock of the graph handle on resident files too; the bounds of a request are validated before the health of the handle (a known order after acquisition, section 6) | `consumer_graph` (11) and, for resident files, `retrieve_reads_the_pivot_records_at_the_read_clock` in `tests/graph_ttl.rs` |
| `Database::search_in_at`, compact arm; `exact_search_compact` (`src/lib.rs`) | Draft, refined by lane E4 (section 7). Hits, their order, the distances, the evaluations and the index candidates equal the resident ones; a declared posting narrows the scan only when it holds at most half of the rows. One exception: `records_scanned` differs where the resident scan prunes by block summaries (a filter that no declared posting serves, over more than one 1,024-row block), because a compact collection has no summaries until P5-71 persists them and its scan examines every row. In the pinned case (2,500 rows, a range inside one block) the resident scan examines 1,024 rows and the compact scan 2,500; R0's probe of range summaries measured 176 rows for the resident scan and 1,200 for the compact scan. See `p3b-exact-scan.md` (lane E4's page), section 4 | `consumer_exact` (1) and the lane A tests; the exception is pinned by `compact_scan_examines_every_row_where_the_resident_block_summaries_prune` (lane B4) |

## 3. Table J obligations that exist only on lane A

| Test | What it covers |
|---|---|
| `compact_kv_point_and_prefix_match_resident` | Runs on both source precisions through a rewrite, an overlay-only key, a deadline-only write, the deletion of a holder and of a displaced key, at every clock around the deadlines: expiry equality, absent and tombstoned winners, namespace and cursor ordering, against the resident store and against hand-computed lists |
| `compact_named_vector_visibility_matches_resident` | Runs for the float32 (`full`) and the float16 (`half`) collection |
| `compact_named_vector_search_propagates_primary_read_error` | The primary-read-error regression; `consumer_vectors` adds the poisoned-cell and tampered-block cases for the other consumers |

## 4. The survivors of lane A, measured

Lane A's ledger lists 47 mutants, 26 of them survivors. Each was re-created from its recorded description and anchored
on the tree after the cherry-picks (`tests/compact_a4_mutants.json`, `first_replay` is the first replay on that tree,
before the A4 tests). On that tree 40 of the 47 were already killed and 7 survived: `pa-exact-cap-quarter`, `pa-graph-evidence-wall-clock`, `pa-group-clock-zero`, `pa-kv-probe-reads-base-slot-only`, `pa-kv-view-health-removed`, `pa-ql-clock-zero`, `pa-ql-order-key-reads-b`. Three of them, `pa-group-clock-zero`, `pa-ql-clock-zero` and `pa-exact-cap-quarter`, are recorded as killed in lane A's ledger; their tests used far-future deadlines only (and a quarter of the rows as the index cap satisfied the same cases as half). `pa-kv-probe-reads-base-slot-only` survived because the fixture of the named test never produced an image in which a key is displaced; `pa-ql-order-key-reads-b` because the named test does not fail on it; `pa-kv-view-health-removed` because the check it removed was dead code (`Database::resolve` checks health first), so the line is gone and the mutant is re-anchored on `resolve`; and `pa-graph-evidence-wall-clock` is lane A's equivalence claim (section 6). Every one of the 47 is killed by the tests in the last column, on the lane head or, for the files that T changed, on T (section 8); the 26 survivors of lane A's ledger are killed on two machines.

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
| `pa-kv-list-namespace-ignored` | `kv.rs` | named test only | killed by `compact_kv_point_and_prefix_match_resident` | killed on both machines | `a_record_of_another_namespace_inside_a_keys_window_costs_no_probe` |
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
| `pa-exact-cap-quarter` | `lib.rs` | recorded as killed | survived | killed on both machines | `compact_exact_search_uses_declared_postings_up_to_half_of_the_rows`, `hybrid_filter_applies_to_both_legs_and_the_vector_leg_reports_its_work`, `hybrid_validation_matches_resident_in_check_order` |
| `pa-exact-compact-arm-off` | `collection.rs` | recorded as killed | killed by `compact_dispatch_grouped_facets_match_nonempty_resident_rows`, `compact_dispatch_unqualified_scans_and_derived_reads_refuse` and 11 more | killed | `compact_exact_search_uses_declared_postings_up_to_half_of_the_rows`, `an_attached_record_never_carries_a_deadline_on_compact_or_resident_records`, `compact_graph_access_and_retrieval_answer_poisoned_on_an_unhealthy_handle` |
| `pa-graph-compact-pivot-state-off` | `property_graph/retrieve.rs` | recorded as killed | killed by `compact_graph_retrieve_projects_effective_row_at_one_clock` | killed on both machines | `an_attached_record_never_carries_a_deadline_on_compact_or_resident_records`, `compact_graph_access_and_retrieval_answer_poisoned_on_an_unhealthy_handle`, `compact_graph_evidence_decodes_only_the_attached_rows_it_projects` |
| `pa-graph-revalidate-wall-clock` | `property_graph/retrieve.rs` | recorded as killed | killed by `compact_graph_retrieve_projects_effective_row_at_one_clock` | killed on both machines | `compact_graph_evidence_is_read_at_the_retrieval_clock_even_if_an_attached_row_had_a_deadline`, `compact_graph_retrieval_uses_one_explicit_clock_for_pivots_and_evidence` |
| `pa-group-clock-zero` | `records.rs` | recorded as killed | survived | killed | `compact_grouped_search_and_facets_apply_the_and_filter_to_overlay_rows`, `compact_grouped_search_follows_the_nearest_hit_and_the_group_limits` |
| `pa-group-record-arm-off` | `records.rs` | recorded as killed | killed by `compact_dispatch_grouped_facets_match_nonempty_resident_rows`, `compact_dispatch_unqualified_scans_and_derived_reads_refuse` and 2 more | killed | `compact_grouped_search_and_facets_apply_the_and_filter_to_overlay_rows`, `compact_grouped_search_and_facets_return_base_read_errors`, `compact_grouped_search_and_facets_validate_before_materializing_rows` |
| `pa-hybrid-compact-dispatch-off` | `hybrid.rs` | recorded as killed | killed by `compact_graph_retrieve_projects_effective_row_at_one_clock`, `compact_hybrid_requires_declared_text_fields` and 5 more | killed | `an_attached_record_never_carries_a_deadline_on_compact_or_resident_records`, `compact_graph_access_and_retrieval_answer_poisoned_on_an_unhealthy_handle`, `compact_graph_evidence_decodes_only_the_attached_rows_it_projects` |
| `pa-iter-cursor-lt` | `records.rs` | recorded as killed | killed by `compact_browse_cursor_skips_tombstones_and_expiry` | killed | `compact_browse_pages_chain_like_the_resident_records_at_every_clock`, `compact_iter_overlay_around_the_base_matches_resident`, `compact_iter_read_errors_are_items_and_browse_returns_them` |
| `pa-kv-list-compact-arm-off` | `kv.rs` | recorded as killed | killed by `compact_kv_point_and_prefix_match_resident` | killed | `compact_kv_list_offers_only_the_record_get_resolves`, `compact_kv_probe_window_is_exactly_sixteen_slots`, `compact_kv_reads_report_base_row_errors` |
| `pa-kv-list-expiry-dropped` | `kv.rs` | recorded as killed | killed by `compact_kv_point_and_prefix_match_resident` | killed | `compact_kv_list_offers_only_the_record_get_resolves` |
| `pa-kv-list-sort-desc` | `kv.rs` | recorded as killed | killed by `compact_kv_point_and_prefix_match_resident` | killed | `compact_kv_list_offers_only_the_record_get_resolves` |
| `pa-kv-list-version-zero` | `kv.rs` | recorded as killed | killed by `compact_kv_point_and_prefix_match_resident` | killed | `compact_kv_list_offers_only_the_record_get_resolves` |
| `pa-kv-store-resident-guard-removed` | `kv.rs` | recorded as killed | killed by `compact_dispatch_mutable_acquisition_keeps_other_refusals`, `compact_installed_database_and_snapshot_method_matrix` | killed on both machines | `compact_dispatch_mutable_acquisition_keeps_other_refusals` |
| `pa-kv-view-dimension-check-removed` | `kv.rs` | recorded as killed | killed by `compact_dispatch_mutable_acquisition_keeps_other_refusals`, `compact_installed_database_and_snapshot_method_matrix` | killed | `compact_kv_view_checks_health_then_dimension_then_arguments` |
| `pa-ql-clock-zero` | `query_language.rs` | recorded as killed | survived | killed | `query_clauses_match_resident_before_and_after_overlay_mutations`, `query_hides_expired_and_deleted_rows_in_every_output_and_shows_the_rest`, `query_named_vectors_match_resident_on_the_full_collection` |
| `pa-ql-compact-dispatch-off` | `query_language.rs` | recorded as killed | killed by `compact_installed_restricted_method_guards_refuse`, `compact_query_language_matches_resident_after_overlay_resolution` and 2 more | killed | `query_clauses_match_resident_before_and_after_overlay_mutations`, `query_errors_match_resident_and_precede_row_materialization`, `query_hides_expired_and_deleted_rows_in_every_output_and_shows_the_rest` |
| `pa-ql-facet-first-count` | `query_language.rs` | recorded as killed | killed by `compact_query_language_matches_resident_after_overlay_resolution` | killed | `query_clauses_match_resident_before_and_after_overlay_mutations`, `query_hides_expired_and_deleted_rows_in_every_output_and_shows_the_rest`, `query_named_vectors_match_resident_on_the_full_collection` |
| `pa-ql-visible-metadata-dropped` | `query_language.rs` | recorded as killed | killed by `compact_query_language_matches_resident_after_overlay_resolution` | killed | `query_clauses_match_resident_before_and_after_overlay_mutations`, `query_hides_expired_and_deleted_rows_in_every_output_and_shows_the_rest`, `query_named_vectors_match_resident_on_the_full_collection` |
| `pa-reembed-payload-dropped` | `vectors.rs` | recorded as killed | killed by `compact_named_vector_visibility_matches_resident` | killed | `compact_named_vector_consumers_read_no_base_row_that_the_overlay_shadows`, `compact_reembed_pending_lists_the_effective_rows_without_the_target_version` |
| `pa-reembed-target-match-inverted` | `vectors.rs` | recorded as killed | killed by `compact_named_vector_visibility_matches_resident` | killed | `compact_named_vector_consumers_propagate_a_poisoned_cell`, `compact_named_vector_consumers_read_no_base_row_that_the_overlay_shadows`, `compact_reembed_pending_decodes_no_row_beyond_its_limit` |

## 5. The A4 mutants

A4 adds 170 mutants to the 47 of lane A, 217 in all. Every record in `tests/compact_a4_mutants.json` is one anchored replacement that was replayed and killed with the tool of section 8, with the evidence of the 0.7.1 acceptance form (the anchor matched once, the mutated tree compiled, the named tests failed, the SHA-256 of the file before, mutated and restored, the unmodified selectors green before the first and after the last mutant of the replay, one mutant per build). `expect` names tests that failed on every replay. 118 of the 217 were replayed on T (the files that T changed, section 8); the other 99 keep their replays from the lane head, which are valid because T leaves their files byte for byte as they were.

| Group | Mutants | Files | Replayed on T | Replays kept from the lane head |
|---|---|---|---|---|
| A4 key-value tests | 13 | `kv.rs` | 13 | 0 |
| A4 exact scan and hybrid lead tests | 10 | `hybrid.rs`, `lib.rs`, `vectors.rs` | 7 | 3 |
| A4 cursor, browse, group and facet tests | 44 | `records.rs` | 44 | 0 |
| A4 hybrid and query-language tests | 43 | `hybrid.rs`, `query_language.rs` | 0 | 43 |
| A4 named-vector and graph evidence tests | 45 | `property_graph/retrieve.rs`, `vectors.rs` | 22 | 23 |
| A4 unavailable-text-index tests | 9 | `hybrid.rs`, `query_language.rs` | 0 | 9 |
| A4 retrieve order after acquisition | 1 | `property_graph/retrieve.rs` | 1 | 0 |
| A4 resident read clock | 2 | `property_graph/retrieve.rs` | 2 | 0 |
| A4 attached-record invariant tests | 3 | `collection.rs`, `lib.rs`, `property_graph/mod.rs` | 2 | 1 |

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
- The `IndexNotReady` arms of the compact hybrid search and of `execute_compact` had no test (`TextIndex.limit` and
  `available` are private). `consumer_text` exhausts the text index through lane B4's test constructor and the rebuild
  that a reopen runs, and kills the mutants that drop, invert or narrow either check.
- `pa-ql-order-key-reads-b` (`ORDER BY` compares a row with itself) survived the test that lane A names for it. The clause
  table of `consumer_query` sorts by metadata fields, with rows that lack the field, and kills it.

The equivalence claim of lane A, `pa-graph-evidence-wall-clock` ("evidence is read at the wall clock, not the retrieval
clock", with the argument that a graph-attached record cannot expire), is not a code-level equivalence: a test that
injects the forbidden state through `Collection::apply_committed` kills it
(`compact_graph_evidence_is_read_at_the_retrieval_clock_even_if_an_attached_row_had_a_deadline`). It is an invariant:
no public path gives an attached record a deadline. `an_attached_record_never_carries_a_deadline_on_compact_or_resident_records`
pins it (`put_with_expiry` of an attached record is refused with `GraphAttachedRecordExpiry` and writes nothing; every
other write path leaves no deadline, enumerated with an exhaustive match on `Mutation` so a new variant fails the
test; attaching a record that has a deadline is `GraphRecordNotAttachable`). The independent review read that
enumeration and the argument below and found no counterexample; it added the two writers the enumeration did not walk,
which are pinned now: `an_attached_key_value_entry_cannot_be_given_a_deadline` (the key-value store's `put` with a
deadline, which the REST, gRPC and MCP writers call, goes through the same guarded function) and
`a_graph_batch_that_puts_and_attaches_a_record_leaves_it_without_a_deadline`. Three mutants remove the three guards of the
invariant (`a4-graph-attached-expiry-guard-removed`, `a4-graph-attach-with-deadline-allowed` and
`a4-graph-overwrite-keeps-deadline`) and are killed. `pa-kv-view-health-removed` was the other equivalent mutant: the
check it removed was dead code, because `Database::resolve` checks health first, so A4 removed the line and the mutant is
re-based onto `resolve`.

Limits that A4 leaves as they are, each with the owner that can change it:

- The approximate leg of hybrid search samples its own wall clock (`search_ready_with_options_in`). Compact hybrid refuses
  that leg, so on the compact path nothing is affected; on a resident file `GraphRef::retrieve` with an approximate pivot
  leg under `with_clock` returns the rows that are live at the wall clock while the re-validation reads at the clock of
  the handle. `retrieve_reads_the_approximate_pivot_leg_at_the_wall_clock` pins that, and the fix, a clock parameter of
  `search_ready_with_options_in`, is left to 0.7.2 (it changes the answer of a request, so the pin flips with it).
- The resident `ParsedQuery::execute` and `reembed_pending` read the wall clock for every row (through `collection.search`
  and `collection.get`, and `self.get(id)?`). A4 did not change them; a clock-taking `execute_at` is the fix. The freeze
  note (CLK-7, section 4) names the hybrid search as the one exception to "one clock per call"; with this lane the
  exceptions are these two consumers and the approximate leg, and the wording belongs to R0.
- The resident arms of seven functions are written twice (a `cfg(test)` branch and a `#[cfg(not(test))]` statement, ten
  pairs in all), and the compact copies `search_hybrid_compact` and `execute_compact` copy the validation and the fusion
  of the resident ones. That is accepted for 0.7.1 on the condition of section 9: a script makes the shipped copies a
  checked fact. An early-return dispatch (as `execute` and `search_hybrid_at` already have), which writes each resident
  body once and removes the ten attributes, is a follow-up lane before L5. A new field of `HybridQuery` or
  `FusionOptions` fails to compile until `every_field_of_the_hybrid_query_is_known_to_this_suite` and both arms are taught
  about it; `ParsedQuery` has private fields and no such guard yet.
- `GraphRef::retrieve` validates the bounds of a request (read limits, `pivot_k`, the exact-scan record cap, the excerpt
  size and the evidence item cap) before it looks anything up, and the first lookup is the first health check. A handle
  that turns unhealthy after `graph()` (a failed flusher) therefore answers `GraphLimitExceedsCeiling` for a request over
  a ceiling and `Poisoned` for every other request. This is the resident behaviour, the compact arm shares the code, and
  the freeze note (ERR-8) asks for health first. It is a known order after acquisition: it is pinned as it is, on both
  arms, by `graph_retrieve_validates_the_request_bounds_before_the_health_of_a_handle_that_turned_unhealthy`, and
  changing it changes the default error precedence, so it is left to 0.7.2.
- `compact_browse` and facets read one base row before they check the deadline or the filter, and `CompactRecordIter`
  decodes a row before its caller can look at it (E4, with the exact scan).
- `search_grouped` runs the exact search before its zero-limit shortcut, as the resident path does.
- A compact collection can only lose named vectors (every put or delete drops the cell and `put_vectors` is refused), so
  the tests install cells directly through `state_mut`.
- The guard that bounds a re-embedding job counts `len()`, which includes hidden rows; the resident path does the same.

## 7. Hand-offs

- **E4** is done on T, the 0.7.2 integration tree (`mahpate-int072-c1` @ `2d405125`): E4 owns the exact scan there. The
  loop of `exact_search_compact_with_workspace` in `src/lib.rs` now walks `CollectionRecords::ordered_ids` (one line) and
  five tests were added (`src/compact/source/overlay_tests/exact_scan.rs`). One of them,
  `compact_exact_scan_scores_without_materializing_record_cells`, pins that scoring creates no record cell; the draft of
  this lane already read visibility, metadata and vectors through the `effective_*` accessors (PRE-1) and did not
  materialize one. Its statistics equal the resident ones except `records_scanned` where summaries prune (section 2;
  `p3b-exact-scan.md`, section 4). `consumer_exact` and the lane A tests are its parity harness.
- **K71, G71 and M71** are final in `main` (0.7.1). The order of the first integration (R0, B4, K71, A4, G71) is
  historical: on T, 27 of this lane's 30 commits (up to `4670d858`) are cherry-picked onto `main` with no conflict. The
  other three are skipped there as duplicates: this lane's picks of K71's two `src/kv.rs` commits `6167835` and `7592127`
  (`f689d03` and `71077a0`; `4ee251b` and `5584946` on `main`) and its pick of B4's `b42dd99` (`3017ff4`). K71's later
  `src/kv.rs` commit `ce3616a` (look-alikes outside their window are skipped without a probe) is `a4c8936` on `main`, and
  the key-value selector (`kv::`, `consumer_kv`, `compact_kv`, the dispatch and method tests) is green on T. A4 edits
  functions that the freeze note reserves for K71 in `src/kv.rs` (`probe`, `entry`, `get_at`, `visit_at`, `kv_view`);
  the edits are the named exceptions of the gate, which takes K71's visitor as the baseline of `visit_at` (`--kv-base`;
  on T pass `--kv-base origin/main`, where K71 is final). The rule "list only the record `get` resolves" is pinned by
  `compact_kv_list_offers_only_the_record_get_resolves` and by K71's `only_candidates_that_can_be_entries_are_probed`. The
  server's key-value PUT computes the slot through `kv_view(..).put_slot_at` before it takes the writable store
  (`src/server/kv.rs`): with the compact `kv_view` arm a compact collection would be read and quota-checked before it is
  refused, so take the store first when that code is touched.
- **R0** (slice 3: `base_rows`, `affected_ids`, `ordered_ids`) is merged on T: it changed `src/compact/source.rs` and the
  effective block of `src/records.rs`. The recorded replays are checked against the SHA-256 of the file they mutated, so
  a change to a mutated file makes every mutant of that file stale until it is replayed again; the table after this list
  shows which files T changed since the lane head, and section 8 what was replayed.
- **L5** removes the test gates and with them the ten shadow copies (section 9). `retrieve.rs` decides "compact or not" from
  the pivot collection alone while `db.graphs()` still requires every collection to be resident, and
  `compact_graph_for_test` bypasses `Database::graph()`, so that public entry point is never exercised for compact
  records.
- **B4, slice S3** (the declared-index differential) runs the compact exact search of A against B's index rebuild;
  `consumer_text` shows how to leave a text index unavailable on both forms (`TextIndex::with_posting_limit`, then the
  rebuild that a reopen runs). B4's `b42dd99` (`3017ff4` in this lane's history) is skipped on T as a duplicate.
- **P3c**: named ANN and the approximate hybrid leg stay refused on compact records.

The files that carry mutants, and what changed them on T since the lane head (`mahpate-r071-a4` @ `4670d858`):

| File | Mutants | Changed on T since the lane head | Replayed on T |
|---|---|---|---|
| `collection.rs` | 2 | nothing: byte for byte the file of the lane head | 0 of 2 |
| `hybrid.rs` | 37 | nothing: byte for byte the file of the lane head | 0 of 37 |
| `kv.rs` | 26 | K71 (`a4c8936`: look-alikes outside their window are skipped without a probe; `3aa708a`: `KvRef` is non-exhaustive) and G71 S1 (`03b2289`: collection accessors by identity); this lane's key-value commit applies on them (`33a2867`) | 26 of 26 |
| `lib.rs` | 11 | E4 (`312bf2f`: the exact scan walks `ordered_ids`), `fb1c870` (one precondition check for dropping a collection) and G71 S1 (`03b2289`) | 11 of 11 |
| `property_graph/mod.rs` | 1 | G71 S1 (`03b2289`) and S7 (`f7fa694`: reserved-resource residues) | 1 of 1 |
| `property_graph/retrieve.rs` | 28 | G71 S1 (`03b2289`: the visible collection resolves in one call) | 28 of 28 |
| `query_language.rs` | 31 | nothing: byte for byte the file of the lane head | 0 of 31 |
| `records.rs` | 52 | R0 (`f167cf3`: base rows, affected IDs and ordered IDs) | 52 of 52 |
| `vectors.rs` | 29 | nothing: byte for byte the file of the lane head | 0 of 29 |

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
python tests/compact_mutant_replay.py verify tests/compact_a4_mutants.json
```

`verify` re-checks every recorded run against the file in the tree: a run replayed on an older file is stale and must be
replayed again. It needs one machine for a mutant, and two for the ids of the ledger's `two_machines` list, which are the 26
survivors of lane A: 26 of the 26 have a kill on two machines. 78 of the other 191 mutants have one as well, and the rest one machine. Tests must not sleep or read the wall clock; every test here passes an explicit clock.

### Replays on the 0.7.2 integration tree

The replays recorded at the lane head (`4670d858`) were made before the compact work moved to 0.7.2, and T carries the changes of other lanes in 5 of the 9 files that the ledger mutates (the table of section 7). The ledger was brought up to T in four steps.

1. **Anchors.** `check` reads every anchor exactly once on T except two: `pa-kv-store-resident-guard-removed` and `a4-cursorgroup-07` matched twice, because G71's `kv_store_by_id` and R0's `OrderedIds::next` repeat the text of their anchors. Both anchors are longer now (the replacements and the tests are the same); `check` reads 217 mutants and 0 problems on T and on the lane head.
2. **What was stale.** `verify` counts a run only while the SHA-256 of the file it mutated is the file's in the tree, so a change to a mutated file makes every mutant of that file stale until it is replayed again. On T it read 99 accepted and 118 stale: the 118 mutants of the files that T changed (`kv.rs` 26, `lib.rs` 11, `property_graph/mod.rs` 1, `property_graph/retrieve.rs` 28, `records.rs` 52) were stale, and the 99 mutants of the files that T leaves byte for byte as they were (`collection.rs` 2, `hybrid.rs` 37, `query_language.rs` 31, `vectors.rs` 29) were not.
3. **Replays on T.** The 118 stale mutants were replayed on T, one mutant per build, in the form above: every one was killed (0 survivors), with the unmodified selectors green before the first and after the last mutant of each replay and every file restored byte for byte. The 15 of them that are on the `two_machines` list were replayed on two machines, as the rule asks, and so were 39 others (the 37 graph evidence, `lib.rs` and `property_graph/mod.rs` mutants of one shard and the two re-anchored ones were also replayed here); the other 64 on one. The second machine replayed T plus the shard lists (files outside the source fingerprint), so its runs record the fingerprint of T as well. The runs of the lane head that these replace are dropped from the ledger (they stay in its history). One mutant is killed on T by another test than the one it named, so its `expect` was updated in the ledger (`expect_history` keeps the old names and the reason; the runs are untouched): `pa-kv-list-namespace-ignored`: K71's `a4c8936` rejects a record outside a key's probe window before any probe, which also stops a record of another namespace that lies outside the window, so `only_candidates_that_can_be_entries_are_probed` passes without the namespace test; K71's `a_record_of_another_namespace_inside_a_keys_window_costs_no_probe` plants such a record inside the window, counts the probes and fails.
4. **Result.** `verify` on T reads 217 accepted, 0 stale, 0 open of 217. `verify --strict` (an option of the final tool of lane B4, which T carries) also requires that a run was replayed on exactly the sources of the tree (`sources` prints the fingerprint of T, `eaf4962586c7`, over 600 files): it counts 172 of 172 valid runs as bound to T and reads 118 accepted, 99 stale, 0 open. The 99 are the mutants of the files that T leaves as they were: their lane-head replays are valid by the file rule of 0.7.1 and carry no fingerprint of T's sources. Replaying them on T closes that; it was not needed for the rule that this lane is accepted by.


## 9. The shadow-copy gate

Where a function has a compact arm, its resident arm is written twice: once for test builds (the `else` of
`if compact`) and once for release builds (`#[cfg(not(test))]`), and `cargo test --lib` compiles only the first. This lane
adds ten such pairs in seven functions, and the independent review (ARCH-2, ARCH-3) asked that the release arm be a checked
fact instead of a reading exercise. `scripts/shadow_copy_check.py` (standard library only, about half a minute, exit code
0 when everything holds) is that check:

```text
python scripts/shadow_copy_check.py                 # the gate, for this lane's own branch
python scripts/shadow_copy_check.py --integration --lane G71=v0.7.0..origin/BRANCH --lane K71=...   # a tree that integrates other lanes
python scripts/shadow_copy_check.py --dump NAME     # the shipped text and the baseline diff of one site
python scripts/shadow_copy_check.py --self-test     # the checks of the checker
```

1. **Shipped equals the baseline.** For each of the 27 sites of the list it takes the shipped variant of the function
   (statements, arms and items under `#[cfg(test)]` removed, `#[cfg(not(test))]` dropped from the ones that stay,
   `cfg(any(x, test))` read as `cfg(x)`), normalizes it (comments, whitespace and trailing commas do not count) and compares
   it with the same function at `v0.7.0` (`git show`). The differences are named edits, each with its reason and each
   matching exactly once in the baseline, so a second, unlisted change to the same function fails: the retrieve clock (four
   edits), the hybrid search clock (three), `search_named_at` (`Poisoned`), `search_named_ann_at`, `search_grouped`, and the
   key-value path (`acquire`, `rows`, `get_physical`). New functions are compared with their exact expected text.
   The key-value visitor is lane K71's code: `visit_at` and `list_at` are compared with the revision `--kv-base` names
   (default `7592127`, the commit that carries K71's visitor), and the five edits listed are the part A4 adds to it.
2. **The two copies of each pair agree.** For each of the ten pairs the resident arm of the test build (the `else` of
   `if compact`) has to equal the shipped statement next to it, so the parity tests of the lib suite compare the compact
   path with the code that ships.
3. **Whole-function copies are test-only.** `search_hybrid_compact` and `execute_compact` have to exist, and only under
   `cfg(test)`; what ships is `search_hybrid_resident_at` (`v0.7.0`'s `search_hybrid` with the clock parameter) and
   `execute` (unchanged).
4. **Coverage.** Every shipped function of a changed source file that differs from `v0.7.0` is covered by a site or by the
   list of other lanes' changes (twenty today; the two in `src/gpu.rs` are lane B4's), so a resident-visible change cannot
   appear next to the listed ones.
5. **Counting.** The number of `#[cfg(not(test))]` attributes of each source file is the baseline's plus the shadow sites
   of the list (ten in all), so an eleventh shadow copy cannot appear unnoticed.

The script needs the `v0.7.0` tag (`git fetch --tags` in a shallow clone) and K71's commit, or `--kv-base REV`; it stops
with exit code 2 when it cannot read a file at a baseline and with 1 when a check fails. It was run against eleven harmful
and two harmless edits of a copy of the tree, and each gave the expected answer. A changed shipped arm, a test-build twin
that drifts from its shipped statement, an eleventh `cfg(not(test))`, an unlisted shipped change, a second change inside a
listed function, a compact copy that ships, a changed resident `execute`, a changed wrapper clock, a changed K71 function, a
changed `get_physical` and a vanished shadow statement all fail for the stated reason; comments, formatting and a trailing
comma, and edits of test-only code, pass.

What the gate does not check: the compact copies themselves against their resident counterparts (that is what the parity
tables are for), K71's visitor beyond the edits above, and anything outside the functions of the list that does not differ
from `v0.7.0`. It is a gate for the release candidate, not a CI job: it needs the tag. The early-return dispatch that removes
the ten pairs is a follow-up lane before L5, after which the gate shrinks to the equality with `v0.7.0` that remains.

### The integration mode

On a tree that integrates other lanes, their own changes sit in the same files, and the default mode, which is as strict as
it is for this lane's branch and stays that way, lists every one of them as uncovered. `--integration` with one
`--lane NAME=RANGE` for each other lane (never this lane's own range) is the mode for that tree. A range is `A..B` (what
`B` changes since `A`), `A...B` (since the merge base of `A` and `B`) or `REV` (what `REV` changes since `--base`); a lane
branch that is built on another lane's gives that lane's functions too, so name `A..B` when the owner matters.

1. **Owned by another lane.** A shipped function that differs from the baseline and that no site and no listed change
   covers is accepted when the range of a lane adds, changes or removes it. It is listed as `owned by lane X, not checked
   here` (by every lane whose range changes it). A function that differs from the baseline and that no site and no lane
   range covers still fails; so does a function that is gone unless the range of a lane removes it. The number of
   `#[cfg(not(test))]` attributes of a file may differ from the baseline's plus the sites by what the lanes that change the
   file add or remove, and says so.
2. **Named integration edits.** A site can carry the change that another lane makes to one of its functions, as an edit
   with a reason that names the lane. These edits are applied only in this mode, are optional alternatives (the mode also
   passes on a tree that does not hold that lane, and then the baseline text is what has to match), and a stale one is
   reported. There is one: lane G71 resolves the visible collection in one call in the closure of `retrieve_with_visibility`
   (`Database::resolve_visible` instead of the visibility check and `db.collection(name)`). A second, unlisted change in
   the same function fails as before; so does any other lane's change to a function of a site, until it has its own named
   edit.

What the mode does not check: the functions that a lane range owns (that lane's review does, and so does a conflict
resolution that changed one of them), and it takes minutes, not seconds, because it reads the lane versions of every
changed file that holds an uncovered function. The self-test covers it with a throwaway git repository (the default
mode's messages unchanged, ownership by function and not by file, a removed function, `A...B`, the attribute count, the
optional and the stale integration edit), and fifteen mutants of the new code all fail it. A dry run on a tree that
integrates this lane with G71, K71 and M71 failed in the default mode, on the functions of the other lanes and on the one
interplay (G71's closure in `retrieve_with_visibility`); in the integration mode it passes with the three ranges, fails
when the range of one lane is left out, and on this lane's own branch both modes pass.

### Run on the 0.7.2 integration tree

On T (`mahpate-int072-c1` @ `2d405125`: `main` with K71, G71 and M71 final, then R0, B4, this lane, S3 and E4) the
integration mode takes what `main` changed since `v0.7.0` as one lane (`rel071`) and E4's commit as another, and K71's
visitor as it is in `main`:

```text
python scripts/shadow_copy_check.py --base v0.7.0 --kv-base origin/main --root . --integration --lane rel071=v0.7.0..origin/main --lane e4=312bf2fa~1..312bf2fa
```

It exits 0 in 24 seconds (`shadow-copy gate: ok`; `--self-test` is ok as well):

- 323 shipped functions differ from `v0.7.0`; 20 are covered by a site or a listed change and 303 are owned by `rel071`
  (listed as `owned by lane rel071, not checked here`); the range of E4 owns none.
- `retrieve_with_visibility` equals `v0.7.0` with its four named edits and the one integration edit of G71's closure,
  and its shadow pair has equal resident arms.
- K71's visitor is compared with `origin/main`, where it is final: the function `visit_at` equals it with the five named
  edits of this lane (`acquire`, `rows` and `get_physical` in place of the inline residency check, map iterator and
  lookup), and the method `KvView::visit_at` and `list_at` equal it unchanged.
- The number of `#[cfg(not(test))]` attributes is the baseline's plus the ten shadow sites.

The `--kv-base` default of the script (`75921271`, the commit that first carried K71's visitor) still names that
commit; a tree that holds K71's final code passes `--kv-base origin/main`, as above.
