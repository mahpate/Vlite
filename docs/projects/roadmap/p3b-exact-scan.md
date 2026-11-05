# P3b slice 5, lane E4: the bounded exact scan over compact records

| | |
|---|---|
| Status | Integrated behind the test gates on `mahpate-r071-e4`. Nothing compact becomes active: the compact exact scan and every arm that reaches it are `cfg(test)`, and no default, file format or wire shape changes |
| Scope | `exact_search_compact` and `exact_search_compact_with_workspace` in `src/lib.rs`, and their four consumers: the direct helper, `Collection::exact_search_candidates`, `Database::search_in_at` and `ReadSnapshot::search_with_stats_at` |
| Out of scope | Persisted block summaries ([P5](p5-filters-and-serving.md), item P5-71: see section 4), the seam (`src/compact/source.rs`), the ordered cursor itself (`src/records.rs`), early termination (an exact top-k scan reads every candidate row) |
| Reads with | The [seam freeze](p3b-seam-freeze.md) (CLK, ERR, ACC), the [record consumers](p3b-record-consumers.md) that delivered the scan as a draft, and the [declared-index differential](p3b-survivor-ledgers.md) of lane B4 slice 3 |

## 1. What the scan promises

One compact scan, one serial 4 MiB vector workspace, one clock.

| Clause | Promise |
|---|---|
| Distance bits and order | Each distance is the resident one bit for bit: the widened `f32` components, folded left to right in `f64`, with no fused multiply-add. The hits are the `k` nearest, ties by ascending ID, for squared L2, cosine and inner product. A stored row of norm zero has no cosine distance, so it is evaluated but never a hit |
| One clock | The caller's clock decides expiry (a deadline hides the row exactly at the clock), filtering and statistics. `Database::search_in` samples the wall clock once and passes it down; snapshots pass their captured time; the explicit-clock forms pass the caller's |
| Statistics | `records_scanned` counts the candidate rows examined before the visibility check, `distance_evaluations` the visible rows that match the filter, counted when the scan commits to reading the vector, `index_candidates` the declared postings |
| Declared postings | Parity with the resident scan, including overlay changes and the half-of-the-rows cap ([B4 slice 3](p3b-survivor-ledgers.md)) |
| No scoring-only cells | Scoring reads metadata and the vector through `effective_metadata` and `effective_vector`; it never creates a record cell (`get_physical`), so a scan leaves the generation's `decode_count` and its initialized cells unchanged |
| Workspace | At most 4 MiB of decoded blocks, serial, operation-local. A block is decoded whole and costs `rows * dimensions * 4` bytes in either source precision. A block larger than the budget is `Error::InvalidQuery("compact scratch budget is smaller than one block")`; a block that fits alone but not beside the cached ones evicts the least recently used blocks, which is not an error. The constructor refuses more than 4 MiB with `InvalidQuery("compact workspace exceeds 4 MiB")`. A block whose vectors no matching row needs is never decoded |
| Bounded traversal | The scan walks `CollectionRecords::ordered_ids`, the lazy merge of the pinned base and the overlay, not `effective_ids`, which builds a full set and vector first. The workspace bounds the decoded vectors, not the whole operation: the heap holds `min(k, rows, candidates)` hits and a candidate set the caller passes is not counted |

### Errors and accounting

A workspace refusal returns the error and no hits; the scan does not return a partial answer. The accounting is **not
transactional** and the statistics say how far the scan got:

- the row whose vector read is refused was counted in `records_scanned` and in `distance_evaluations` (the counters name
  the attempted row), and no row after it was;
- the refusal is decided before any cache counter changes, so the cache (`vectors_live`, `vectors_peak`, hits, misses,
  evictions) is exactly what the earlier rows left; a second scan with the same workspace refuses at the same row and
  adds only cache hits for the rows before it;
- the metadata read of the refused row has already happened, so an encrypted collection's plaintext accounting has
  moved. Other read failures (a damaged block) can fail after a miss was counted; a refusal for size is the one case with
  an untouched cache, and the freeze note (section ERR) says the same.

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
| `compact_exact_scan_consumes_ordered_ids_across_base_and_overlay` | The scan sees the effective rows in ID order, each once: an unshadowed first base ID, an overlay-only first and last ID, a replaced ID, a tombstone, an overlay deadline over a base row and a later nearest row, for both precisions, plain and encrypted, through the helper, the collection state, the database, a snapshot and the handle |
| `compact_exact_scan_scores_without_materializing_record_cells` | Zero cell and decode deltas at the consumer boundary, on a fresh database per measurement, for the helper, the collection state, the database search and the snapshot, both precisions, plain and encrypted, with and without overlay changes; bitwise distances, order and statistics at explicit clocks (one before the deadline, one at it) for squared L2, cosine and inner product, with ties, zero-norm rows and a filter; literal answers where the arithmetic is exact |
| `compact_exact_scan_workspace_boundaries_preserve_explicit_error_accounting` | Exact-fit and just-short budgets of the biggest block, the exact LRU counters, the refusal with its attempted-row counters and untouched cache, a zero budget, a warmed workspace, ordinary eviction against refusal, a filter that never needs the big block, plain and encrypted |
| `compact_exact_scan_workspace_ceiling_and_the_largest_blocks` | The 4 MiB constructor ceiling and the wrapper's real workspace, over the largest decoded blocks of both precisions (1 MiB and 2 MiB, 512 dimensions); no valid block exceeds 4 MiB, so the ceiling is the constructor's and the wrapper's, not a reachable per-block refusal |

The encrypted runs of the boundary test use the cheaper scans: an encrypted frame is authenticated again on every row
read, which is slow in a debug build. The plain runs cover every scenario, and the wide-block test is plain only.

## 4. The one allowed difference: summaries

The resident scan consults block summaries (P5) when no declared posting serves a filter, so above one 1,024-row block a
filter that summaries can prune but no posting serves makes the resident scan examine only the blocks the summaries keep.
The compact collection has no summaries until P5-71 persists them, so its scan examines every row. The hits, their order,
the distance bits, the evaluations and the index candidates are equal; **`records_scanned` differs**, and
`compact_scan_examines_every_row_where_the_resident_block_summaries_prune` (B4 slice 3) pins it. This amends the
unqualified "summaries parity" of the E4 gate for 0.7.1 and moves the compact summary fallback to P5-71. Nothing about it
is silent: the difference is only in a work counter, only above one block, and only where no posting serves the filter.

## 5. Mutants

`tests/compact_e4_mutants.json` lists 19 hand mutants, each killed by a test of the E4 selector itself
(`compact::source::overlay_tests::exact_scan`): four on the ordered cursor (G2), five on materialization, the metric and
the clock (G3) and ten on the workspace boundaries and accounting (G4). Replay with
`tests/compact_mutant_replay.py` (`check`, `run`, `finish`, `merge`, `verify`); the evidence is the 0.7.1 form (anchor
matches once, clean compile, named failing tests, SHA-256 before, mutated and restored, selector green before and after,
one mutant per build, a replay on a second machine).
