# Gap 5: private CSR algorithm parity adapter

Status: **design accepted 2026-11-02; private and unreleased**.
Phase 1 changed documentation only. Phase 2 commit 1 (resident seam) and its TTL-resolution tests are independently accepted and integrated; the CSR source stays gated by section (g).
Baseline: `49c77d6f8f2ed87d85b5a570e74457af248bcd45`.
Candidate P4 view: `50190ded2834abe8efdb8902e629c744c833efff`, inspected through
git objects only; not accepted, integrated or activated by this document.
Paths and line numbers below refer to the baseline unless explicitly marked candidate.
Requirements remain [Gap 5](gap5-graph-algorithms.md), [P4](p4-compact-graph.md)
and the [shared contract](compact-storage-contract.md); none of their gates changes.

## (a) Exact claim and its preconditions

For Degree, WeightedShortestPath, PageRank, LabelPropagation and Betweenness,
the resident and CSR sources must return byte-identical values, scope,
truncation, reasons and errors, with identical ordering, for the same retained
logical graph, request, limits, view sequence, TTL clock and logical stop schedule.
Compare floats by `to_bits()`, including path cost and residual, not epsilon.
This is an acceptance obligation, not a claim that the candidate already passes.

| Term | Exact meaning / exclusion |
|---|---|
| Same retained logical graph | Same physical IDs, keys, labels, properties and deadlines, including expired records and hidden incident edges; committed deletes absent in both. Same effective overlay state, not the same base/overlay split. |
| Same request | Same borrowed selectors, filters, direction, algorithm parameters and caps; same staging and condition-conversion callbacks. |
| Same scope | Sorted seeds, endpoints, normalized echo, projection node IDs and physical edge IDs/endpoints, scope/computation flags and counts. No renumbering to CSR ordinals. |
| Same errors | Same engine variant, fields and reason string, at the same charged operation; unchanged public mapping remains resident-only. |
| Same truncation | Same single stopping reason, DegreeLimit node, last full iteration/sweep/source, output caps and request-echo presence. |
| Same schedule | Fixed fake monotonic clock/cancellation events at the same logical checks; real clocks may stop the adapters at different completed steps. |
| Compared statistics | All integer work, extraction and scratch counters; elapsed time is excluded. No Rust struct-memory comparison or invented wire encoding. |
| Not claimed | Speed, size, memory improvement, cross-platform float identity, serving, checkpoint loading, production activation or release qualification. No benchmark campaign. |

Two literal readings are impossible without an owner decision: equality of
wall-clock interruption outcomes, and budget equality for graphs with equal
visible edges but different retained expired edges. Gap 5 already exempts elapsed
time and variable deadline completion (`gap5-graph-algorithms.md:172-176`).
Here, identity means explicit field/float-bit comparison; no new serializer.
The retention precondition must be reviewed, not silently broadened to arbitrary
post-reclamation checkpoints (see section h).

## (b) Resident access inventory

The layer reads `Graph`, the individual graph in `GraphState`, not the registry.
No algorithm enumerates the registry, property indexes, attachments or vectors.
`algorithm.rs` below means `src/property_graph/engine/algorithm.rs`;
`iterative.rs` means its `algorithm/iterative.rs` child.

