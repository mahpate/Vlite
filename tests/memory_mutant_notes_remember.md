# Remember mutant notes (`g1r-*`, list `g1-mut-remember.json`, 80 hand mutants)

Closure state against the candidate `bc20f8b6`: CI run 41957876040 judged 59 mutants
killed by the existing suites and 21 survivors. All 21 survivors are killed by
`src/server/memory/pins_remember.rs` (verified one by one with the local runner and a
command that runs only those tests). The full-stack checks live in
`tests/server_memory_remember.rs`; they also kill `g1r-order-auth-write`,
`g1r-limit-body-max` and `g1r-limit-body-max-plus-one`, whose list command runs only
`--test server_memory` (add `--test server_memory_remember` to make them independent of
the real-clock tests in that file).

## Equivalent or not a contract requirement

None. Every mutant of the list changes behavior the serving contract (sections 3, 4, 6
and 8) or the adapter mapping (sections 1-4) requires, and each is killed by a test, so
there are no `<id>: <reason>` lines.

## Anchor drift (not an equivalence)

- `g1r-order-writable-after-refusal` has no match at `bc20f8b6`: its `old` text puts
  `bound.covers(db)?;` directly before `refuse(...)`, but `fields.drop_empty_evidence();`
  now sits between them. Re-based anchor, same defect (the writable precheck moves behind
  the refusals): `old` = `writable(db)?;` / `bound.covers(db)?;` /
  `fields.drop_empty_evidence();` / `refuse(bound, fields.raw(), remember_refusal)?;`,
  `new` = the same three calls in that order followed by `writable(db)?;`. The pins kill
  it (`a_read_only_node_answers_read_only_before_refusals_and_validation`).

## Open readings (no test pins either side)

- OPEN-G1R-01: an item-level `vector` answers 422 (unknown field), while mapping section 1
  lists `vector` among the refused fields. Unchanged and unpinned, as directed.
- OPEN-G1R-02: persisted binding records and the per-write generation recheck are not
  implemented (mapping section 4); adoption recognizes any existing memory scope.
- OPEN-REM-01: an explicit `provenance: []`. The schema has no minimum, section 8 says
  sources default provenance to `[source]`, and the independent oracle keeps an explicit
  empty list literally empty. The core cannot store it (its own default entry has no
  `source_kind`, so Recall answered 500). The adapter now answers like an absent list; a
  422 reading is also possible. The pin only requires that an accepted write stays
  readable. The consolidate output shares the same parser.
- OPEN-REM-02: the adapter applies the opaque-key rules (non-empty, no control character)
  to a source `uri`, which the OpenAPI schema bounds only by `maxLength: 256`. Not pinned.
- OPEN-REM-03: integer inputs given as JSON floats with a zero fraction (`90.0`, `1e3`) are
  rejected as not exact integers; "exact JSON integers" could also mean integral values.
  Not pinned.
