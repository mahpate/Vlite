# Consolidate hand-mutant notes (`g1k-*`)

Mutants of `g1-mut-consolidate.json` judged equivalent or not a contract
requirement, one line each: `<id>: <reason, citing the contract clause>`.

None. Every one of the 32 mutants that survived on CI is killed by a test in
`tests/server_memory_consolidate.rs` or `src/server/memory/pins_consolidate.rs`
(see the commit messages for the contract clauses). The other 41 mutants of the
list were already killed by existing tests on CI and were not re-run.