| Function / file:line | Resident access and required data | Ordering / visibility | Charging point |
|---|---|---|---|
| `run` / algorithm.rs:550-605 | Passes `&Graph` to borrowed runner; owned request conversion only | Limits/caps/condition shape first | No graph scan or request clone |
| `run_with` / algorithm.rs:620-650 | `has_due(C)`; `edge(EdgeId(0))` for typed scans | Due hint captured once; edge zero is physical, not TTL-filtered | Shape before meter; constant lookup/hint outside item charges |
| `Prepared::new` / algorithm.rs:911-973 | Calls `resolve` for seeds/endpoints; labels through `carries`; personalization through `teleport` | Types, labels, seeds, endpoints, masses in that order; seeds sorted/deduped | Reserve seed buffer; vertex per selector/endpoint; shared checked sorts |
| `resolve` / algorithm.rs:1000-1018 | `node(id)`, `node_by_key(key)`, `node_expired(id,C)`; returns node ID | Validate key before lookup; missing/due ID vs key errors remain distinct | Caller pays one vertex before lookup; no table scan |
| `carries` / algorithm.rs:1022-1024 | `GraphNode.labels.contains` for every required label | All-label conjunction; no node-property dependency | Inside already charged selector/adjacency work |
| `Local::eligible_node` / algorithm.rs:1319-1326 | `node(id)`, `node_expired`; labels | Excluded or due intermediate node cannot bridge scope | Inside charged discovery entry |
| `Local::eligible_edge` / algorithm.rs:1329-1333 | `edge_expired(edge,C)` and `conditions_match(edge.properties,...)` | Own or either endpoint due hides edge; no temporal selection | After adjacency unit, including rejected edges |
| `Local::scan` / algorithm.rs:1340-1372 | `adjacency(node,dir,None,after)`; cached edge zero | Physical `(relationship UTF-8 bytes,edge ID)` order; Both loop once; sorted types probed separately | One vertex per type, absent included; shared per-node entry count |
| `Local::touches` / algorithm.rs:1375-1381 | Edge source/destination only | Direction decides whether edge zero belongs; no TTL here | Inside typed probe, then normal entry charge |
| `Local::step` / algorithm.rs:1386-1406 | `edge(id)` physical record | Missing entry is `InvariantViolation("adjacency names a missing edge")` | DegreeLimit check, edge budget/check, increment, lookup, callback |
| `Local::extract` / algorithm.rs:1419-1494 | Calls scan/eligibility; reads edge ID/endpoints | BFS levels and admitted nodes ascending; Both retains at stored source; final edges by ID | Vertex per seed/expanded/rescanned node, repeated entries again; checked grow/sort/merge |
| `Local::shortest_path` / algorithm.rs:1582-1625 | `graph.edge(projected.id).properties[field]` | All retained weights by ascending edge ID, before endpoint-in-scope answer | Reserve existing weight buffer, edge unit then lookup/classify |
| `iterative::teleport` / iterative.rs:57-102 | `resolve(graph,selector,C)` for masses | Same ID/key and TTL rules, then sort by ID, duplicate and sum checks | Vertex per mass and sum; checked sort and buffer |
| `Local::page_rank` / iterative.rs:160-190 | `rank_weight` for each retained edge | Edge-ID order; forward then reverse Both contribution | Existing edge unit before weight lookup; no early extraction validation |
| `Local::rank_weight` / iterative.rs:287-306 | `graph.edge(id).properties[field]` | Missing projected record invariant; shared `classify`; negative zero normalized | Called within the charged PageRank weight pass |
| `degree`, LPA, Brandes / algorithm.rs:1522; iterative.rs:315; algorithm/betweenness.rs:209 | Projection only; no Graph reads | ID order, fixed arc/pair/reduction order | Existing kernel meter unchanged |

Resident backing: `state.rs:104-134,269-280` holds maps and temporal series;
`state.rs:321-339,624-708` supplies lazy adjacency and merges Both.
`state.rs:367-380` implements the due hint and expiry cascade; `due` at
`state.rs:394` is not called by algorithms. Temporal edge properties are empty
on the ordinary physical record (`state.rs:131-134`), even if a segment exists.

`GraphRef::algorithm` and private `algorithm_with_clock` are in
`src/property_graph/algorithms.rs:378-400`; private `run` at :409-444 builds
`ExecutionContext { view_sequence, now_ms, clock, cancel }` and calls `run_with`.
`GraphRef::with_clock` / `now` at `src/property_graph/mod.rs:157,176` pin or
sample C once. The wrapper does **not** forward its `as_of_ms` to algorithms.

## (c) Chosen seam: small private source trait

