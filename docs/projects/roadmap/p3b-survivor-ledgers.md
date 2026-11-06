# P3b slice 4: survivor ledgers of lanes A and B

Status: inventory for lane R0 of the compact work, taken against `v0.7.0` (`02ef3e5f`). It records the evidence that exists for each
lane at the tag; it does not accept, reject or integrate either one. Since then the lanes have been integrated on a private
integration tree, T (`mahpate-int072-c1` at `2d405125`, [seam freeze](p3b-seam-freeze.md#1-baseline), section 1): lane B (see
[Lane B](#lane-b)) and lane A (see [Lane A](#lane-a), the work of A4). 0.7.1 shipped without any of it; the compact work moved
to 0.7.2, where it is qualified again on T before anything is activated ([handover-0.7.1.md](handover-0.7.1.md)). Read it with the [seam freeze](p3b-seam-freeze.md) and
the slice 4 row of the [P3 design](p3-compact-vectors.md). Nothing compact is active: the compact arms of both lanes are
test-gated (the few changes that lane A4 makes to resident code are listed on A4's page), and no size or speed claim follows
from them.

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
by `git cherry-pick` of these five commits, never by merge. B4 and A4 did so, and A4 put the key-value hunks last.
The two lanes do not conflict with each other.

Three 0.7.0 changes land in functions that lane A also edits and needed a read, not just a textual merge, which A4 made:

- `src/kv.rs::list_at` now skips a record that merely decodes to a key unless `probe` resolves the key to that slot.
  Lane A's compact KV listing must keep that rule; A4's does, and `compact_kv_list_offers_only_the_record_get_resolves`
  pins it.
- `src/property_graph/retrieve.rs` gained `retrieve_visible` and `retrieve_with_visibility`; lane A's
  `compact_graph_retrieve_projects_effective_row_at_one_clock` edits the same function, and A4's own edits of it are the
  ones its page lists.
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

**Closed by A4** (the [record consumers of lane A4](p3b-record-consumers.md), accepted at `4670d858`; T carries its own
commits). A4 cherry-picked
lane A's four patches with `git cherry-pick -x`, never by merge: `02062058` and `834a0a6f` without their key-value hunks,
`c859d2d5` unchanged, as a draft that E4 then finished (on T its loop walks `ordered_ids`, [E4's page](p3b-exact-scan.md)),
and `60d65f99` with those hunks last, as a group on K71's bounded visitor (which shipped in 0.7.1).
It re-created lane A's 47 mutants from their recorded descriptions and anchored them on the tree after the
cherry-picks (`tests/compact_a4_mutants.json`): 40 were killed there already and 7 survived. Three of the seven,
`pa-group-clock-zero`, `pa-ql-clock-zero` and `pa-exact-cap-quarter`, are recorded as killed in lane A's ledger: their
tests used far-future deadlines only, and a quarter of the rows as the index cap satisfied the same cases as half. All 47 are
killed on the final tree, and the 26 survivors of lane A's ledger each have a kill on two machines (A4's page names the
closing tests of each). The equivalence claim `pa-graph-evidence-wall-clock` is not an equivalence in the code: a test that
injects the forbidden state through `Collection::apply_committed` kills it. It rests on an invariant, that no public path
gives a graph-attached record a deadline, which A4 pinned with `an_attached_record_never_carries_a_deadline_on_compact_or_resident_records`
and three mutants of its guards; A4's page records that an independent review read the enumeration of the writers and
found no counterexample. `pa-kv-view-health-removed` was dead code (`Database::resolve` checks health first), so A4
removed the line and re-based the mutant onto `resolve`. A4 added 170 mutants to the 47: 217 in all, every one killed on A4's own head.
`verify` of B4's final tool accepts all 217 on A4's head, with none stale (the copy of the tool that A4's branch carries has
`check`, `run` and `merge` only). The changes that a resident file can see (the exact and
text legs of the hybrid search and the pivot and evidence reads of `GraphRef::retrieve` read one clock, while the approximate
leg still reads the wall clock; a named-vector search whose handle turns unhealthy answers `Poisoned`; the
key-value read path and two guards) are listed on A4's page, with the script that checks the shipped code against
`v0.7.0`, `python scripts/shadow_copy_check.py` (`--integration` with one `--lane` range per other lane, for a tree that
holds other lanes). A ledger run is tied to the file it ran on: a file that changed after a run makes that run stale (not
failed) until it is replayed again, and [the table below](#the-ledgers-on-the-integration-tree) gives the state on T. Two
of A4's anchors need more than a replay on T, because T holds a second copy of the code they name. `a4-cursorgroup-07` anchors
the merge of a base row and an overlay entry in `CompactRecordIter::next`, which appears a second time in R0's
`OrderedIds::next`; `pa-kv-store-resident-guard-removed` anchors the call that makes the writable key-value handle refuse
a compact collection (`check_kv_collection` in `Database::kv_store`), which 0.7.1's `kv_store_by_id` repeats. `check`
reports `matches 2` for both and each anchor needs more context. The
same holds the other way round: R0's 50 mutants in `records.rs` and 2 in `lib.rs` were replayed on R0's version of
those files, which B4 and A4 also edit (E4 changes one line of `lib.rs`), so a file-hash check of their runs calls them stale on T; their anchors
still match once there (`check` accepts both R0 ledgers on T), and [section 7.3 of the seam
freeze](p3b-seam-freeze.md#73-the-mutants-of-recordsrs-and-librs-on-the-integration-tree) replays them on T: all 52 are killed, on the lane's machine and on separate machines. R0's 72 mutants in `source.rs` and its one in `quality.rs` are in files that
no other lane edits (those two files on T are byte-identical to R0's head). The combined declared-index scan
differential, A's compact exact search against B's index rebuild, is B4's slice S3: T carries it, with its 11 mutants in
`tests/compact_b4_s3_mutants.json`, and it is not part of what R0 established.

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

At `v0.7.0` all 18 survivors were test gaps, because the closing tests are in the patch that the tag lacks. Seven of them
are in the read seam itself (`compact/source.rs` and `records.rs`): lane R0 replayed each of those on the 0.7.0 tree, found
all seven alive and closed them with its own tests; see [the seam freeze](p3b-seam-freeze.md#7-the-r0-mutation-campaign).

**Closed by B4.** B4 cherry-picked `250a98bc` (never merged it) and replayed the other eleven (consumers,
settings and rerank) in the evidence form below: each gap is measured alive on `v0.7.0` before the tests that kill it,
and each kill records the failing tests, the SHA-256 of the file before, during and after, and a green selector before
the first and after the last mutant of its replay. B4's ledger, `tests/compact_b4_mutants.json`, holds 48 mutants: lane
B's 11 own obligations; six from lane B's report, which are its five survivors (four are killed by new tests, and
`x-settings-health-removed` is accepted as an equivalent mutant, because the compact settings arm repeats the health
check that `resolve` has just made, with the caveat that a failure of the background flusher between the two checks would
tell them apart, a case no contract orders and no deterministic test can observe) and the pin mutant that a test kills to
hold the invariant that equivalence rests
on; three gaps that B4's wider campaign found in the lane's consumer tests (the persisted flag and the returned
persistence of a settings change, and the limit that reaches the reranking provider); three mutants of the read-only
gate; and 25 neighbors of the consumers. They are replayed and checked with `tests/compact_mutant_replay.py`. Three of the
five report survivors (the frame-kind guard of the body read and of the record cell, and the error of `effective_vector`)
are the same replacements as `R0-AU-01`, `R0-AU-02` and `R0-AU-12`, which B4 re-derived from lane B's report, so those
three are killed in both ledgers. B4's ledger lists the seven seam obligations and the 17 mutants of the earlier seam
campaign as `reused`, and its `verify` mode reads R0's ledger to check that it holds each of those 24, and that the
final-tree run of each obligation is a kill that restored the file byte-identically (the 17 mutants carry only their
baseline kill on `v0.7.0`). B4's slice 3, a differential of the declared-index scan that runs the compact arm against the
resident arm and a model of the contract, has 11 mutants of its own in `tests/compact_b4_s3_mutants.json`; T carries it.

## Evidence that would settle OPEN-71-02

The commit-message ledgers and the fix-forward report are claims, not acceptance. A lane's survivors are accepted when,
for every mutant: the anchor is re-based onto the tree being accepted and still matches once; the mutant compiles;
exactly the named test (or tests) fail, with the failing test names recorded; the source is restored byte-identically
(SHA-256 before and after); the restored selector is green; and an independent reviewer has replayed a sample or the
whole list on another machine. A mutant that needs an equivalence argument keeps it in the ledger, with the invariant
it rests on pinned by a test where one can be written. Replays of several mutants in one build do not count.
`tests/compact_mutant_replay.py` (B4) checks this form mechanically: `check` runs the anchor test, `run` replays one
mutant per build, and `verify` re-checks the recorded evidence against the tree it runs in, calling a run that was made
on an older tree stale and not failed. `verify` is in B4's final tool; the copy of the tool that A4's branch carries has
`check`, `run` and `merge` only. R0's two older ledgers record the fields of this form in their own shape (`final` and
`independent` records), and `check` accepts them, on R0's head and on T; `verify` reads the `runs` shape of B4's and A4's
lists, so it reports every mutant of those two ledgers open (see [the seam freeze](p3b-seam-freeze.md#9-verification)). A4's
ledger (`tests/compact_a4_mutants.json`) passes `check` on A4's head; on T two of its anchors match twice (see [Lane A](#lane-a)).

## The ledgers on the integration tree

R0 ran `check` and `verify` of T's `tests/compact_mutant_replay.py` on every compact ledger on `2d405125` (2026-11-05), before any lane
replayed its list there; the last row of the table is R0's own replay on T, which came after (section 7.3 of the seam freeze).
`verify` calls a run stale when the file it ran on differs from the file on T, and `verify --strict`
accepts only runs made on exactly T's sources (`python tests/compact_mutant_replay.py sources`), so no run that a lane recorded before T
is accepted by `--strict`. The counts of the other rows change when a lane replays its list on T.

| Ledger | Mutants | `check` on T | `verify` on T | The stale runs are in files that changed since |
|---|---:|---|---|---|
| R0 seam, `compact_seam_mutants.json` | 91 | every anchor matches once | all 91 open (the ledger has its own record shape, which `verify` does not read) | |
| R0 cursors, `compact_ordered_cursors_mutants.json` | 34 | every anchor matches once | all 34 open (own record shape) | |
| R0 on T, `compact_r0_integrated_mutants.json` (the 52 mutants of `records.rs` and `lib.rs` of the two ledgers above) | 52 | every anchor matches once | 52 accepted by `verify --strict` (104 runs on exactly T's sources, on two machines) | none |
| B4, `compact_b4_mutants.json` | 48 | ok | 25 accepted, 23 stale | `lib.rs` (14), `collection.rs` (6), `compact/source.rs` (2), `records.rs` (1) |
| B4 slice 3, `compact_b4_s3_mutants.json` | 11 | ok | 4 accepted, 7 stale | `lib.rs` (7) |
| A4, `compact_a4_mutants.json` | 217 | two anchors match twice | 99 accepted, 116 stale, 2 open (those two) | `records.rs` (51), `property_graph/retrieve.rs` (28), `kv.rs` (25), `lib.rs` (11), `property_graph/mod.rs` (1) |
| E4, `compact_e4_mutants.json` | 24 | ok | no runs recorded: 24 open | |

## Not established here

This page is the inventory that R0 took, so it states what R0 did not do. R0 did not replay lane A's 26 survivors, lane
B's eleven consumer survivors or lane B's 18 killed mutants, and did not build either lane's branch.
The classification of the evidence comes from the lanes' own reports and commit messages, the mutant lists and a
`git cherry` of each branch against `v0.7.0`.

Since then B4 replayed lane B's eleven consumer survivors (see [Lane B](#lane-b)). It did not replay lane B's 18 killed
mutants as such: eight of them (consumers, settings, rerank and quality) are re-derived in its list, and the other ten
were not replayed as such. A4 replayed lane A's 26 survivors, the other 21 mutants of lane A's ledger and 170 of its own (see
[Lane A](#lane-a)); R0 did not replay them. R0 checked the ledgers of A4, B4 and E4 on T with `check` and `verify` only
([the table above](#the-ledgers-on-the-integration-tree)) and replayed none of their mutants. R0 replayed on T its own 52 mutants of
`records.rs` and `lib.rs` and not the other 73 of its ledgers (section 7.3 of the seam freeze). The coordinator's runs on T,
the library suites, the strict lint and the documentation guards,
are in section 1 of [the seam freeze](p3b-seam-freeze.md#1-baseline) with their counts; R0 ran the 204-test selector of its own replays on T,
and no clippy and no other test target.
