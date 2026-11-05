# P3b slice 4: survivor ledgers of the parked lanes A and B

Status: inventory for 0.7.1 lane R0, taken against `v0.7.0` (`02ef3e5f`). It records the evidence that exists for each
parked lane; it does not accept, reject or integrate either one. Read it with the [seam freeze](p3b-seam-freeze.md) and
the slice 4 row of the [P3 design](p3-compact-vectors.md). Nothing here is active: both lanes are test-gated, and no
size or speed claim follows from them.

## How the ledgers were kept

Both lanes were checked with hand-mutation campaigns: a list of `{id, desc, file, old, new, cmd}` records, each one
anchored replacement at one site that must occur exactly once, run one at a time and restored. A mutant counts as
killed only when a named test fails; compile errors and unexecuted tests are not kills. The lists and the CI results
lived on throwaway campaign branches that were deleted after 0.7.0, so this page is the durable record of their IDs,
counts and gaps. The branch tips below are named by commit.

| Lane | Campaign | Killed | Survivors (test gaps) |
|---|---|---:|---:|
| A: record consumers | 47 mutants, `pa-*` | 21 | 26 |
| B: derived indexes and the read seam | 36 mutants, `pb-*` | 18 | 18 |

The lists label each mutant `[expect KILL]` or `[expect SURVIVE = test gap]`; the labels agree with the recorded counts
(A: "21 mutants killed and 26 survivors" in its fix-forward report; B: the 18 named obligations of its commit
message). The original per-mutant results are not in the repository.

## What is not in 0.7.0

`git cherry v0.7.0 <tip>` marks a patch `-` when an equivalent is already in the tag. Both lanes start with the same
seven patches (`2ae1c6c8` to `fba501b1`), which are in 0.7.0 under other hashes, so the lanes' remaining work is only
this:

| Lane | Tip | Patches not in 0.7.0 |
|---|---|---|
| A, `mahpate-p3b-consumers` | `834a0a6f` | `02062058` compact record consumers; `60d65f99` KV dispatch test alignment; `c859d2d5` bounded exact scan; `834a0a6f` fix-forward |
| B, `mahpate-p3b-indexes` | `250a98bc` | `250a98bc` review gaps (six files, tests plus one three-line production gate and a GPU preflight extraction) |

A merge of either branch conflicts only because the identical equivalent patches sit next to the new hunks. Reconcile
by `git cherry-pick` of these five commits in order, never by merge. The two lanes do not conflict with each other.

Three 0.7.0 changes land in functions that lane A also edits and need a read, not just a textual merge:

- `src/kv.rs::list_at` now skips a record that merely decodes to a key unless `probe` resolves the key to that slot.
  Lane A's compact KV listing must keep that rule.
- `src/property_graph/retrieve.rs` gained `retrieve_visible` and `retrieve_with_visibility`; lane A's
  `compact_graph_retrieve_projects_effective_row_at_one_clock` edits the same function.
- `src/lib.rs::create_collection_as_occupied` is new; it does not touch the seam.

## Lane A

Lane A's fix-forward report (`834a0a6f`) states that all 26 survivors now have named, non-vacuous coverage and that the
per-survivor proof was not completed. Its evidence falls into four classes:

- **single replay**: a recorded fail-first run of this mutant alone, with the failing selector and a byte-identical
  restoration;
- **bundle replay**: replayed only inside a combined mutant with other survivors, so the failing selector does not show
  that each one is killed;
- **named test only**: a closing test is named, no replay is recorded;
- **equivalent claim**: argued equivalent without an independent review.

