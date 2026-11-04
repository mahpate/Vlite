# Independent write fixtures

Source: `docs/projects/roadmap/gap6-cypher-subset.md` sections 3-7, 9, 11-12.
Original local scenarios only; no imported TCK or Rust-generated expectations.

Run `python -B tests\graph_subset_write_oracle.py --self-test`, also with `-O`.
`evaluate_write(graph, statement, params=None, limits=None, clock=None)` imports
the accepted Python parser and value/read primitives without modifying them.
It returns ordered plain staged operations, frame count, counters, RETURN cells,
and a complete graph. Failure returns the entire original graph and no committed
operations/frames. Graph IDs, revisions, sequence, expiry are decimal strings.

`write-cases.json`: schema `vlite.graph-subset-write`, version 1, top-level
`schema`, `version`, `content_hash`, `graphs`, `cases`. The hash excludes only
`content_hash` and uses sorted keys, compact UTF-8 JSON, `ensure_ascii=False`,
`allow_nan=False`, exactly as the read fixtures. All files use LF.

Graphs extend the read graph with `next_node_id`, `next_edge_id`, `attachments`,
`records`, and `schema` (`nodes` and `edges` rule arrays). Attachments have
`node_id`, `collection`, `record_id`; vector records are opaque retained objects.
Node rules have `label`, `properties`; each field has `type`, `required`, `unique`.
Edge rules have `type`, `properties` (type/required), `endpoints` label pairs,
`unique_triple`. These are reference-input shapes, not a proposed public API.

A graph specification is a shared graph name, an entire graph, or a declarative
overlay: `base`, optional `put_nodes`, `drop_nodes`, `put_edges`, `drop_edges`,
and `replace` for graph-level fields. Overlay replacement is by ID, never by
matching the query. Expected overlays are hand-authored full-state shorthand;
they are expanded independently of the evaluator. Unmentioned fields survive.

Large arrays permit a bounded declarative element
`{"series":4000,"start":1,"template":{...}}`: repeat the literal template,
substituting `{i}` in string values with successive decimal IDs. Count and
start are integers in 0..10000. No evaluation, queries or arbitrary expressions
are allowed. Tagged String values may use the read fixture's long-value form.
This keeps exact 4,096-operation and 16 MiB witnesses reviewable. The two
body-cap pairs use 4,000 edge patches and one deduplicated node patch, not
lowered test caps or synthetic operation sizes. Each case cites its arithmetic.

Every case has `id`, `group`, `origin: local`, `cite`, `graph`, `text`, `params`,
`expect`, optionally `limits` and `clock`. Typed parameters and long String
forms mirror the accepted read fixtures. Success expectations contain `graph`,
`operations`, `body_bytes`, optionally `columns`, `rows`, `binding_rows`,
`reclaim_entries`, `detach_edges_examined`. Errors contain only `error`.
All defaults are structural constants, not computed by running the evaluator.
Complete expanded results are compared, not selected properties or hashes.
An explicit excluded execution case instead has
`expect: {"not_modeled":["execution clock controls"]}` and returns NOT_MODELED,
unchanged graph, zero operations and zero frames. Execution-clock controls
are never silently ignored. Error expectations carry their full public kind,
offset (for language/envelope diagnostics) and fixed reason where specified.
`["commit sequence exhaustion"]` likewise leaves the graph untouched rather
than inventing a storage error or returning a revision above u64 max.

Body arithmetic is pinned in `tests/graph_subset_write_report.md`. No-op patches
still write revisions. MERGE hits and identical keyed edge retries do not write.
The original graph's `view_sequence` identifies the read view; `revision` is the
single new frame sequence, or null. Staged new entities all have that revision.
First existing-entity patch/delete carries its revision precondition; subsequent
operations under the same retained guard use `any`.

Execution time, scratch execution, cancellation, locking, I/O/indeterminate
commits, durability and reopen are NOT_MODELED, explicitly listed in results.
No claim about filesystem atomicity or performance follows from this oracle.

The final corpus has 164 original scenarios and a strict hashed
`write-mutant-map.json` (`vlite.graph-subset-write-mutants`, version 1).
Each mutant has exactly `name`, `change`, and nonempty `cases`; all 32 actual
switches must appear once, and every named scenario must fail exact comparison.
There are 38 named kills, zero survivors/equivalents. Exceptions are failures,
not kills. Switches reset in `finally`, then mapped baselines run again.

Self-test also rejects 46 malformed fixture/map/raw-JSON copies; checks 14
independent numerical/bit boundaries; detects three valid, rehashed but tampered
expected goldens; runs five seeded graph/parameter/case shuffles; compares text
and AST inputs; and checks actual graph/parameter/control immutability.
Float/GeoPoint storage comparisons include signed-zero bit identity.
No self-test writes fixture files, samples wall clocks, uses unseeded randomness,
or depends on assertions. Both normal and optimized Python must pass.

Limits validate before execution, never clamp. Epoch C is captured once;
later `ttl_samples_ms` entries exist only to kill clock-resampling mutants.
Work uses a shared deterministic reference ledger, not a normative Rust sort
schedule. Scratch/deadline execution and inherited C-to-i64 error classification
are not newly frozen. See the write report for byte citations, exact cap
arithmetic, counters, milestone evidence and explicit exclusions.
