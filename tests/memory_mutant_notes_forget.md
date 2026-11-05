# Forget mutant notes (g1-mut-forget.json, 55 mutants)

Equivalent or not a contract requirement: none. Every mutant of the list is killed; nothing is waived.

- 32 mutants were already killed on CI by the existing replay, unit and integration tests
  (campaign B run 41957876040).
- The 23 CI survivors are killed by the new pins. Each was applied alone with `run_mutants.py`
  and failed the named test; the real code passes all of them.
  - `cargo test --locked --offline --features server --test server_memory_forget`:
    a02 a03 a04 a05 a06 i02 i03 i04 j01 j03 j04 v03 v04 v06 v08.
  - `cargo test --locked --offline --features server --lib server::memory::pins_forget`:
    e03 i05 r09 s02 s03 s04 s05 s06.

Readings the pins rely on (contract text and adapter agree; listed so a later change is deliberate):

- The first-slice erasure notice has exactly five fields and no `manifest_id`, on execution and on
  replay (mapping section 1, E4 gap; the contract's section 5 expects a manifest, see OPEN-G1R-01).
- Shared evidence is pinned as retained (state); no reporting shape is pinned (OPEN-G1R-02).
- `details.field` is `target` for every explicit-target shape fault (count 0 and 4097, a
  non-array `ids`, a bad ID or revision) and `mode` for a missing or null mode; API.md says the field
  is named but not which name, so these are the adapter's convention.
- Not pinned either way: whether a reordered target list under one key is the same request (the
  core digest is order sensitive), and whether Forget also needs `graph_read` (contract section 6
  table lists it for "referenced existing memories").