| Option | Evaluation |
|---|---|
| 1. Private source trait | **Choose.** Share preparation, extraction and the two late property lookups. Generic Local methods can retain identical instructions/charges; numerical reductions remain unchanged. |
| 2. Source enum | Would couple the std-only engine to compact types or require another indirection layer. No benefit for two private implementations. |
| 3. Extraction only | Insufficient: seed keys/labels/TTL and personalized seeds precede extraction; Dijkstra/PageRank fetch weights afterwards. Moving weights into Projection changes validation timing, error precedence, allocations and MemoryLimit boundaries. |

The trait is not a public generic bound. Preserve `pub fn run` and
`pub fn run_with` with exactly their existing signatures: the latter forwards
to a new crate-private runner with the resident implementation. No API additions
even to the separately compiled `vlite-graph` harness.

Proposed `algorithm/source.rs` module, declared `mod source;`; all items and
fields shown below are exactly `pub(crate)`, trait methods have that effective
visibility. The `impl Source for Graph` adds no inherent methods.
`algorithm.rs` adds `pub(crate) use source::{EdgeRef, Source};` so the root-only
CSR implementation can name them without exposing the private source module.

```rust
pub(crate) struct EdgeRef<'g> {
    pub(crate) id: EdgeId,
    pub(crate) source: NodeId,
    pub(crate) destination: NodeId,
    pub(crate) relationship: &'g str,
    pub(crate) properties: &'g Properties,
}
pub(crate) trait Source {
    fn node(&self, id: NodeId) -> Option<NodeId>;
    fn node_by_key(&self, key: &str) -> Option<NodeId>;
    fn carries(&self, id: NodeId, labels: &[&str]) -> bool;
    fn edge(&self, id: EdgeId) -> Option<EdgeRef<'_>>;
    fn may_have_due(&self, clock: u64) -> bool;
    fn node_expired(&self, id: NodeId, clock: u64) -> bool;
    fn edge_expired(&self, edge: &EdgeRef<'_>, clock: u64) -> bool;
    fn adjacency<'g>(
        &'g self, node: NodeId, direction: Direction,
        after: Option<(&str, EdgeId)>,
    ) -> impl Iterator<Item = (&'g str, EdgeId)> + 'g;
}
pub(crate) fn run_source_with<'s, A: Source, S: Seed, T>(
    source: &A,
    request: &Request<'s, S>,
    limits: &GraphLimits,
    caps: &GraphAlgorithmLimits,
    context: &ExecutionContext<'_>,
    conditions: impl FnOnce(&mut Meter<'_>)
        -> Result<Cow<'s, [PropertyCondition]>, AlgorithmTruncation>,
    stage: impl FnOnce(&Finished<'_>, &mut Meter<'_>)
        -> Result<T, AlgorithmTruncation>,
) -> Result<(AlgorithmResult, Option<T>), AlgorithmError>;
```

`after` is consumed to locate the starting slice before returning the iterator,
never borrowed by it. `EdgeRef` is a stack-only `Copy`/`Clone` view; no owning
labels, properties, strings or extra projection buffers. Imports use engine
module-relative types only, retaining the shared engine boundary
(`src/property_graph/engine/mod.rs:3-18`).

| Existing private item | Exact replacement signature / visibility |
|---|---|
| `Prepared::new` | `fn new<S: Seed, A: Source>(source: &A, request: &Request<'s,S>, limits: &GraphLimits, expired_at: Option<u64>, conditions: Cow<'s,[PropertyCondition]>, meter: &mut Meter<'_>) -> Result<Self,Halt>` |
| `resolve` | `fn resolve<A: Source>(source: &A, selector: Selector<'_>, expired_at: Option<u64>) -> Result<NodeId,GraphError>` |
| `carries` | Removed; equivalent `Source::carries` called at the same sites |
| `Local` | `struct Local<'a,A: Source> { source: &'a A, direction: Direction, types: Option<&'a [&'a str]>, labels: Option<&'a [&'a str]>, conditions: &'a [PropertyCondition], limits: &'a GraphLimits, expired_at: Option<u64>, edge_zero: Option<EdgeRef<'a>> }` |
| `teleport` | `pub(super) fn teleport<S: Seed,A: Source>(source: &A, entries: &[(S,f64)], seeds: &[NodeId], expired_at: Option<u64>, meter: &mut Meter<'_>) -> Result<Teleport,Halt>` |

