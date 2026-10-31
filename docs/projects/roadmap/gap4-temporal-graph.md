# Gap 4: time-aware graph

Status: implementation design. No implementation or wire-gate approval is claimed.
Scope: validity time for facts, not transaction-time history or bitemporal storage.
Semantic rules below are the chosen direction; explicit Open items are prerequisites
for their implementation slices. Audit citations refer to base `d530b4c`;
TTL integration below was reconciled with `cf146fe`.

## Constraints and evidence

Gap 4 asks for edge validity and as-of reads, starting with validity only.
Source: `docs/projects/product-gaps/product-gaps.md:49`.
Keep mixed writes atomic and forbid dangling edges. Keep security and feature gates.
Keep OpenAPI frozen. Benchmarks remain major-release-only.
Source: `docs/projects/product-gaps/product-gaps.md:90-97`.
Rust goes first; format changes must follow P2 and its converter.
Source: `docs/projects/roadmap/next-major.md:78-87`.
The original identity decision is recorded at `docs/projects/feature-agent-graph/decisions.md:225-231`.

| Current fact | Code evidence |
|---|---|
| An edge key maps to one ID within its graph, not to an endpoint triple | `src/property_graph/engine/state.rs:30-43` |
| Edges store endpoints, type, properties and a revision, but no validity | `src/property_graph/engine/model.rs:87-98` |
| Keyed create retries only an identical edge; keyless creates can be parallel | `src/property_graph/mod.rs:207-219` |
| Reads share a borrowed committed state and report its sequence | `src/property_graph/mod.rs:113-123`; `src/property_graph/read.rs:759-781` |
| Writes validate, append one frame, then apply graph and vector state | `src/property_graph/mod.rs:598-615` |
| Failed state application rolls back earlier entries | `src/property_graph/engine/state.rs:604-628` |
| Limits are 4,096 user operations and 16,777,216 encoded body bytes | `src/property_graph/engine/limits.rs:30-34`; [TTL framing](../feature-agent-graph/decisions.md#gap-3-graph-frame-rule-2026-10-30) |
| Node uniqueness uses property postings; relationship properties cannot be unique or indexed | `src/property_graph/engine/schema.rs:885-896`; `src/property_graph/engine/schema.rs:278-281` |
| UniqueTriple forbids a second stored edge for the same directed triple | `src/property_graph/engine/schema.rs:922-944` |
| Compaction and directory snapshots share graph-body generation | `src/property_graph/frame.rs:155-169` |

Current: [graph TTL](../../GRAPH.md#expiry-ttl) is implemented.
It adds optional `expires_at_ms` to nodes and edges.
A due node hides itself and all incident edges. A due edge hides only itself.
Reads use now or `GraphRef::with_clock(ms)` for expiry visibility, not time travel.
Every write removes due entities in its atomic frame.
Compaction and checkpoints drop them. Older builds reject the new ops as `Corrupt`.
Reclaims are internal entries, not user operations; the frame prefix records their
count. Temporal work must preserve that rule, including same-batch reclamation.
## 1. Model and identity

Choose temporal edges, not temporal nodes, for the first release.
Durable node anchors avoid a second validity axis and temporal cascades; their properties stay nonhistorical.
A historical node view therefore means today's retained anchor, not its past contents.
Model changing node attributes as fact edges when their history matters.
A temporal edge is one logical relationship with one ID and optional graph-wide key.
Its source, destination and relationship type stay fixed.
It owns an ordered list of segments: `{ valid_from, valid_to, properties }`.
Properties belong to each segment, so an earlier fact does not inherit today's value.
Use signed i64 milliseconds since Unix epoch in UTC; permit pre-epoch values.
`None` means negative infinity for `valid_from`, positive infinity for `valid_to`.
Use half-open intervals `[from,to)`; require `from < to` for two finite ends.
Reject zero length, inverted bounds and out-of-range timestamps.
Missing both ends means always valid; do not use integer sentinel values.
One relationship can have several non-overlapping intervals under the same ID/key.
For example, edge `employment-7` can hold `[100,200)` and `[300,None)`.
Different destinations require different logical edges.
Within one series, reject any overlap, even when properties match.
Allow touching intervals and gaps. Do not silently merge adjacent segments.
Propose a hard cap of 256 segments per temporal edge; reject excess explicitly.
This bounds binary search, correction validation and per-edge memory growth.
Keep legacy edges as a separate, unchanged form with implicit all-time validity.
Explicit promotion creates an all-time segment from their current properties.
Promotion is conditional and enables the temporal format capability.
Do not silently reinterpret existing arbitrary properties named `valid_from` or `valid_to`.
Expose validity as typed fields, outside ordinary metadata and projection.
Temporal edge views carry the matching segment's bounds and properties.
The edge ID and revision identify the whole series; there is no separate segment ID.
## 2. Reads and the two time selectors

Propose `graph.as_of(T)` as a Rust read-view modifier, not a database snapshot.
Keep `with_clock(C)` independent: C controls only TTL visibility.
Capture C once per operation; absent T, use T = C after checking it fits i64.
Capture real epoch time for C unless overridden; never use the deadline timer as epoch time.
The current native deadline clock measures elapsed `Instant` time.
Evidence: `src/property_graph/read.rs:746-749`.
Keep deadlines and cancellation on that separate elapsed-time clock.
For retained temporal edge e, return its unique segment containing T only if:
`e.expires_at_ms` is absent or C is before it, and both endpoints pass node TTL at C.
Legacy edges need only the same TTL and endpoint checks.
An edge whose endpoints are hidden is neither returned nor traversed.
Closing validity does not remove its endpoints or change attachments.

| Read | Proposed as-of behavior |
|---|---|
| Get edge by ID/key | Matching segment or `None`; never fall back to the latest segment |
| Get node by ID/key | Retained node if visible at C; T does not reconstruct node metadata |
| Find nodes | Same anchor semantics; no temporal node predicate |
| Neighbors | Only edges valid at T with visible endpoints at C |
| Traverse | Filter before discovery and expansion; every path edge must be valid at the same T |
| Shortest path | Run existing unweighted search over that filtered graph |
| Retrieve | Current pivot search and evidence; expansion uses the same fixed T and C |
| Explain/query text | Explain the selectors; inherited read view, no new text grammar initially |

Current get and traversal methods enter the shared executor.
Evidence: `src/property_graph/read.rs:879-985`.
Query text delegates to the typed query: `src/property_graph/read.rs:857-866`.
Current retrieve searches, revalidates live records and maps attachments before expansion.
Evidence: `src/property_graph/retrieve.rs:370-438`.
Do not promise historical embeddings, ranking, payloads, attachments or full recall at T.
Return effective `as_of_ms`, `ttl_clock_ms` and `view_sequence` with temporal results.
`view_sequence` remains a commit identifier, never a validity timestamp.
Bind T and C into paged cursor identity, including resolved defaults.
Continuation reuses those captured selectors; a conflicting override fails explicitly.
Keep graph-sequence stale detection. Do not resample now between pages.
Current cursors check fingerprint then graph sequence.
Evidence: `src/property_graph/engine/cursor.rs:261-267`.
Charge every examined adjacency entry, even when its segment is invalid at T.
Use binary search within the bounded segment list; preserve output order and cancellation.
Current traversal charges before edge filtering: `src/property_graph/engine/traverse.rs:135-152`.
Preserve certified shortest-path outcomes; budget exhaustion is not proof of no path.
Evidence: `src/property_graph/engine/traverse.rs:243-246`.
## 3. Corrections, revisions and schema

Provide explicit create, append-segment, replace-segment and close-segment mutations.
Address a segment by its exact old bounds plus the series revision.
Require `Expected::Revision` for appends, corrections, closure, promotion and removal.
Check the whole retained series, not just the segment visible at now or T.
`Expected::Absent` refers to ID/key absence, not absence at a chosen validity time.
An expired entity removed by TTL is absent; a closed but retained edge is not.
Current preconditions distinguish Any, Absent and Revision.
Evidence: `src/property_graph/engine/model.rs:56-65`.
Advance the whole edge revision to the commit sequence on any real temporal mutation.
Current edge patches do that: `src/property_graph/engine/state.rs:833-846`.
Retries with a stale revision fail; never silently overwrite a concurrent correction.
Close `[a,b)` at c only if `a < c < b`, treating open ends as infinities.
Closing at the existing finite end is an exact no-op after checking the revision.
Reject other bounds. To retract an entire segment, use an explicit removal operation.
Reject overlap; never auto-close another segment or infer a contradictory fact.
Supersede in one batch: close the old fact at c, then create/append the replacement at c.
For a changed destination this touches two series. Require both relevant expected checks.
No automatic semantic conflict detection across different types, keys or destinations.
Gap 1 must define its fact-slot identity and exclusivity rules.
Ordinary edge patches on temporal edges fail with an explicit temporal-operation error.
Correction replaces the selected segment's properties; past answers may change.
This is validity history as currently believed, not an audit of past beliefs.
Keep node unique properties global over retained nodes, regardless of T.
Keep UniqueTriple as one logical series per triple, not one segment per triple.
Parallel temporal series remain allowed where the existing schema permits parallel edges.
Validate each segment's properties against relationship schema rules.
Do not add temporal unique properties or weaken existing node uniqueness.
Zero remaining segments means an empty retained series until explicit deletion or TTL.
It keeps its identity and revision but is absent from ordinary as-of reads.
Add bounded Rust timeline inspection for corrections; it ignores validity, never TTL.
## 4. TTL and the limits of history

Validity closure retains history. TTL is destructive retention.
At C equal to a deadline, hide the entity even if T predates that deadline.
Node TTL hides all of its history-bearing incident edges.
Edge TTL deletes the entire series, not just its currently active segment.
Before physical cleanup, `with_clock` can change expiry filtering of retained data only.
After cleanup, no clock override or as-of timestamp can resurrect removed data.
As-of cannot recover deleted nodes, deleted edges or overwritten corrections.
Compaction/checkpoints must retain closed and future segments unless TTL has removed the series.
Do not discard history merely because `valid_to <= now`.
Example: `[100,200)` with expiry 500 is readable at T=150,C=499, not at T=150,C=500.
After a write purges it at 500, T=150,C=499 cannot recover it either.
Keep vector evidence retention separate; historical edges do not guarantee surviving evidence.
## 5. Storage, bounds and change feed

Add dedicated temporal entries behind a persisted, required format capability.
Do not change the meaning or payload of legacy edge ops in place.
Old files remain readable; enable the capability atomically with the first temporal write.
Readers without the capability must refuse before publishing partial state.
Open: allocate a separate temporal capability and precise unsupported-format error with the format owner.
The [compact-storage flag](compact-storage-contract.md) guards checkpoint blocks,
not new temporal WAL entries; do not reuse it for temporal semantics.
Do not reuse the TTL-only marker or assume its old-build `Corrupt` rule is negotiation.
Current replay maps invalid graph bodies/state to `Corrupt`.
Evidence: `src/property_graph/frame.rs:47-48`; `src/property_graph/frame.rs:78-80`.
Plan temporal changes, TTL cleanup and vector mutations together in one atomic frame.
Keep the 4,096-user-operation cap. Temporal correction operations count toward it;
computed TTL reclaims retain the accepted separate prefix/body accounting.
Check encoded bytes too; never split an atomic supersession into several commits.
One bounded segment replacement is one entry, but its entire payload counts in the byte limit.
Overflow fails before append and rolls back planning, including tentative TTL cleanup.
Internal entries still count toward the encoded byte limit and must pass shared frame validation.
Restore segment lists, TTL, revisions and high-water IDs in reopen and snapshot paths.
Current compaction emits nodes, edges, attachments and high-water entries.
Evidence: `src/property_graph/frame.rs:182-218`.
Open: audit revision preservation in every temporal rewrite; do not infer it from that encoder.
Keep key maps and adjacency per logical edge; no adjacency entry per segment.
Add ordered segment storage; no global temporal index in the first slice.
Point lookup uses binary search; high-degree historical series still consume read budgets.
Storage grows with segments and their properties, not just logical edges.
Two finite bounds need 16 payload bytes before tags, properties and allocation overhead.
Open: actual retained bytes per segment and suitable production caps need measurement.
Do not claim latency or memory gains. Leave benchmark gates and timing unchanged.
Add Rust feed operations with full segment bounds/properties and explicit close/remove changes.
Feed time is commit order, not validity order; do not schedule events at validity boundaries.
Current feed operations include edge upsert, patch and delete.
Evidence: `src/property_graph/change.rs:28-50`.
Temporal feed extensions must describe corrections without guessing old values.
Today's feed omits TTL deadlines/reclaims; it is not a full replication stream.
Do not promise TTL reconstruction until a separately versioned feed extension
adds those events and every consuming surface is updated.
REST/gRPC/MCP and their feeds wait for an approved OpenAPI diff and wire review.
Until then, serving a temporal-capability file must fail explicitly on unsupported surfaces.
Never silently omit temporal feed operations or send ordinary current properties instead.
No changes to OpenAPI, proto, CLI, bindings or query grammar in this design slice.
## 6. Minimal Gap 1 contract

| Memory action | Required temporal primitive |
|---|---|
| Remember | Create a keyed fact series/segment; atomically attach evidence through existing batch operations |
| Recall | Read at explicit T or captured now; report T, C, sequence, completeness and evidence limitations |
| Correct/supersede | Revision-checked close plus replacement in one batch; no inferred exclusivity |
| Forget fact history | Physically delete its series, not merely close its current interval |
| Forget an entity | Explicit bounded edge deletion and node removal, or configured TTL cascade |

Memory owns sessions, fact-slot naming, contradiction policy and extraction.
Closing a fact is not privacy erasure. Feed consumers and backups need their own erasure policy.
Open: Gap 1's cross-record erasure and oversized-entity workflow are not specified here.
## 7. Competitor comparison

Primary documentation checked 2026-10-30. These precedents inform semantics,
not performance claims or guarantees that VLite implements the same model.

| Product | Evidence | Adopt | Do not copy into this gap |
|---|---|---|---|
| Graphiti | [Official README](https://github.com/getzep/graphiti#what-is-a-context-graph) describes validity windows, provenance and invalidation without deletion | Explicit fact validity and retained history | Automatic contradiction inference or its bitemporal scope |
| Neo4j temporal types | [Temporal values](https://neo4j.com/docs/cypher-manual/current/values-and-types/temporal/) describes typed temporal properties | Typed timestamps and explicit boundary rules | Infer historical graph semantics from scalar types alone |
| Datomic as-of | [Historic database values](https://docs.datomic.com/peer-tutorial/see-historic-data.html) use transaction/time bases | A clear read-view selector and explicit history limits | Treat validity T as a transaction snapshot or promise immutable correction history |

VLite deliberately separates validity closure from destructive TTL retention.
## 8. Acceptance matrix
| Case | Required result |
|---|---|
| Bounds | Test from-1, from, to-1, to, open ends, negative epoch, i64 extremes and invalid bounds |
| Multiple intervals | Gaps return none; adjacent intervals select exactly one; overlap rejects without mutation |
| Identity/schema | Same key keeps ID; changed endpoints reject; UniqueTriple counts series; node uniqueness stays global |
| Corrections | Stale expected fails; close-and-replace is atomic; past answers reflect explicit corrections |
| Rollback | Failure in the final op leaves graph, vectors, TTL cleanup, indexes and revisions unchanged |
| Reads | Get, neighbors, traverse, shortest path, retrieve and query text agree on T/C; no hidden endpoint leaks |
| Paging/budgets | Fixed T/C across pages; stale graph fails; invalid history consumes budget; exhaustion stays certified |
| Reopen | Every temporal op and mixed frame round-trips; unknown capability/truncated body refuses cleanly |
| Compaction | Closed/future intervals survive; due TTL series vanish; IDs and revisions remain valid |
| Directory checkpoint | Snapshot plus later WAL equals uncompacted state, including corrections and capability |
| Encryption | Encrypted reopen, checkpoint, compaction and feed retain the same intervals; wrong key fails |
| TTL | Test C before/equal/after expiry independently of T; node cascade, edge-only expiry and post-purge non-resurrection |
| Limits | Accept 256 segments and 4,096 user ops at byte bounds, including legal internal reclaims; reject 257, 4,097 user ops and byte overflow atomically |
| Feed/surfaces | Commit-ordered correction payloads; no validity-boundary event; unsupported serving refuses explicitly |

The independent [interval oracle](../../../tests/temporal_interval_oracle.py)
checks [hand-authored fixtures](../../../tests/fixtures/temporal_intervals/golden.json)
for half-open bounds, gaps, touching segments, per-segment properties, the
256-segment cap and TTL visibility at an independent clock. It includes
retention flags to model why purged data cannot reappear. Run
`python tests/temporal_interval_oracle.py --self-test` for the reference cases
and malformed-fixture checks; CI runs the same command. These are mathematical
references, not engine coverage for mutations, revisions, storage, traversal,
cursors, authorization or work budgets.

## 9. Delivery and remaining decisions
| Slice | Exit |
|---|---|
| A: contract and format | Implement chosen identity, 256-segment cap and time type; allocate temporal marker with storage owner and document converter behavior |
| B: Rust writes/storage | Atomic temporal mutations, preconditions, schema checks, feed entries and maintenance round-trips |
| C: Rust reads | Shared visibility predicate, all reads, timeline inspection, cursors and acceptance matrix |
| D: Gap 1 integration | Remember/recall/forget use the primitives without inventing transaction history |
| E: serving | Approved OpenAPI diff, proto/wire review, authorization parity and explicit surface support |

Main risks are unbounded history growth, misleading historical evidence and correction races.
Open: capability allocation, exact Rust names/errors, versioned feed changes and real memory cost.
Open: privacy erasure across retained logs/backups and future temporal-node demand.
Engine implementation, engine acceptance tests, benchmark campaigns and API
approval remain outstanding; the reference fixtures do not satisfy those gates.
