# P3b slice 5, lane E4: the bounded exact scan over compact records

| | |
|---|---|
| Status | Moved to 0.7.2 with the rest of the compact work (owner decision, 0.7.1 ships the request guard, the memory follow-ups and the dashboard). Integrated behind the test gates on `mahpate-r071-e4`; 24 mutants killed on two machines (section 5). Nothing compact becomes active: the compact exact scan and every arm that reaches it are `cfg(test)`, and no default, file format or wire shape changes |
| Scope | `exact_search_compact` and `exact_search_compact_with_workspace` in `src/lib.rs`, and their four consumers: the direct helper, `Collection::exact_search_candidates`, `Database::search_in_at` and `ReadSnapshot::search_with_stats_at` |
| Out of scope | Persisted block summaries (item P5-71 of the [0.7.0 handover](handover-0.7.0.md) and [P5](p5-filters-and-serving.md): see section 4), the seam (`src/compact/source.rs`), the ordered cursor itself (`src/records.rs`), early termination (an exact top-k scan reads every candidate row) |
| Reads with | The [seam freeze](p3b-seam-freeze.md) (CLK, ERR, ACC), the [record consumers](p3b-record-consumers.md) that delivered the scan as a draft, and the [declared-index differential](p3b-survivor-ledgers.md) of lane B4 slice 3 |

## 1. What the scan promises

One compact scan, one serial 4 MiB vector workspace, one clock.

| Clause | Promise |
|---|---|
| Distance bits and order | Each distance is the resident one bit for bit: the widened `f32` components, folded left to right in `f64`, with no fused multiply-add (the scan feeds the same `exact_kernels::Scorer` as the resident scan, which owns that arithmetic and its own witness tests; the E4 fixtures add rows whose sums change with the order of the fold). A zero inner product is reported as `+0.0` and a cosine ratio is clamped to `[-1, 1]`. The hits are the `k` nearest, ties by ascending ID, for squared L2, cosine and inner product. A stored row of norm zero has no cosine distance, so it is evaluated but never a hit |
| One clock | The caller's clock decides expiry (a deadline hides the row exactly at the clock), filtering and statistics. `Database::search_in` samples the wall clock once and passes it down (one test calls it, with deadlines far in the future; the instant itself is not observable); snapshots pass their captured time; the explicit-clock forms pass the caller's and are what the tests pin |
| Statistics | `records_scanned` counts the candidate rows examined before the visibility check, `distance_evaluations` the visible rows that match the filter, counted when the scan commits to reading the vector, `index_candidates` the declared postings |
| Declared postings | Parity with the resident scan, including overlay changes and the half-of-the-rows cap ([B4 slice 3](p3b-survivor-ledgers.md)) |
| No scoring-only cells | Scoring reads metadata and the vector through `effective_metadata` and `effective_vector`; it never creates a record cell (`get_physical`), so a scan leaves the generation's `decode_count` and its initialized cells unchanged |
| Workspace | At most 4 MiB of decoded blocks, serial, operation-local. A block is decoded whole and costs `rows * dimensions * 4` bytes in either source precision. A block larger than the budget is `Error::InvalidQuery("compact scratch budget is smaller than one block")`; a block that fits alone but not beside the cached ones evicts the least recently used blocks, which is not an error. The constructor refuses more than 4 MiB with `InvalidQuery("compact workspace exceeds 4 MiB")`. A block whose vectors no matching row needs is never decoded |
| Lazy ID traversal | The scan walks `CollectionRecords::ordered_ids`, the lazy merge of the pinned base and the overlay, not `effective_ids`, which builds a full set and vector first. That bounds the memory of the ID walk, not the work: the scan is O(rows) even when a candidate set names few rows, because it tests every ID against the set. The workspace bounds the decoded vectors, not the whole operation: the heap holds `min(k, rows, candidates)` hits, a candidate set the caller passes is not counted, and an encrypted block is authenticated into a plaintext frame of up to the maximum body size outside the 4 MiB budget. No test pins that the loop uses `ordered_ids` directly (both iterators give the same IDs); the cursor mutants of section 5 do, because they would survive a revert |

