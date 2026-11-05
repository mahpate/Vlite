# Recall mutants judged equivalent or not a contract requirement

Scope: the recall survivor families `auth`, `refusal`, `request`, `json`, `scopes`, `seeds`,
`kinds`, `text`, `candidates` and `output` (27 survivors of CI run 41957876040). The other
families (`error`, `response`, `ranking`, `partial`, `barrier`) belong to the `recall2` worker.

Equivalent or not-a-requirement: none. Every mutant of these families changes behavior the
contract requires and is killed by a test in `src/server/memory/pins_recall.rs` (library,
Clock seam, real routes over `tower::oneshot`) or `tests/server_memory_recall.rs` (real server
stack, credentials file and middleware). No line is owed.

Readings deliberately not pinned (the author's OPEN-G1R items):

- OPEN-G1R-02 (order of several field refusals): the refusal tests combine only fields whose
  winner is the same whether the order is read as JSON text order, key order or schema order
  (`embedding_provider` over `filter`, `projection`, `text`; `filter` over `projection`, `text`;
  `kinds` over `projection`, `text`; `projection` over `text` and `vector`). The full schema
  order stays as the existing replay test leaves it.
- OPEN-G1R-03 (four or five distinct kinds are unreachable): only the two served kinds, and the
  absent default, are exercised.
- Fields of unspecified error members (an unknown key in `ranking`, `limits` or the body, and a
  badly typed budget) pin status and `invalid_input` only; `details.field` is asserted where it
  is the name of the request field itself (`limit`, `text`, `scopes`, `entity_seeds`, ...).