| Mutant | File | Gap at the campaign head | Closing test | Evidence |
|---|---|---|---|---|
| `pa-exact-cap-whole` | `lib.rs` | the compact index-candidate cap is the row count, not half | `compact_exact_search_candidate_and_stats_match_declared_index` | single replay |
| `pa-hybrid-resident-default-clock-zero` | `hybrid.rs` | resident hybrid samples clock 0, so expired rows are served | `hybrid_resident_default_clock_honors_ttl` | single replay |
| `pa-hybrid-compact-health-removed` | `hybrid.rs` | a poisoned handle still serves text-only hybrid | `compact_hybrid_rejects_poisoned_text_only_search` | bundle replay |
| `pa-hybrid-compact-k-not-truncated` | `hybrid.rs` | fused hits are not cut to `k` | `compact_hybrid_text_leg_matches_resident_after_overlay_resolution` | bundle replay |
| `pa-hybrid-compact-tie-break-desc` | `hybrid.rs` | fused ties break by descending ID | `compact_hybrid_ties_order_by_id` | bundle replay |
| `pa-hybrid-undeclared-text-fields-allowed` | `hybrid.rs` | a text query on a collection without declared text fields is served | `compact_hybrid_requires_declared_text_fields` | bundle replay |
| `pa-hybrid-visible-conditions-ignored` | `hybrid.rs` | the text leg's visible set ignores the filter | `compact_hybrid_text_leg_matches_resident_after_overlay_resolution` | bundle replay |
| `pa-iter-search-step-two` | `records.rs` | the cursor search skips one base row | `compact_browse_cursor_skips_tombstones_and_expiry` | bundle replay |
| `pa-iter-search-high-short` | `records.rs` | a cursor at or past the last base ID yields that row again | same | bundle replay |
| `pa-iter-overlay-cursor-included` | `records.rs` | an overlay row at the cursor is returned twice | same | bundle replay |
| `pa-group-depth-one` | `records.rs` | compact grouped search asks for one hit | `compact_grouped_search_and_facets_match_resident` | bundle replay |
| `pa-facets-compact-conditions-ignored` | `records.rs` | compact facets ignore the AND filter | same | bundle replay |
| `pa-kv-list-namespace-ignored` | `kv.rs` | compact list ignores the namespace | `compact_kv_point_and_prefix_match_resident` | named test only |
| `pa-kv-list-after-inclusive` | `kv.rs` | the list cursor is inclusive | same | named test only |
| `pa-kv-list-limit-plus-one` | `kv.rs` | a page holds `limit + 1` entries | same | named test only |
| `pa-kv-list-zero-limit-allowed` | `kv.rs` | limit 0 is accepted | same | named test only |
| `pa-kv-list-namespace-validation-removed` | `kv.rs` | an empty or over-long namespace is accepted | same | named test only |
| `pa-kv-view-health-removed` | `kv.rs` | a poisoned handle hands out a KV view | same | named test only |
| `pa-kv-get-key-validation-removed` | `kv.rs` | an empty or over-long key probes the table | same | named test only |
| `pa-kv-probe-reads-base-slot-only` | `kv.rs` | a key displaced by a hash collision is never found | same | named test only |
| `pa-reembed-limit-never-breaks` | `vectors.rs` | `reembed_pending` ignores its limit | `compact_named_vector_visibility_matches_resident` | named test only |
| `pa-named-search-ignores-clock` | `vectors.rs` | named search checks visibility at clock 0 | same | named test only |
| `pa-ql-limit-not-applied` | `query_language.rs` | compact hits are not cut to `LIMIT` | `compact_query_language_matches_resident_after_overlay_resolution` | named test only |
| `pa-ql-order-key-reads-b` | `query_language.rs` | `ORDER BY` compares a row with itself | same | named test only |
| `pa-ql-vector-depth-one` | `query_language.rs` | the vector leg is limited to one hit | `compact_query_language_vector_leg_uses_full_candidate_depth` | named test only |
| `pa-graph-evidence-wall-clock` | `property_graph/retrieve.rs` | evidence is read at the wall clock, not the retrieval clock | none: graph-attached records cannot expire | equivalent claim |

Three regressions outside the list have their own record: `committed_apply_errors_poison_compact_handle` (single
replay), and the named-vector primary read error and the compact-query validation order, whose fail-first proofs were
made in the lane's session and not preserved. The bundle runs were: hybrid (five mutants combined; two of the four
selector tests failed) and cursor, group and facet (five mutants combined; only the cursor selector is shown failing).

The 21 killed mutants are `pa-iter-cursor-lt`, `pa-group-clock-zero`, `pa-group-record-arm-off`,
`pa-hybrid-compact-dispatch-off`, `pa-kv-list-sort-desc`, `pa-kv-list-version-zero`, `pa-kv-list-expiry-dropped`,
`pa-kv-view-dimension-check-removed`, `pa-kv-store-resident-guard-removed` (killed incidentally), `pa-kv-list-compact-arm-off`,
`pa-reembed-target-match-inverted`, `pa-reembed-payload-dropped`, `pa-ann-resident-guard-removed`,
`pa-ql-compact-dispatch-off`, `pa-ql-visible-metadata-dropped`, `pa-ql-clock-zero`, `pa-ql-facet-first-count`,
`pa-exact-cap-quarter`, `pa-exact-compact-arm-off`, `pa-graph-revalidate-wall-clock` and
`pa-graph-compact-pivot-state-off`. Their killing tests are the lane's parity tests against a resident oracle.

Open for A4: replay each of the 26 individually (fail-first, byte-identical restore, restored selector green), rebase
anchors onto the tree after the five commits, and give `pa-graph-evidence-wall-clock` an independent review of its
equivalence argument. Run the combined declared-index scan differential with lane B after both are integrated.

## Lane B

Lane B's fix-forward (`250a98bc`) adds boundary, expiry, overlay, index, rerank, settings and authenticated-read tests.
Its message records eight mutants as compiled, failing a named test, restored and green, and ten as "tests added,
replay deferred".