Other Local method visibility stays private except existing `page_rank`
(`pub(super)`). Replace `&GraphEdge` callback/eligibility/touches parameters
with `&EdgeRef<'_>`; preserve return types and charge locations. Replace
`other_end` calls locally with the same source/destination comparison; do not
change the traversal helper or its callers. No new public/private error type.
`may_have_due` is conservative: Graph delegates to `has_due`; CSR returns true
without a scan. False is permitted only when no record can be due.

Exact Local signatures inside `impl<A: Source> Local<'_,A>` (the existing
`iterative::Rank` and `Iterated` keep `pub(super)` visibility):

```rust
fn eligible_node(&self, id: NodeId) -> bool;
fn eligible_edge(&self, edge: &EdgeRef<'_>) -> bool;
fn scan(&self, node: NodeId, meter: &mut Meter<'_>,
    visit: impl FnMut(&EdgeRef<'_>, &mut Meter<'_>) -> Result<(),Halt>)
    -> Result<(),Halt>;
fn touches(&self, edge: &EdgeRef<'_>, node: NodeId) -> bool;
fn step(&self, node: NodeId, id: EdgeId, examined: &mut u64,
    meter: &mut Meter<'_>,
    visit: &mut impl FnMut(&EdgeRef<'_>, &mut Meter<'_>) -> Result<(),Halt>)
    -> Result<(),Halt>;
fn extract(&self, seeds: &[NodeId], meter: &mut Meter<'_>)
    -> Result<(Projection,u64),Halt>;
fn shortest_path(&self, projection: &Projection, weight_property: Option<&str>,
    endpoints: (NodeId,NodeId), meter: &mut Meter<'_>)
    -> Result<WeightedPathOutcome,Halt>;
pub(super) fn page_rank(&self, projection: &Projection,
    rank: &iterative::Rank<'_>, meter: &mut Meter<'_>)
    -> Result<iterative::Iterated,Halt>;
fn rank_weight(&self, id: EdgeId, field: &str) -> Result<f64,Halt>;
```

### Resource invariant

| Work | Required preservation |
|---|---|
| Source construction | Borrow a prevalidated, already indexed generation; O(1) stack state, no rebuilding Graph, CSR, key maps or overlay snapshot during a run. |
| Adjacency | Lazy physical rows, no Vec/BTreeSet, no TTL/label/condition filtering before `step`; constant-sized cursor state; no per-degree scratch. |
| Typed scans | Keep the edge-zero workaround, one vertex per type probe and exclusive `(kind,0)` cursor. No scanning unrelated type prefixes. |
| Discovery/rescan | Exactly existing grow capacities, moved-item units, level sort/merge, entry counts, Both retention and final edge sort. |
| Weights/staging | Existing late weight passes and callbacks, same reservations/order; no cache or prevalidation. |
| Bounds | Existing 10,000-node, 100,000-edge, 10,000-entry/node ceilings and algorithm caps, no new allowance. All run allocations use Meter before allocation. |
| Hidden backing work | No unbounded tombstone-skip, key-table scan or overlay sort inside `next()`/lookup. P4 must supply indexed effective membership before admission; additional work units would break parity. |

The sorted relationship/ID dictionaries, node/edge indexes and effective
membership are generation-owned source state, not algorithm scratch. Their
construction/ownership must be explicit in P4, never disguised as free
per-request allocation. This design proves no total generation-memory bound.
Without that P4 contract, commit 2 is blocked; do not use a bounded prefix,
allocate then charge, or silently fall back to a rebuilt resident graph.