### Errors and accounting

This is the behavior of the serial test helper today (`exact_search_compact_with_workspace` with a chosen budget); no
public API reports the statistics of a failed search, and a later parallel or batched scan sums its statistics per chunk,
so only the success-path statistics are a contract. A workspace refusal returns the error and no hits; the scan does not
return a partial answer. The accounting is **not transactional**. By the stage that fails:

- **Size refusal in the vector read** (the case the tests pin): the row was counted in `records_scanned` and in
  `distance_evaluations` (the counters name the attempted row), no later row was. The refusal is decided before any cache
  counter changes, so the cache (`vectors_live`, `vectors_peak`, hits, misses, evictions) is exactly what the earlier
  rows left. A second scan with the same workspace refuses at the same row and adds only cache hits when the blocks
  before the refused one fit in the budget together (the staged fixture has one). The row's metadata read has already
  authenticated its frame, so an encrypted collection's plaintext accounting has moved.
- **Failure in the metadata read** (a damaged or tampered frame): the row is in `records_scanned` but not in
  `distance_evaluations`, and the vector cache is untouched. No test drives a damaged frame through the scan.
- **Failure after a miss was counted** (an invalid vector extent, a frame that changed between the two reads): the row
  is in both counters, a miss is counted, blocks may already have been evicted and the cache clock has advanced
  ([seam freeze](p3b-seam-freeze.md), "Refused reads are not atomic"). Not pinned here either.
## 2. Changes

| Change | Where | Kind |
|---|---|---|
| The compact exact loop consumes `CollectionRecords::ordered_ids` instead of `effective_ids` | `src/lib.rs`, `exact_search_compact_with_workspace` (one line) | Production, `cfg(test)` path, no behavior change |
| Tests and the mutant ledger | `src/compact/source/overlay_tests/exact_scan.rs`, `tests/compact_e4_mutants.json` | Tests |

The independent model in the tests is written from the clauses above (visibility, filter, the three distance formulas
folded in `f64`, ties by ID, the statistics rule), not from the scan. The resident oracle is used as a second opinion.

## 3. Tests

| Test | Pins |
|---|---|
| `compact_exact_scan_consumes_ordered_ids_across_base_and_overlay` | The scan sees the effective rows in ID order, each once: an unshadowed first base ID, an overlay-only first and last ID, a replaced ID, a tombstone, an overlay deadline over a base row and a later nearest row, for both precisions, plain and encrypted, through the helper, the collection state, the database, a snapshot and the handle; `k = 0` scans and counts nothing; the wall-clock form |
| `compact_exact_scan_scores_without_materializing_record_cells` | Zero cell and decode deltas at the consumer boundary, on a fresh database per measurement, for the helper, the collection state, the database search and the snapshot, both precisions, plain and encrypted, with and without overlay changes (including a metadata-only change of an expiring row); bitwise distances, order and statistics at explicit clocks (one before the deadline, one at it) for squared L2, cosine and inner product, with ties, zero-norm rows, a filter and `k` larger than the row count; literal answers where the arithmetic is exact |
| `compact_exact_scan_workspace_boundaries_preserve_explicit_error_accounting` | Exact-fit and just-short budgets of the biggest block, the exact LRU counters, the refusal with its attempted-row counters and untouched cache, a zero budget, a warmed workspace, ordinary eviction against refusal, a filter that never needs the big block, a candidate set inside the first block, plain and encrypted |
| `compact_exact_scan_workspace_ceiling_and_the_largest_blocks` | The 4 MiB constructor ceiling and the wrapper's workspace, over the largest decoded blocks of both precisions (1 MiB and 2 MiB, 512 dimensions). No valid block exceeds 2 MiB, so the ceiling is the constructor's and the wrapper's, not a reachable per-block refusal, and any wrapper budget of 2 MiB or more behaves the same through the wrapper (the 4 MiB value itself is pinned by the constructor test only) |
| `compact_exact_scan_folds_dimensions_left_to_right` | Rows whose squared L2 and inner product change with the fold order, against the model, the resident oracle and a literal answer, through every consumer |

