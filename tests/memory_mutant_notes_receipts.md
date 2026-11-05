# Receipts and capability-refusal mutants judged equivalent or not a requirement

Family `receipts`: `receipts/lookup` and the four always-refusing routes
(`facts/query`, `facts/correct`, `erasures/get`, `erasures/resume`). One line per mutant of
`g1-mut-receipts.json`; every other mutant of the list is killed by a test in
`src/server/memory/pins_receipts.rs` (library, clock seam) or `tests/server_memory_receipts.rs`
(full server stack).

g1t-time-clock-recaptured: equivalent, so no test can kill it. In `lookup` the captured `now` reaches the response only as the fourth argument of `MemoryRef::retry_until` (through `mutation_body`), which uses it only in `graph.with_clock(now).get_node(receipt_key)`; `with_clock` hides a node only when the node carries a graph deadline (`NodeView.expires_at_ms`). Receipt nodes are written without one (forget: `GraphOp::CreateNode`/`UpsertNode` with labels and properties only in `MemoryMut::forget`; consolidate: `put_node` in `consolidate.rs`), and `retry_until_ms` is the stored `expires_at_ms` string property, so the value returned is the same for every clock. A second clock sample (mapping section 3, "captures the clock once per request") therefore changes no status, header or body for any clock value, and the seam clock cannot differ between samples anyway. The premise is executable, not just argued: `pins_receipts::a_stored_receipt_expiry_does_not_depend_on_the_read_clock` asserts `expires_at_ms == None` on the stored forget and consolidate receipt nodes and an identical `retry_until` at clocks 0, 1, 99, 100, 60099, 60100, 60101, `i64::MAX` and `u64::MAX`; it fails the day a receipt node gains a deadline, which is when this mutant would stop being equivalent.