## (d) CSR source and P4 dependency

| Candidate | Decision |
|---|---|
| Baseline `CsrReader` / `src/compact/graph/read.rs:8-44,103-170` | Do not bind: type/methods private, constructor builds whole-graph indexes, neighbors materializes a Vec and filters TTL before returning; no pinned overlay. |
| Candidate `CompactGraphView` / candidate view.rs:9-12,103-118 | Bind **after independent P4 acceptance/integration and bounded-accessor review**. It owns Arc base plus optional Arc committed overlay snapshot; physical and C/T reads are separate. |
| Candidate physical view | Correct retention-unfiltered record surface, but not a sufficient budgeted scan implementation yet. Do not copy candidate code into the algorithm slice. |
| Overlay | Highest committed version wins; tombstones remove entities, replacements replace properties/deadlines, retained expired edges remain physical entries. Pin one snapshot for the whole run. |
| C / T | C is `ExecutionContext.now_ms`, matching resident wrapper. T is always `None`: current algorithms ignore GraphRef's as-of selector. No new historical algorithm semantics. |
| Sequence | Caller supplies the same database `view_sequence`, not `CompactGraphView::graph_sequence()` (last graph mutation may be older). No implicit clock/sequence refresh. |

Adapter implementation file: `src/property_graph/engine/algorithm_csr.rs`,
compiled **as a child of the root-only `algorithms` adapter**, not the shared
engine, using `#[cfg(test)] #[path = "engine/algorithm_csr.rs"] mod csr;`
in `src/property_graph/algorithms.rs`. This avoids referencing `crate::compact`
from the std-only engine; `src/compact/mod.rs:36-37` is already test-only.
Inside it, `pub(crate) struct CsrSource<'g> { view: &'g CompactGraphView }`
has private `view` and `pub(crate) fn new(view: &'g CompactGraphView) -> Self`.
Implement Source there; use physical node/edge views, and `view.read(C,None)`
only for expiry predicates. No constructor accepts T or mutable overlay.

Minimum requested P4 additions (not authorized edits in this slice):

```rust
// In the accepted view module; existing NodeRef/EdgeRef retain their fields.
impl CompactGraphView {
    pub(crate) fn algorithm_node_by_key(&self, key: &str) -> Option<NodeRef<'_>>;
    pub(crate) fn algorithm_adjacency<'g>(
        &'g self, node: NodeId, direction: Direction,
        after: Option<(&str, EdgeId)>,
    ) -> impl Iterator<Item = (&'g str, EdgeId)> + 'g;
}
```

Both additions are physical/effective-state accounting access, not user reads.
The first requires an indexed base+overlay key lookup, no node scan; the second
requires canonical indexed base+overlay membership, no materialization or
unbounded deleted-entry walk. Existing `physical().node/edge` and
`read(C,None).node_expired/edge_expired` suffice for all other needs.
The physical edge view, not a read-filtered edge lookup, supplies conditions
and named weights; a temporal series with no ordinary properties therefore
still produces MissingWeight when retained and a named weight is requested.
P4 owns any indexes and snapshot/publication changes these require; this slice
must not edit overlay, view or framing files. No property-index accessor needed.
Recommended order: resident seam first; P4 accept view plus these bounded
accessors next; CSR adapter and overlay/base parity next; independent review;
serving/loader/activation remain separate, unchanged gates.

## (e) Determinism, errors and truncation

