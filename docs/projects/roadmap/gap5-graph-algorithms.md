# Gap 5: bounded graph algorithms

Status: implementation design; public/wire signatures still need surface review.
Audit citations refer to base `d530b4c`; TTL integration is reconciled with `cf146fe`.
Open means not verified or not decided. No implementation or performance gains are claimed.

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
This task does not edit `src\openapi.json`, any proto, code or tests.

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
Deliver S1: scoped adapter, visibility hook, unified meter and result contract with exhaustion oracles.
Deliver S2: degree and hop-constrained Dijkstra. Deliver S3: PageRank and deterministic label propagation.
Deliver S4: seeded sampled betweenness. Deliver S5: separately approved serving contracts and documentation.
Each slice must pass its oracle before the next. CSR must pass identical adapter fixtures later.
Risks: rescans consume budgets; dense graphs may yield no values. Cancellation is cooperative.
Risks: the read borrow may delay writes; boundary scores are local, not global estimates.
The added caps are the initial implementation limits, not larger graph budgets.
Open: public type names/error mapping, offline/admin execution, cross-platform
reproducibility and the remaining competitor evidence.
