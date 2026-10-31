# Gap 5: bounded graph algorithms

Status: S1 and S2 are implemented and independently reviewed in embedded Rust, unreleased.
The surface and budget corrections in `1202f3cc` are accepted; integration
`9ad5b63a` adds exact-sum reference cases and the API-reference entry.
S3 (PageRank and label propagation) is implemented in embedded Rust, unreleased,
and awaits independent review of its additive surface; S4 and S5 remain design.
Wire surfaces are out of scope until separately approved.
Audit citations refer to base `d530b4c`; TTL integration is reconciled with `cf146fe`.
Open means not verified or not decided. No performance gains are claimed.

## Why revisit the deferral

The PRD deferred weighted paths and centrality/community analytics for
"unmeasured demand, unbounded cost" (`docs\projects\feature-agent-graph\feature-agent-graph.prd.md:121`).
Gap 5 names ranking, clustering and GraphRAG summaries as the intended demand
(`docs\projects\product-gaps\product-gaps.md:50`). The approved roadmap accepts this demand.
The release plan includes graph depth (`docs\projects\roadmap\next-major.md:22`).
Bound extraction and computation; report incomplete work. No all-paths or k-shortest-path feature.

## Verified baseline

| Current fact | Evidence |
|---|---|
| Typed reads cover gets, find, neighbors, traversal and unweighted shortest path. | `src\property_graph\read.rs:174-186` |
| Shortest paths are unweighted and use edge-ID sequence tie-breaks. | `src\property_graph\read.rs:148-158`; `src\property_graph\engine\traverse.rs:1-10` |
| Graph state has node/edge maps and in/out adjacency in memory. | `src\property_graph\engine\state.rs:24-48` |
| Lazy adjacency is ordered by relationship type and edge ID; Both deduplicates self-loops. | `src\property_graph\engine\state.rs:187-215` |
| Reads expose completeness, reasons, sequence, statistics and a cursor. | `src\property_graph\read.rs:313-326` |
| A truncated shortest-path search is Exhausted, not NoPath. | `src\property_graph\engine\traverse.rs:242-247,315-323` |
| Query text delegates to the typed read. | `src\property_graph\read.rs:852-865` |
| Property variants include i64 and f64. Non-finite floats fail validation. | `src\property_graph\engine\model.rs:22-30`; `src\property_graph\engine\properties.rs:33-37` |