| Dimension | Required rule |
|---|---|
| Adjacency | UTF-8 relationship byte order then u64 edge ID, never ordinal/destination order; type cursors strict; edge 0 included exactly once. |
| IDs | Sparse IDs 0 and large representable IDs preserved; allocate by admitted count, never max ID. `u64::MAX` cannot be stored; retain existing fixture remapping/rejection rules. |
| Insertion/threads | Reverse construction with preserved IDs, seed order and base/overlay split cannot alter output. Run in fresh 1/2/4-thread configurations; kernels remain serial, no new parallel implementation. |
| Hidden/expired | Charge physical adjacency before hiding due edges/incident endpoints or applying filters; equality C=deadline hides; no mid-run wall-clock sampling for TTL. Tombstones are absent, not TTL-hidden entries. |
| Parallel/loops | Distinct physical IDs kept for degree/paths/PageRank; Both loop once; LPA/Brandes retain existing collapse/loop-removal inside kernels only. |
| Dijkstra | Exact fixed-point sums; cost, hops, lexicographic edge-ID sequence; round winner once. Unit and named weights unchanged. |
| PageRank | Existing node/edge reduction order, Both reverse second, synchronous steps and last-full-vector rules. |
| LPA / Brandes | Ascending-ID immediate sweeps and canonical labels; SplitMix64-v1 sampling, arc order, source-prefix commit and normalization unchanged. |
| Errors | Shape errors precede meter start; item errors only when reached; key vs ID errors distinct; smallest bad retained edge by ID; late numeric errors stay errors even after completed work. |
| Stops | First reason only; exact Degree/Edge/Vertex/Memory precedence; no values on extraction stop; last full iterative step/source only; final check and output caps unchanged. |

Use the shared `Meter`, `push`, `merge_sort`, `cap` and result assembly without
new logical charges. Inject the same clock-check trace for each adapter,
including cancellation at start/periodic/final checks. Real elapsed time and
real-time source counts are explicitly not a bitwise reproducibility claim.

## (f) Fixtures, counts and fail-first evidence required in phase 2

Existing tests and fixtures remain byte-for-byte unchanged, not merely green.
Read-only source counts at this baseline: 53 engine tests (20 base, 17 iterative,
16 betweenness), and 29 public-adapter tests (12 base, 8 iterative, 9 betweenness).
The older narrative counts in Gap 5 predate later sort/staging regressions.
There is no `tests/graph_algorithms*.rs` suite at this baseline; tests live in
`src/property_graph/engine/algorithm/tests{.rs,/**}` and
`src/property_graph/algorithms/tests{.rs,/**}`.

| Matrix | Required coverage / expected count |
|---|---|
| Cross-product skeleton | 5 algorithms x 12 fixture families x 3 sources x 3 thread settings = **540 comparisons** per construction order; reverse-preserved-ID order doubles this to 1,080. Sources: resident, CSR base, CSR base+overlay. |
| Twelve families | chain; weighted diamond/ties; dangling/isolates; parallel/loops/edge-zero; sparse IDs/types; labels/conditions; TTL boundaries; temporal ordinary-record semantics; overlay replacement/delete; high-degree/absent-types; invalid inputs/weights; exact budget/partial-output boundaries. |
| Directions | Run Out/In/Both within every applicable matrix cell; all 540 cells must execute, including negative/error cases, no silent skip. Multiple requests and budget sweeps add cases, not reduce the skeleton. |
| Golden scope/path | 24 `golden.json` scopes/degrees; 11 path cases, 6 stationary PageRanks, 3 LPA and 4 betweenness cases, preserving existing consumers. |
| Finite iterations | 12 `iterative-cases.json` cases, 17 full steps/sweeps; compare independent expected scores within 1e-14 and exact communities; adapters mutually bit-identical. |
| Sampled centrality | 23 `betweenness-cases.json` classifications, 7 SplitMix64 vectors; preserve 20 readable / 3 rejected classifications and existing max-ID remaps, not pretend all fixtures are directly storable. |
| Caps | For each small case, every edge/vertex/scratch budget 1..full requirement plus full/full+1 when legal; zero/ceiling violations separately. Output/iteration/source caps and DegreeLimit boundaries too. |
| Oracle independence | Hand-built retained record tables and expected ordered scopes/degrees; exhaustive paths and existing Python rational oracles. Never expected output solely from GraphSource::from_graph, encoder, overlay fold or resident execution. |