| Mutant | File | Gap at the campaign head | Closing test in `250a98bc` | Message |
|---|---|---|---|---|
| `pb-seam-payload-extent-unchecked` | `compact/source.rs` | an out-of-bounds payload extent reads as an empty payload | `compact_seam_rejects_bad_extents_and_accounting_overflow` | claimed |
| `pb-rec-metadata-error-swallowed` | `records.rs` | `effective_metadata` turns a read or authentication error into no row | `compact_effective_reads_propagate_tampered_block_errors` | claimed |
| `pb-rec-payload-error-swallowed` | `records.rs` | the same for `effective_payload` | same | claimed |
| `pb-rec-payload-delete-fallthrough` | `records.rs` | a tombstone falls through to the base payload | `compact_effective_accessor_pins_overlay_without_materializing_cells` | claimed |
| `pb-rec-visible-base-boundary-lt` | `records.rs` | a base row whose deadline equals the clock stays visible | `compact_effective_visibility_matches_exact_expiry_for_base_and_resident` | claimed |
| `pb-rec-visible-put-expiry-ignored` | `records.rs` | an expired overlay put is visible | the overlay accessor test above | claimed |
| `pb-rec-visible-resident-boundary-lt` | `records.rs` | the resident arm hides one tick late | both visibility tests | claimed |
| `pb-settings-validate-removed` | `lib.rs` | the compact settings arm does not validate | `compact_read_only_settings_preserve_error_precedence`, `compact_settings_validate_and_persist_named_collection` | claimed |
| `pb-collection-text-assign-dropped` | `collection.rs` | `rebuild_derived` discards the rebuilt text index | `compact_metadata_text_indexes_verify_overlay` | deferred |
| `pb-meta-overlay-rows-only` | `metadata_index.rs` | the rebuild indexes only overlay rows | same | deferred |
| `pb-text-overlay-rows-only` | `text_index.rs` | the same for the text index | same | deferred |
| `pb-rerank-batch-bound-inclusive` | `rerank.rs` | a batch of exactly `MAX_BATCH_OPERATIONS` hits is refused | same | deferred |
| `pb-rerank-clock-max` | `rerank.rs` | rerank samples the maximum clock | same | deferred |
| `pb-rerank-distance-dropped` | `rerank.rs` | candidates lose the incoming hit distance | same | deferred |
| `pb-rerank-health-check-removed` | `rerank.rs` | a poisoned handle is still reranked | same | deferred |
| `pb-settings-frame-wrong-collection` | `lib.rs` | the settings frame is written for collection 0 | `compact_settings_validate_and_persist_named_collection` | deferred |
| `pb-settings-metadata-assign-dropped` | `lib.rs` | the rebuilt metadata index is discarded | same | deferred |
| `pb-text-limit-reset` | `text_index.rs` | the rebuild resets the posting limit | `compact_text_rebuild_preserves_posting_limit` | deferred |

The 18 killed mutants are `pb-rec-ids-reversed`, `pb-rec-payload-put-ignored`, `pb-rec-visible-base-expiry-ignored`,
`pb-rec-visible-delete-visible`, `pb-rec-visible-unknown-id-visible`, `pb-seam-source-live-off-by-one`,
`pb-seam-payload-wrong-row`, `pb-seam-payload-oob-row-some`, `pb-rerank-clock-zero`, `pb-rerank-visibility-inverted`,
`pb-rerank-workspace-over-4mib`, `pb-text-track-deletions`, `pb-quality-overlay-bytes-zero`,
`pb-collection-fallthrough-resident`, `pb-settings-commit-removed`, `pb-settings-persisted-false`,
`pb-settings-state-dropped` and `pb-settings-workspace-over-4mib`. By the lane's own record their killers are tests of
the integrated lane B part, which `v0.7.0` carries.

At `v0.7.0` all 18 survivors are still test gaps, because the closing tests are in the parked patch. Seven of them
are in the read seam itself (`compact/source.rs` and `records.rs`): lane R0 replays each of those on the 0.7.0 tree and
closes them with its own tests; see [the seam freeze](p3b-seam-freeze.md#7-the-r0-mutation-campaign).

Open for B4: cherry-pick `250a98bc` and replay the other eleven (consumers, settings and rerank). The lane's mutant list
has no mutant for the frame-kind guard or for the vector error path of the read seam; the freeze tests pin both
(`R0-AU-01` and `R0-AU-12` in the R0 ledger).

## Evidence that would settle OPEN-71-02

The commit-message ledgers and the fix-forward report are claims, not acceptance. A lane's survivors are accepted when,
for every mutant: the anchor is re-based onto the tree being accepted and still matches once; the mutant compiles;
exactly the named test (or tests) fail, with the failing test names recorded; the source is restored byte-identically
(SHA-256 before and after); the restored selector is green; and an independent reviewer has replayed a sample or the
whole list on another machine. A mutant that needs an equivalence argument keeps it in the ledger, with the invariant
it rests on pinned by a test where one can be written. Replays of several mutants in one build do not count.

## Not established here

R0 did not replay lane A's 26 survivors, lane B's eleven consumer survivors or lane B's 18 killed mutants, and did not
build either lane's branch.
The classification of the evidence comes from the lanes' own reports and commit messages, the mutant lists and a
`git cherry` of each branch against `v0.7.0`.