Current: [graph TTL](../../GRAPH.md#expiry-ttl) hides due nodes and their incident
edges at one read clock. Algorithms must reuse that visibility rule.
Decided input: a compact CSR checkpoint is planned, not required. Use a storage-neutral adjacency adapter.

## One bounded scope

Require one graph and 1..=max_visited_nodes seeds. Resolve, deduplicate and sort IDs; reject missing/excluded seeds.
Require explicit relationship types and a node-label filter; an explicit Any is allowed.
All listed labels must match, on seeds and every intermediate node.
This is stricter than a return-only filter: excluded nodes cannot bridge the scope.
Also accept edge property conditions and Out, In or Both direction.
Extract nodes by deterministic breadth-first expansion, through at most H hops from seeds.
Then scan admitted nodes' adjacency to retain every eligible edge between admitted endpoints.
Include boundary nodes. Charge repeated examinations again.
Out keeps stored arcs; In reverses them; Both adds both arcs, once for a self-loop.
Deduplicate retained physical edge IDs. Count each computational arc when processed.
This is an induced local projection. The hop radius is scope, not truncation; paths also have an H-edge bound.

### Limits shared across all phases

| Limit | Current default | Current ceiling |
|---|---:|---:|
| H / max_hops | 2 | 4 |
| Distinct admitted nodes, including seeds | 1,000 | 10,000 |
| Edge examinations | 10,000 | 100,000 |
| Adjacency entries per node expansion | 1,000 | 10,000 |
| Deadline, milliseconds | 250 | 5,000 |
| Output nodes / edges / paths | 100 / 200 / 20 | 1,000 / 2,000 / 100 |

Verified in `src\property_graph\engine\limits.rs:70-109`.
Above-ceiling and zero work limits fail, never clamp (`src\property_graph\engine\limits.rs:126-161`).
Reuse these values; do not silently grant an algorithm larger budgets.
The edge budget covers extraction, validation and repeated passes. Rejected and reused edges still cost units.
Add proposed caps: vertex operations 100,000 default / 1,000,000 ceiling.
Add iterations 20 / 100, sampled sources 16 / 64, scratch bytes 64 MiB / 256 MiB.
Account buffers, heap entries and output staging before allocation; stop with MemoryLimit.
Charge seed resolution, sorting comparisons, heap actions and vertex loops to vertex operations.
The per-node adjacency cap applies to extraction; iterative passes also pay the total budget.
Check cancellation/deadline before work, at most every 64 charged operations and before return.
Current meter checks at 64 edge units; its final clock read only measures elapsed time
(`src\property_graph\engine\meter.rs:45-107`). Extend accounting, not just its call sites.
Use a monotonic deadline clock separate from TTL. Cancellation is cooperative, not hard real-time.
If extraction truncates, return Exhausted with no analytic values. Never silently analyze a prefix.
Computation may expose its last fully completed iteration or sample batch, clearly incomplete.
Output caps apply after computation, not as exploration bounds (`src\property_graph\engine\limits.rs:81-84`).
Whole-graph execution is rejected in v1. A graph may incidentally fit the explicit local scope.
Open: separate admin/offline jobs, only with explicit opt-in, authorization and independent quotas.

## Edge weight contract

`weight_property: Option<String>` names one edge property; None means unit weight 1.
When named, missing values are errors. Never silently drop an edge or assume 1.
Accept finite f64 and integers within [-2^53, 2^53], converted exactly to f64.
Reject other integers rather than silently round. Reject strings, booleans and geo values.
Reject negative values, NaN and infinities. Normalize negative zero to positive zero.
Zero is allowed. Check sums, distances and scores for overflow; return NumericError.
Validate all retained weights before computing, even on edges not used by the winning path.
Recheck finiteness despite property validation (`src\property_graph\engine\properties.rs:33-37`).
Dijkstra and PageRank accept weights. v1 community detection and centrality reject weight options.
No stored schema change or default property named "weight" is required.

## Exact algorithm semantics

### Weighted shortest path

Run Dijkstra on states (node ID, hops used), for 0..=H hops.
Resolve both endpoints; source must be a scope seed. An excluded destination has no scoped path.
This avoids discarding a costlier short prefix needed to satisfy the hop bound.
Keep an indexed heap and at most (H+1)*V states, not one heap entry per relaxation.
Rank candidates by total cost, then fewer hops, then lexicographic edge-ID sequence.
Use exact f64 comparisons, not epsilon ties. Source equals destination returns cost 0.
Parallel edges remain separate; zero-cost cycles cannot win over a shorter equal-cost path.
Reject negatives without fallback. Exhaust states needed to certify the destination and its tie-break.
Found and NoPathWithinScopeAndHops require complete extraction and certified computation.
Any work, deadline or output truncation returns Exhausted, not an uncertified shortest path.

### PageRank

Default damping d=0.85, tolerance=1e-8, max_iterations=20; ceiling 100.
Require finite 0<d<1 and finite 0<tolerance<1, with positive iterations.
Teleport distribution p is uniform over all scoped nodes by default.
Personalisation accepts explicit scoped seed IDs with positive finite masses, normalized to p.
Unlisted nodes have p=0; reject unknown seeds, duplicate IDs and zero total mass.
Start r=p. Let W(u,v) sum parallel arc weights, and s(u)=sum_v W(u,v).
For s(u)=0, treat u as dangling, including nodes with only zero-weight outgoing arcs.
One synchronous step is r'(v)=(1-d)*p(v)+d*(sum_u r(u)*W(u,v)/s(u)+D*p(v)).
The sum excludes dangling u; D is their total rank mass. Self-loops contribute normally.
Stop after a complete step whose L1 change sum_v abs(r'(v)-r(v)) <= tolerance.
Return the last full vector, residual, completed iterations and converged.
Iteration cap means complete=false and IterationLimit unless that final step converged.
No full step before interruption means no scores. Values refer only to this projection.

### Community detection: label propagation first

Choose bounded label propagation first: small state and a simple oracle, not modularity optimality.
Symmetrise retained edges, collapse parallel neighbor pairs and ignore self-loops.
Start each node with its own ID as label. Isolates keep their label.
Each sweep visits ascending node IDs and updates labels immediately, in that order.
Choose the most frequent neighbor label; keep the current label if it is tied for first.
Otherwise choose the smallest tied label. No random scheduling or random tie-breaks.
A sweep with no changes converges. Cap at 20 sweeps by default, ceiling 100.
On interruption discard the partial sweep. Expose only the previous full assignment.
Canonical community ID is the smallest member node ID in that assignment.
Return converged and sweeps; a capped result is incomplete, not a stable partition.
Open: Louvain/Leiden only after measured need for hierarchical communities.

### Centrality

Degree returns raw in_degree, out_degree and incident_degree over retained physical edges.
In reverses endpoints; Both keeps stored orientation for degree. Count parallel edges separately.
A loop adds one to in, one to out, one to incident.
The sum in_degree+out_degree therefore counts a loop twice. No normalized degree in v1.
Sampled betweenness uses unweighted Brandes on directed arcs, or symmetric arcs for Both.
For betweenness only, collapse parallel arcs and ignore self-loops.
Count all shortest paths, not only the canonical path. Exclude source and target endpoints.
Require a u64 sampling seed. Rank nodes by (SplitMix64(seed XOR node_id), node_id).
Select min(samples,V) sources. Discard interrupted sources. Define SplitMix64 version 1 as:
x=input+0x9E3779B97F4A7C15; x=(x XOR (x>>30))*0xBF58476D1CE4E5B9;
x=(x XOR (x>>27))*0x94D049BB133111EB; return x XOR (x>>31). Arithmetic wraps modulo 2^64.
For n>2, score(v)=(n/k)*sum_s dependency_s(v)/((n-1)*(n-2)).
Here k means completed sources; return k and the selected/completed IDs. No confidence interval.
No completed source means no scores. For n<=2, return zeros. Partial sampling is labelled estimated.
Completing the requested sample is complete, though estimated; only full sampling is exact.
Symmetric ordered pairs use the same formula; do not divide by two again.
Use finite f64 path counts and dependencies; overflow is NumericError, not saturation.
Source count, vertex work, edge work and deadline jointly bound this computation.

## Determinism and read consistency

Sort node IDs and edges by (relationship type bytes, edge ID); fix all reduction orders.
Use f64; no unordered hash iteration or fast-math. Parallel scheduling must preserve reduction order.
For the same view, scope, seed and completed steps, values are independent of thread count.
Deadline interruptions may finish different step counts; elapsed time is not deterministic.
Open: cross-platform bitwise equality; golden tests use stated tolerances until proven.
Hold one GraphRef across extraction and computation; never reacquire between steps.
Current graph handles capture the database sequence (`src\property_graph\mod.rs:574-582`);
execution forwards it into results (`src\property_graph\read.rs:775-780,801-808`).
Current borrow-based consistency is documented at `src\property_graph\read.rs:13-16`.
Capture one TTL read_time_unix_ms at entry. Apply the decided visibility rule at every lookup.
A due node hides its incident edges, even when an edge's own deadline is later.
Return that clock with view_sequence. No mid-run wall-clock refresh or implicit historical view.
Embedded callers choose their authorized graph; the algorithm itself grants no tenancy rights.
Future serving must require graph_read and qualify names before lookup, as current routes do
(`src\server\graph.rs:149-153,321-326`; `src\server\tenants.rs:462-467`).
Reuse graph binding checks, including tenant ownership (`src\server\credentials.rs:177-183`).
Do not follow attachments or return vector payloads. Graph permission grants no collection access.
No cross-graph seeds, cross-tenant projections or shared result cache.
Current cursors bind graph sequence and query (`src\property_graph\engine\cursor.rs:258-265`).
They do not pin a TTL clock. Do not reuse them for algorithms; v1 has no continuation cursor.

## Proposed Rust API and result

```rust
impl GraphRef<'_> {
    pub fn algorithm(
        &self,
        request: &GraphAlgorithmRequest,
        options: &GraphAlgorithmOptions<'_>,
    ) -> Result<GraphAlgorithmRead>;
}
```

Request holds GraphAlgorithmScope and a typed algorithm enum. Options hold GraphLimits, extra caps, projection and cancellation.
Use a new result type; do not silently change existing GraphRead or truncation variants.
It mirrors today's envelope (`src\property_graph\read.rs:313-326`) with these additions:

| Field | Contract |
|---|---|
| output | Typed path+cost, node scores, degree rows or node/community assignments; optional on exhaustion. |
| complete / truncated / reasons | complete iff reasons is empty; truncated is its inverse. Include current reasons plus IterationLimit, VertexLimit and MemoryLimit. |
| scope_complete / computation_complete | Separate extraction/computation from OutputLimit; local completeness never means global coverage. |
| view_sequence / read_time_unix_ms | One committed view and one TTL clock. |
| algorithm_version / scope | Normalized parameters, sorted seeds, direction, filters, weight policy and hop bound. |
| stats / quality | Extraction nodes/edges, total examinations, vertex work, scratch peak, elapsed time, iterations, residual, convergence or sample counts. |

Rows sort by node ID; path arrays remain in traversal order. Ranking is a caller operation.
Validate options before work; invalid inputs and numeric failures are typed errors, not truncations.
Rust first; no writes, persisted scores, CALL syntax or new dependencies.
REST, MCP and gRPC follow only through a separately approved OpenAPI change.
This design does not edit `src\openapi.json` or any proto.

## Implemented S1 and S2 (unreleased)

The kernel is `src\property_graph\engine\algorithm.rs` (std-only, also built by `vlite-graph`).
The public adapter is `src\property_graph\algorithms.rs`, re-exported from `vlite::property_graph`.
Names, error mapping and the decisions below are proposals pending public surface review.

```rust
impl GraphRef<'_> {
    pub fn algorithm(
        &self,
        request: &GraphAlgorithmRequest,
        options: &GraphAlgorithmOptions<'_>,
    ) -> std::result::Result<GraphAlgorithmRead, GraphAlgorithmError>;
}
pub struct GraphAlgorithmRequest { pub scope: GraphAlgorithmScope, pub algorithm: GraphAlgorithm }
pub struct GraphAlgorithmScope {
    pub seeds: Vec<NodeSelector>,
    pub direction: Direction,
    pub relationship_types: RelationshipScope, // Any | Only(Vec<String>)
    pub node_labels: LabelScope,               // Any | All(Vec<String>)
    pub edge_conditions: Vec<Condition>,
}
#[non_exhaustive]
pub enum GraphAlgorithm {
    Degree,
    WeightedShortestPath { source: NodeSelector, destination: NodeSelector, weight_property: Option<String> },
}
pub struct GraphAlgorithmOptions<'a> {
    pub limits: GraphLimits,
    pub algorithm_limits: GraphAlgorithmLimits, // #[non_exhaustive]: DEFAULT, then assign fields
    pub cancel: Option<&'a AtomicBool>,
}
#[non_exhaustive]
pub enum GraphAlgorithmError {
    Read(Error),
    MissingWeight { edge_id: u64 },
    InvalidWeight { edge_id: u64, reason: &'static str },
    NumericOverflow,
}
```

Outputs are `#[non_exhaustive]`: `GraphAlgorithmRead`, `GraphAlgorithmOutput::{Degree(Vec<DegreeRow>), WeightedShortestPath(WeightedPathOutcome)}`,
`WeightedPathOutcome::{Found(WeightedPath), NoPathWithinScopeAndHops}`, `GraphAlgorithmTruncation` and `GraphAlgorithmStats`.
`GraphAlgorithmRead` holds output, complete, truncation, scope_complete, computation_complete,
view_sequence, read_time_unix_ms, algorithm_version (1), request, max_hops and stats.
`request: Option<GraphAlgorithmRequest>` is the normalized request echo, `Some` exactly when computation_complete.
Only implemented variants exist; S3 and S4 add variants, caps and quality fields later.
Existing types, fields, `Error` variants, `GraphRead` and ordinary read limits are unchanged.

| Failure | Result |
|---|---|
| A read or algorithm limit above its ceiling; over 10,000 seed selectors or 256 `Only` entries | `Read(GraphLimitExceedsCeiling)` |
| A zero algorithm cap; no seed; an empty `Only` or `All` list; over 8 `All` entries (duplicates count) | `Read(InvalidQuery)` |
| An invalid type, label or key; a seed lacking a scope label | `Read(InvalidQuery)` |
| More distinct seeds than max_visited_nodes; a path source that is not a seed; a bad weight name | `Read(InvalidQuery)` |
| An invalid edge condition | `Read(InvalidFilter)` |
| A missing, due or other-graph seed or path endpoint | `Read(GraphNodeNotFound)` |
| A retained edge lacking the weight / holding a bad one (smallest edge ID) | `MissingWeight` / `InvalidWeight` |
| A winning cost above `f64::MAX` | `NumericOverflow` |

Before any work: read limits, algorithm caps, condition shape, then the request shape (list lengths, weight name).
Then, under the meter: condition conversion, types, labels, seeds (resolution and label check), endpoints.
An item error (invalid name or key, missing node, seed lacking a label) surfaces only if the meter reaches it;
a pre-set cancellation or an exhausted budget stops first, so the request is never normalized unmetered.
`Read` reuses the ordinary read mapping of `GraphError` to `Error`, so codes match other reads.
Work limits and cancellation are truncations (`Ok`, `complete == false`), never errors.

### Metering as implemented

One meter charges edge units (`max_edges_examined`), vertex operations and scratch bytes.
It starts after the shape checks and before any per-item work: it reads the monotonic deadline clock and checks cancellation, then the deadline.
Each charge first tests its budget, then checks cancellation and deadline when the combined count reaches a multiple of 64.
A final check runs after the public result is built; failing it returns no output and no request echo.
Preparation charges one vertex per condition, type, label, seed selector and path endpoint, plus sort steps.
Sort steps cost n times the bit length of n vertex operations.
Discovery is level-synchronous: one vertex per seed copied and per expanded node, one edge unit per adjacency entry.
Each level's new nodes are sorted (sort steps), checked against max_visited_nodes (VisitedLimit) and merged (one vertex per node).
`max_edges_per_node` is checked before each entry (DegreeLimit), across all types of a node.
A typed scope probes each listed type of each scanned node with one lazy range lookup charged one vertex, found or not,
so absent types are charged and the cancellation, deadline and vertex checks interrupt them.
The rescan charges one vertex per admitted node and one edge unit per entry again; Both retains an edge at its stored source.
Degree charges one vertex per node and one edge unit per retained edge.
Paths charge one edge unit per retained edge for weight validation, one per arc placed and one per relaxation.
They charge one vertex per node prefix, per state initialized, per heap push, pop and improvement, and per path node.
The adapter then converts the output and echoes the normalized request: one vertex per row, ID, seed, name and condition.
Scratch counts every buffer at its allocated capacity, reserved before allocation, including the adapter's.
A growing buffer doubles (at least 4 slots) into a new allocation reserved in full first.
Reservations accumulate and are never released, so old and simultaneously live buffers stay charged and the total bounds the peak.
Uncharged: the result envelope (flags, stats, at most one truncation reason with at most one node ID), error values and allocator headers.
`arcs` counts computational arcs: one per retained edge for Out and In, two per non-loop edge and one per loop for Both.

### Numeric rules as implemented

Weights convert exactly to (mantissa, exponent); negative zero becomes zero.
Path costs are exact fixed-point sums sized for the hop bound, compared exactly and rounded once to the nearest f64, ties to even.
The result is therefore independent of addition order and platform.
States are (node, hops) for hops 0..=H with an indexed heap of at most (H+1)*V entries.
The winner is the smallest (cost, hops, edge-ID sequence); a costlier prefix with fewer hops stays in its own state.
Example: a direct edge of 2^53+2 and a path of 2^53, 1 and 1 tie exactly, so the direct edge wins on hops.
`tests/graph_algorithm_oracle.py` independently sums the binary64 inputs with exact rational arithmetic.
Goldens cover the large-integer hop tie, binary64 versus decimal-rational weights,
and distinct exact costs that round to the same f64; only the winner is rounded.
A visible destination outside the projection gives NoPathWithinScopeAndHops after a complete run.

### Accepted S1/S2 decisions

1. `output: None` represents Exhausted; there is no separate Exhausted variant, and `complete` has no `truncated` twin.
2. The first truncation stops all work, so at most one reason is reported.
3. Degree OutputLimit returns the first max_output_nodes rows; a path above any output cap is withheld.
4. NoPathWithinScopeAndHops is returned even when max_output_paths is 0.
5. The doc's NumericError is `NumericOverflow`; exact sums make overflow possible only in the final rounding.
6. There is no projection option: nothing returns properties, so options hold limits, caps and cancellation only.
7. The rescan follows the requested direction; Both examines each adjacency entry from both endpoints.
8. A due or missing destination fails as GraphNodeNotFound; a visible one outside the scope gives no path.
9. `GraphAlgorithmLimits` is `#[non_exhaustive]` so S3/S4 caps can be added without a break.
10. Unpinned handles sample the TTL clock per call, as ordinary reads do; `with_clock` pins it.
11. `request` is `Option`, echoed only when computation_complete; a stop never forces a request clone.
12. `Only` lists hold at most 256 entries and `All` lists at most 8, counted before deduplication.
13. Item errors surface in metered order (types, labels, seeds, endpoints); cancellation or exhaustion first returns `Ok`.

### Coverage

The engine has 20 unit tests, run by both `vlite` and `vlite-graph`.
They check exact budgets at and below need, fake-clock deadlines in edge-free loops, a naive oracle and replay.
They also check split commits and reinsertion in descending ID order (new IDs must ascend).
Every engine result is checked to have reserved at least the capacity of the buffers it returns.
The adapter has 12 tests over the public API, including TTL equality, filters, weights, caps, typed errors and non-mutation.
Each suite was written first and failed (0 of 15, 0 of 10) before the implementation passed.
Review regressions also failed first: buffer growth (4,097 self-loops held 196,656 bytes against 98,416 reserved),
preparation under a pre-set cancellation, absent-type probes (1,800 vertex units for 25,600 probes) and adapter staging.
All 24 golden cases check S1 scope node and edge IDs exactly, plus degrees derived from the oracle's scope.
The 11 weighted_shortest_path cases check outcomes, costs and path IDs exactly.
Betweenness golden values are not exercised by Rust code yet; S3 coverage is listed below.

## Implemented S3 (unreleased, under review)

PageRank and label propagation run on the same extracted projection, TTL clock and meter as S1 and S2.
The additions below are additive and await independent review; nothing else in the surface changed.

```rust
// GraphAlgorithm (new variants)
PageRank {
    damping: Option<f64>,   // default 0.85
    tolerance: Option<f64>, // default 1e-8
    personalization: Option<Vec<(NodeSelector, f64)>>,
    weight_property: Option<String>,
},
LabelPropagation,

// GraphAlgorithmOutput (new variants)
PageRank(PageRankOutput),
LabelPropagation(LabelPropagationOutput),

#[non_exhaustive] pub struct PageRankOutput {
    pub scores: Vec<NodeScore>, pub iterations: u64, pub residual: f64, pub converged: bool,
}
#[non_exhaustive] pub struct NodeScore { pub node_id: u64, pub score: f64 }
#[non_exhaustive] pub struct LabelPropagationOutput {
    pub assignments: Vec<CommunityAssignment>, pub sweeps: u64, pub converged: bool,
}
#[non_exhaustive] pub struct CommunityAssignment { pub node_id: u64, pub community_id: u64 }

// GraphAlgorithmLimits (new field): max_iterations, default 20, ceiling 100, at least 1.
// GraphAlgorithmTruncation (new last variant): IterationLimit.
```

No `Error` or `GraphAlgorithmError` variant is added.
Bad damping, tolerance, masses, an empty personalization, a repeated node or a non-seed node is `Read(InvalidQuery)`.
More than 10,000 personalization entries is `Read(GraphLimitExceedsCeiling)` with limit `personalization`.
A missing or due personalization node is `Read(GraphNodeNotFound)`.
`MissingWeight` and `InvalidWeight` keep the S2 weight rules over every retained edge, in edge-ID order.
`NumericOverflow` now also covers a non-finite out-weight sum or personalization total; its text became generic.

### S3 semantics as implemented

PageRank arcs are those of S1/S2: one per edge for Out and In, two per non-loop edge for Both, one per loop.
Parallel arcs add their weights and self-loops count; zero-weight-only nodes are dangling.
Each node's out-weight sum is formed in arc order (edge-ID order; Both adds the reverse arc second).
The teleport vector is 1/n, or each mass divided by the total of the masses summed in ascending node order.
A step zeroes the next vector and sums dangling mass ascending, adds r(u)*(w/s(u)) for each arc in edge-ID order,
then sets (1-d)p+d(next+Dp) and accumulates the L1 change ascending, all in binary64.
Label propagation symmetrizes non-loop edges, sorts and deduplicates neighbor pairs, and ignores weights.
A sweep reads this sweep's label for smaller neighbors and the last sweep's for the others.
The community ID is the smallest node ID holding the label in the last full assignment.

### S3 metering and partial results

Setup charges one unit per edge and per node for weights, sums, finiteness, teleport and state vectors.
Label propagation charges pair collection and deduplication per pair, the sort, offsets and state.
Every buffer (weights, sums, teleport, ranks, next, rows; or pairs, offsets, labels, votes, rows) is reserved at capacity first.
The request echo is staged and the result rows are reserved and prepaid before the first step.
A PageRank step charges 2n vertex units and one edge unit per arc; a sweep n vertex units and one edge unit per neighbor entry.
Nothing allocates or is charged after the last full step except the final deadline and cancellation check.
A stop inside the first step returns `output: None` and `request: None`.
A later stop discards the interrupted step and returns the last full one with the echo, `complete` and `computation_complete` false.
A converged run whose final check fails keeps its output but lists Deadline or Cancelled.
`computation_complete` is true only for a converged run that passed the final check (output caps aside).

### Proposed S3 decisions for review

1. Iterative outputs are returned with their provenance (iterations or sweeps, residual, converged, echo); S1/S2 rules are unchanged.
2. Only the first reason is listed: IterationLimit (or a budget) is listed alone even when rows were also cut.
3. `iterations` and `sweeps` include the converging step or unchanged sweep; a cap that ends on a converged step is complete.
4. Personalization is echoed as node IDs ascending with the masses as given, not normalized; damping and tolerance are echoed as their effective values.
5. Score and community rows are cut to `max_output_nodes` after the computation, which still runs over every scoped node.
6. The output types are `#[non_exhaustive]` and re-exported; `GraphAlgorithmLimits` gains a field, as decision 9 above allows.

### S3 coverage

The engine adds 10 tests: closed forms, documented sweeps, caps, every vertex, edge and scratch budget,
fake-clock deadlines (including steps over 300 isolated nodes), cancellation, output caps, weights, validation order and the TTL clock.
Random scopes (TTL, types, labels, conditions, directions) match a naive PageRank bitwise and a naive label propagation exactly, converged or capped.
The adapter adds 7 tests: the 6 stationary PageRank goldens within 1e-8 (tolerance 1e-10, mass 1 within 1e-12)
and the 3 label propagation goldens exactly, every sweep included, on exact IDs and through a database file.
All 12 cases of `iterative-cases.json` (checked by `tests/graph_iteration_oracle.py`) match every one of their 17 full steps or sweeps within 1e-14.
Their synthetic interruptions map to the vertex budget that stops the same update; they are not engine budgets.
The adapter also covers the echo, partial-result provenance, typed errors, the shared budget and non-mutation.
Both suites were written first and failed to compile (61 errors, all missing S3 API) before the implementation passed.
Remaining gates: independent review of this surface, the API-reference row, S4 betweenness, CSR parity and serving.

## GraphRAG and competitors

Community assignments can group already stored evidence for application-produced summaries.
Carry graph, view sequence, TTL clock, scope, members and completeness into summary provenance.
Applications must authorize evidence collections separately and avoid calling local groups global.
Do not implement extraction, ingestion, LLM calls, summary storage or automatic refresh here.

The Neo4j primary documentation was checked 2026-10-30. Unverified rows are
future comparison leads, not claims or implementation dependencies.

| Product | Capability status | What to investigate and copy if verified |
|---|---|---|
| Neo4j GDS | [PageRank](https://neo4j.com/docs/graph-data-science/current/algorithms/page-rank/) documents named graphs, weights, iteration/tolerance parameters and convergence statistics | Adopt explicit scope and diagnostics; retain VLite's tighter shared work budgets and documented L1 stopping rule. |
| Memgraph MAGE | unverified | Small typed algorithm procedures; separation from graph mutation. |
| Kuzu algo extension | unverified | Optional analytics surface over an embedded graph. |
| TigerGraph | unverified | Parameterized PageRank and sampled centrality; explicit execution controls. |

No numerical parity with another implementation is implied; normalization and
stopping rules differ. VLite's golden oracle below is authoritative.

## Test oracle and delivery

| Future check | Required oracle |
|---|---|
| Weighted diamond | Costs 1+1 versus 3; return cost 2. Equal costs follow hops then edge IDs. Test a cheaper prefix that exhausts H. |
| PageRank golden | Reciprocal two-node graph: (0.5,0.5). A->B with dangling B: (1/(2+d),(1+d)/(2+d)). Check L1 residual and mass. |
| Personalised PageRank | Two isolates redistribute to p exactly; test zero-weight arcs and invalid masses. |
| Community golden | Two disjoint pairs plus isolate; canonical community IDs are each group's minimum node ID. Freeze sweep assignments. |
| Centrality golden | Directed A->B->C: B has normalized betweenness 0.5 at full sampling; endpoints 0. Freeze sampled IDs for seeds. |
| Property tests | Dijkstra equals exhaustive H-bounded walks on tiny nonnegative graphs; PageRank stays nonnegative and sums to 1 within 1e-12. |
| Projection/determinism | Reverse insertion order while preserving IDs; vary thread count and adjacency adapter. Compare values, scope and reasons. |
| Limits/deadlines | At and above every ceiling; exact edge/vertex budgets; empty output caps; huge degree; cancellation and fake-clock stops between vertices with no edges. |
| Consistency/errors | TTL equality hides endpoints/edges; one sequence under attempted writes; denied tenant/graph seeds; bad types, missing/negative/NaN weights and overflow. |

Run golden PageRank with tolerance 1e-10 and 100 iterations; compare scores within 1e-8.
IDs, counts and path costs for integer fixtures must match exactly.
[`graph_algorithm_oracle.py`](../../../tests/graph_algorithm_oracle.py) checks
[`golden.json`](../../../tests/fixtures/graph_algorithms/golden.json) using tiny
exhaustive paths, exact-rational stationary PageRank and full-sample centrality.
CI also verifies rejection of corrupted answers and duplicate JSON keys.
Stationary PageRank scores are not fabricated iteration counts; future Rust
tests must independently establish convergence and compare within tolerance.
Rust tests consume them for S1 to S3 (see Coverage); TTL and work limits have their own Rust tests.
Authorization stays untested until a serving slice exists.
Delivered S1: scoped adapter, visibility hook, unified meter and result contract with exhaustion oracles.
Delivered S2: degree and hop-constrained Dijkstra. Delivered S3 (under review): PageRank and deterministic label propagation.
Deliver S4: seeded sampled betweenness. Deliver S5: separately approved serving contracts and documentation.
Each slice must pass its oracle before the next. CSR must pass identical adapter fixtures later.
Risks: rescans consume budgets; dense graphs may yield no values. Cancellation is cooperative.
Risks: the read borrow may delay writes; boundary scores are local, not global estimates.
The added caps are the initial implementation limits, not larger graph budgets.
Open: offline/admin execution,
cross-platform reproducibility of S3/S4 floats and the remaining competitor evidence.