Retain `tests/graph_algorithm_oracle.py`, `graph_iteration_oracle.py` and
`graph_betweenness_oracle.py` and their JSON unchanged. Existing oracle
tolerances compare mathematical references, not adapter equality.
Add tests in `engine/algorithm_csr/tests.rs`; root-only test inclusion above.
Proposed named tests below are **not implemented or claimed to have failed**.
For each mutation, run that named test, retain actual failing assertion evidence
in the review report, restore implementation, rerun successfully; no mutation
commits or production hooks.

| Deliberate mutation | Named new test that must fail |
|---|---|
| Sort CSR row by neighbor ordinal, or reverse relationship dictionary | `csr_physical_order_and_typed_probes_match` |
| Skip ID 0 in typed probe, or duplicate Both loop | `csr_edge_zero_parallel_and_loops_match` |
| TTL `<` changed to `<=` visibility, or endpoint cascade omitted | `csr_ttl_equality_and_hidden_entry_charges_match` |
| Use read-filtered adjacency instead of physical entries | `csr_ttl_equality_and_hidden_entry_charges_match` (exact EdgeLimit/DegreeLimit/counters) |
| Validate weights while extracting or choose relationship-order bad edge | `csr_weight_error_order_and_cancel_precedence_match` |
| Select temporal segment at T=0 | `csr_temporal_records_keep_retention_only_semantics` |
| Allocate degree-sized Vec before Meter | `csr_high_degree_scan_allocates_no_row_buffer` (allocation instrumentation plus cap boundary) |
| Scan every node on key miss | `csr_key_misses_use_indexed_lookup` (test-only source-visit instrumentation) |
| Ignore overlay tombstone/replacement or rebind snapshot mid-run | `csr_overlay_generation_stays_pinned` |
| Keep parallel Brandes arcs, halve Both, change SplitMix input | `csr_betweenness_oracle_and_completed_prefix_match` |
| Expose partial step/source or move final check | `csr_all_budget_boundaries_and_check_traces_match` |
| Add one scratch reservation / type-probe charge | `csr_all_budget_boundaries_and_check_traces_match` |

New instrumentation must detect backing-record visits, tombstone skips and
allocations as well as result equality; an equal answer alone cannot prove
bounded access. High-degree fixtures exceed the entry cap and a large excluded
type prefix; all-deleted backing rows must not create an unmetered linear scan.
Keep engine `every_budget_is_exact_and_stops_without_output` (:806),
`absent_relationship_type_probes_are_charged_and_interruptible` (:1929),
iterative `shared_sorts_keep_degree_and_path_results_and_units` (:1224), and
betweenness `every_budget_stops_at_its_boundary_and_keeps_only_full_sources`
(:694) unchanged, alongside all other existing tests.

## (g) Phase-2 commits, ownership and risks

| Order | Files owned by algorithm slice | Exit before next commit |
|---|---|---|
| 1. Resident-only seam | `src/property_graph/engine/algorithm.rs`, `algorithm/source.rs`, `algorithm/iterative.rs` | Existing signatures, all 82 existing tests and oracle consumers unchanged; exact counters/float bits unchanged. No compact imports. |
| Dependency, P4-owned | Accepted view, indexed physical key/adjacency accessors and generation construction/tests | Independently reviewed contract for bounded effective membership and snapshot pinning. Not part of algorithm commit. |
| 2. CSR source | New `src/property_graph/engine/algorithm_csr.rs`; one test-only module declaration in `src/property_graph/algorithms.rs` | Base and overlay source contract tests first; no public selection/serving switch. |
| 3. Parity evidence | New `src/property_graph/engine/algorithm_csr/tests.rs` and private instrumentation there | Full matrix, independent expected results, real fail-first evidence; existing fixtures/tests unchanged. |
| 4. Documentation | This file and the two current status locations | Record actual evidence and residual limitations, no invented qualification. |

