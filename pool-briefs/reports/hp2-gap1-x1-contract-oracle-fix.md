# HP-2 fix-forward report (review fixes 1-7)

Candidate `2a8cf47` on `fa4b355`; three permitted files only. Status stays PROPOSED.

- Fix 1: a byte-identical second SetNodeExpiry (24 bytes) is the summary guard
  (`src/property_graph/mod.rs:301-306`), counted physically and logically. A
  fixture commit walk at max(C, wall, high-water) replaces the planning check.
  Rejected: PatchNode (re-encodes the node), receipt edge (extra state),
  memory-layer check (no commit clock).
- Fix 2: witness binds `c`; receipt 527, body 2,077.
- Fix 3: pinned `derived-x1:v2` key, 199-byte edge, entry lengths and hex
  outcome index `000a`.
- Fix 4: frame pins all-retain 2,250, mixed 2,358, unshared 2,125, shared
  2,123; logical 4,096 accepted, 4,097 refused; composite role order.
- Fix 5: extra-chunk probe at index K, closure paging budget, result fields.
- Fix 6: E1 matrix (`DeletionPending`, pre-barrier replay), S2 `None`, serial
  merge.
- Fix 7: Serving reconciliation; owner wire decision for slice D.

Mutation: 66 cases, 32 killed (19 new names), 1 declared equivalent.
`manifest_chunk_cap7` survives: exact key set, full chunks and length
2*(4+41N), N<=256, refuse seven chunks first. A guard-before-expiry mutant is
not added: the ops are byte-identical, so it is fully equivalent.

Checks: X1 and three sibling oracles plain and `-O`; release_docs,
agent_docs, content_budget; `git diff --check`; CR=0.
