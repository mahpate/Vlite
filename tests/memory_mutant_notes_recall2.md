# Recall mutants, second half (recall2): equivalent or not a requirement

None. All 29 survivors of the `error` (10), `response` (7), `ranking` (6),
`partial` (4) and `barrier` (2) families of `g1-mut-recall.json` are killed by
tests in `src/server/memory/pins_recall2.rs`; none is equivalent and none
is outside the contract.

Readings the tests deliberately leave open (see the final report):

- g1c-response-evidence-alias: the contract (sections 3 and 8) and OpenAPI only
  type `MemoryEvidenceView.collection` as a `MemoryBinding` alias. The adapter
  reports the binding name; mapping section 4 calls the configured evidence
  collection name caller-visible. The test accepts either value and kills the
  mutant's invented `{binding}_evidence` name.
- g1c-partial-budget-refusal and g1c-partial-false-ignored pin only the
  documented first-slice answer (`docs/API.md`, mapping section 6): an
  exhausted budget with `allow_partial: true` is 422
  `memory_bounded_plan_unsupported`. The contract's marked partial 200
  (OPEN-G1R-01) is neither pinned nor excluded.