No ownership of compact overlay/view/framing, feed/change/memory, loader,
format/records/directory, server, proto, OpenAPI, site, CLI or bindings.
P4 must own even the two minimal accessors; algorithm code only consumes them.
Risks: hidden-entry accounting, late weights, source lifetime leakage,
standalone harness compilation, per-generation indexes disguised as query
scratch, stale overlays, ID remaps, and timer-dependent outcomes.
No dependency install, code build or algorithm execution is part of phase 1.

## (h) Coordinator decisions and verified disagreements

Rulings, 2026-11-02: every default in the first table is approved.
Sources are compared on the retained logical graph and on fixed logical stop schedules.
The internal physical-accounting access is a crate-private algorithm path, never a
user-visible read; ordinary reads keep expiry-before-return and the shared contract is unchanged.
T stays None. Commit 1 (resident seam, all 82 tests and the oracle consumers unchanged) may start now.
The CSR source commit is gated on separate P4 acceptance of the view and an independently
reviewed bounded key-lookup and adjacency accessor pair. If P4 cannot supply them, CSR
implementation stays blocked; budgets are not expanded and rows are not materialized.
No gate, policy or activation changes.

| Open decision | Recommended default / consequence |
|---|---|
| Exact claim versus reclamation | Approve the retained-record equivalence precondition, not merely visible-graph equivalence. Otherwise strict budget/truncation parity is blocked; do not alter resident charges or checkpoint reclamation policy. |
| Real-clock byte identity | Compare fixed logical check schedules; exclude elapsed time as already documented. Unconditional real-time identity cannot be promised. |
| Candidate view acceptance | Require separate P4 acceptance/integration and bounded accessor review before commit 2. Resident seam can proceed after this design is accepted. |
| P4 physical access versus shared visibility wording | Confirm the internal accounting accessor is not a user-visible read; preserve expiry-before-return for ordinary reads and apply algorithm visibility after metering. No shared-contract edit is authorized here. |
| Test-only inclusion ownership | Approve the single module declaration in `algorithms.rs` to keep compact imports out of the standalone engine; all adapter implementation remains in the new engine-directory file. |
| Generation indexing | P4 supplies bounded key lookup and effective ordered adjacency, including tombstone-heavy rows, off the read path. If unavailable, block CSR implementation; do not expand algorithm budgets or materialize rows. |
| T selector | Preserve today's T=None algorithm behavior, including calls on an as-of GraphRef; historical analytics requires a separately reviewed API decision. |

| Documentation / code mismatch found | Treatment in this candidate |
|---|---|
| Gap 5:37 requests a storage-neutral adapter; baseline has direct Graph reads | Proposed private Source, no claim of completion. |
| Projection-only seam suggested by extraction shape | Refuted by seed/personalization resolution and late path/PageRank property reads, inventory above. |
| Candidate view.rs:210 scans nodes for key; :336 scans for due; :355-400 allocates set/Vec and filters visibility | Cannot use directly for budget parity; require two P4 additions and conservative due hint. |
| Shared contract:294-296 says deletes and expiry apply before a candidate leaves an accessor | Internal physical accounting needs coordinator confirmation; ordinary read accessor semantics stay untouched. |
| Broad temporal read availability versus algorithm wrapper | `algorithms.rs:395-444` forwards C but not T; preserve physical ordinary-record properties, not segment-selected properties. |
| Gap 5 coverage says 10 S3 engine / 7 adapter tests | Baseline source has 17 / 8 after later regressions; full counts above are source-inspected, not execution evidence. |
| P4:309 says codec not wired to framing/checkpoint reads, while P4 opening status and compact contract:351-376 describe accepted guarded framing | Older paragraph is stale; this design relies on neither activation nor a production compact executor. |

Unverified: proposed Rust signatures have not been compiled; matrix counts are
planned executions, not passing results; bounded P4 accessors do not exist in
the inspected candidate; candidate tests were read, not run; cross-platform
float identity, runtime parity and memory/performance qualification are not
established. Phase-1 validation is documentation checks only.
All new test modules, test functions and instrumentation helpers are private;
there is no new persisted request, fixture schema, format tag or public item.