The encrypted runs of the boundary test use the cheaper scans: an encrypted frame is authenticated again on every row
read (twice for a cached vector), which is slow in a debug build. The plain runs cover every scenario, and the wide-block
test is plain only.

## 4. The one allowed difference: summaries

The resident scan consults block summaries (P5) when no declared posting serves a filter, so above one 1,024-row block a
filter that summaries can prune but no posting serves makes the resident scan examine only the blocks the summaries keep.
The compact collection has no summaries until P5-71 persists them, so its scan examines every row. The hits, their order,
the distance bits, the evaluations and the index candidates are equal; **`records_scanned` differs**, and
`compact_scan_examines_every_row_where_the_resident_block_summaries_prune` (B4 slice 3) pins it. This amends the
unqualified "summaries parity" of the E4 gate for 0.7.1 (decision of the release coordinator, recorded here) and moves
the compact summary fallback to P5-71. The difference is only in a work counter, only above one block, and only where no
posting serves the filter.

## 5. Mutants

`tests/compact_e4_mutants.json` lists 24 hand mutants, each meant to be killed by a test of the E4 selector itself
(`compact::source::overlay_tests::exact_scan`): five on the ordered cursor and the loop's limit (G2), eight on
materialization, the metric, the clock, the filter and the statistics (G3) and eleven on the workspace boundaries,
accounting and candidate set (G4). Ten of them repeat the mechanism of a mutant in the cursor or seam ledgers
(`compact_ordered_cursors_mutants.json`, `compact_seam_mutants.json`); they are listed again because the E4 selector,
not a cursor or seam test, has to kill them, and they are the ones that would survive a revert of the loop.

**Evidence.** All 24 mutants were replayed on the 0.7.2 integration tree (`2d405125`, source fingerprint `eaf4962586c7`): 24 of 24 killed on the local machine and 24 of 24 on a second machine (three shards), each kill naming E4 tests, with the selector green before and after and every file restored byte for byte. No survivor, no equivalent mutant. `python tests/compact_mutant_replay.py verify tests/compact_e4_mutants.json --strict --min-machines 2` reports 24 accepted, 0 stale, 0 open. Any change to the sources it fingerprints makes the runs stale and needs a new replay.

## 6. Hazards for the release promotion

Recorded now so that the lane that un-gates compact reading does not rediscover them:

- **Candidate sets cost O(rows).** Iterate the candidate IDs and test membership with the overlay-aware `contains_key`
  (same ascending order, same `records_scanned`) instead of testing every ID against the set.
- **Metadata is decoded for every row,** also when the filter is empty, and for an encrypted base row that is a
  whole-frame authentication plus a second one for the vector. Skipping the read when there are no conditions leaves the
  statistics unchanged but also stops reading metadata damage; decide it with the damage semantics.
- **Per-row lookups and a batch of one.** `ordered_ids` drops the row index and deadline the base cursor already
  holds, and the scan scores one row per call where the resident scan scores eight lanes. An additive cursor yielding
  base rows and overlay entries would remove both costs.
- **Parallel scan.** The resident scan splits large inputs and sums per-chunk statistics; the error-path statistics of
  section 1 do not survive that.
- **`expiries` is unused for compact rows** (`effective_visible` reads the base and overlay deadlines); the parameter
  stays only for the resident arm.
- **Invariant breaches report `InvalidQuery`** ("compact scan metadata is missing", "compact scan vector is missing"),
  which the seam freeze reserves for budget problems; a corruption error fits better.
- The seam freeze's deviation rows for the exact scan (the `compact_iter` walk, the resident expiry map and "decodes
  metadata for every row") describe the draft and are stale after this lane; the freeze note belongs to lane R0.