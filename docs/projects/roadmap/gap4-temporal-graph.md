# Gap 4: time-aware graph

Status: private/gated slices A, B and C1/C2 reads are implemented; A/B and C1
are independently reviewed and integrated, but all remain unreleased. Slice C
public activation, serving approval and production qualification remain
outstanding; release builds continue to reject flag 64.
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

| Design-baseline fact | Code evidence |
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
Promotion is conditional and needs a file with the temporal format capability (§5).
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
Old files remain readable. Enable the capability by an explicit copy-out into a new
temporal-enabled database with a new lineage; the source is never modified and its replicas
are reseeded from the copy. A temporal write needs a file that already has the capability and
is then an ordinary atomic append; no write enables it in place.
(Amended by user decision during the slice A/B review, replacing "enable the capability
atomically with the first temporal write". The decision covers this design and a gated,
private implementation only: the conversion API stays gated until reviewed and qualified,
and production activation is not approved. See
[Activation publication (decided: copy-out)](#activation-publication-decided-copy-out).)
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

The independently reviewed [temporal read reference](../../../tests/temporal_read_oracle.py)
and [read fixtures](../../../tests/fixtures/temporal_intervals/read-cases.json)
model neighbors, FIFO traversal and certified shortest paths at fixed T/C.
They charge examined adjacency entries before visibility filtering and treat
truncated shortest-path searches as exhausted, not certified results. CI runs
`python tests/temporal_read_oracle.py --self-test`, including malformed inputs,
shuffle trials and mutations. This is logical acceptance evidence, not a Rust
implementation or proof of persistence, paging, property-filter or cancellation
behavior. It does not complete slice C or open any activation gate.

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

## Implemented slices A and B (unreleased, under review)

Status: implemented in Rust behind a preactivation gate; not released and not
accepted. The first independent review returned five blockers. Round 1 fixed
blockers 1, 3, 4 and 5. Blocker 2 (activation publication) is resolved by
the user-approved copy-out design, implemented as a gated, private path in
round 2, which also fixes two medium findings from the static re-review (A,
B); see [Review fixes, round 2](#review-fixes-round-2). Round 3 applies the
same frame checks to non-precision log export; see
[Review fixes, round 3](#review-fixes-round-3). Round 4 extends them to
compact images without precision and audits every graph mode; see
[Review fixes, round 4](#review-fixes-round-4). Everything waits for
independent execution review. Slice C (reads) is not started, so no user can
observe validity yet and every release build refuses the capability.

### Slice A: contract and format

| Allocation | Value | Where |
|---|---|---|
| Header capability | `FLAG_TEMPORAL = 64`, required, v5 only, valid only with `FLAG_GRAPH` (else `Corrupt` at offset 0) | `src/format.rs` |
| Unsupported builds | `Error::UnsupportedFeature(64)` from the header check, before any frame is read | `src/format.rs`, `src/property_graph/temporal.rs` |
| Graph entry tags | 65 put temporal edge, 66 append segment, 67 replace segment, 68 close segment, 69 remove segment | `src/property_graph/engine/codec.rs` |
| Engine model | ordered segments, 256-segment cap, binary-search lookup | `src/property_graph/engine/temporal.rs` |

Flags 1, 2, 4, 8 (legacy), 16 (compact image, reserved) and 32 (precision)
were in use; 64 is the first free bit. Tags 1..=15 and 64 (vector mutation)
were in use; 65..=69 follow the maximum. Legacy edge ops (7, 8, 9, 14) keep
their meaning and payload, and no temporal value ever travels in them.

Wire format (all integers little-endian; every payload starts with `edge_id u64`):

| Tag | Payload after `edge_id` |
|---|---|
| 65 | `revision u64`, `source u64`, `destination u64`, `type_len u8` + type, `key_len u16` + key (0 = keyless), `count u16`, then `count` segments |
| 66 | one segment |
| 67 | the old segment's bounds, then the replacement segment |
| 68 | the old segment's bounds, then `at i64` |
| 69 | the old segment's bounds |

Bounds are a flags byte (bit 0 `valid_from` present, bit 1 `valid_to`
present; other bits are corrupt) followed by the present `i64` values. A
segment is bounds plus `properties_len u32` and the ordinary property encoding.
A count above 256 is rejected before allocation. Decoding validates the series
(bounds, order, no overlap) and every property map, so a valid frame cannot
carry an invalid series. Each temporal entry is one user operation of the
4,096 cap, and its whole payload counts in the 16,777,216-byte body limit.

The tag-65 `revision` is zero in every commit body (op 12), where the edge
takes the commit sequence. Only a snapshot (op 15) restates a nonzero
revision, in `1..=` the snapshot sequence. Replay rejects a nonzero value as
`Corrupt`; snapshot load rejects a value outside that range as non-canonical.

Validation order matches the independent oracle: segment cap first, then per
segment its properties, bounds (`from < to` when both are finite), order
(starts ascend; an open start only first) and overlap (touching is allowed).
Errors use the internal `GraphError::InvalidTemporal(reason)`; the reason
starts with the oracle category (`bounds`, `order`, `overlap`,
`segment_limit`, then slice B's `no_segment`, `close`, `not_temporal`,
`temporal_edge`, `series_bytes`). The public API maps it to the existing
`Error::InvalidQuery(reason)`; no public `Error` variant is added.

Coverage added in slice A:

- `engine::temporal` unit tests: bounds at from-1/from/to-1/to, open ends,
  i64 extremes, negative epoch, gaps, touching segments, overlap, order, the
  256/257 cap, insert/replace/remove/close leaving the series unchanged on error.
- Codec: golden bytes for tags 65..=69, strict payloads (bad bounds flags,
  trailing bytes, truncation, oversized counts, invalid series), round trips
  and unknown tags still refused.
- `property_graph::temporal` adapter tests consume
  `tests/fixtures/temporal_intervals/golden.json` (every valid series, every
  invalid series by category, both limit cases and TTL-clock queries) against
  the engine model, and check that flag 64 is refused with
  `UnsupportedFeature(64)` while support is off, opens when on, and is
  `Corrupt` without `FLAG_GRAPH`.

### Slice B: Rust writes and storage

Public Rust API, unreleased. Every build refuses it with
`UnsupportedFeature(64)` until slice C ships.

| Item | Meaning |
|---|---|
| `GraphOp::Temporal(TemporalOp)` | A temporal operation inside an ordinary graph batch |
| `TemporalOp` (`#[non_exhaustive]`) | `CreateEdge`, `PromoteEdge`, `AppendSegment`, `ReplaceSegment`, `CloseSegment`, `RemoveSegment` |
| `TemporalSegment` | `valid_from` and `valid_to` as `Option<i64>` milliseconds, plus `properties: Metadata` |
| `GraphChange::Temporal(TemporalChange)` | A feed operation. `TemporalChange` (`#[non_exhaustive]`) has `EdgePut`, `SegmentAppended`, `SegmentReplaced`, `SegmentClosed` and `SegmentRemoved` |
| `Database::temporal_changes_after` | The opt-in temporal change feed |

Writes:

- **Atomic batches.** Temporal operations are planned with the other operations of a batch. Vector mutations, computed TTL cleanup and temporal entries go into one frame.
- **Limits.** Each temporal operation counts as one of the 4,096 user operations. The encoded 16 MiB body limit is checked before the append. Every series change except a removal (create, promote, append, replace and close) also checks that the resulting series fits one graph body (`series_bytes`), because a close can grow a series by 8 bytes. Any failure rolls back the whole plan: tentative TTL cleanup and the vector mutations.
- **Creating an edge.** `CreateEdge` needs live endpoints.
  - If the key is already live on an identical temporal edge (same endpoints and type, bit-identical series), the edge is returned with `created = false`. Any other live edge with that key is `GraphKeyConflict`.
  - Ordinary `CreateEdge`/`EnsureEdge` never treat a temporal edge as an identical retry.
- **Promotion.** `PromoteEdge` turns an ordinary edge into a temporal one. It keeps the ID, key and endpoints, and the edge's properties become one all-time segment.
- **Corrections.** Promotion and every correction need `Expected::Revision`; anything else is `InvalidQuery("expected: …")`.
  - The check uses the revision the edge had when the batch started. The planner records it before the batch's first mutation of the edge, ordinary (`PutEdge`, `PatchEdge`, expiry) or temporal.
  - TTL deletion still wins: an edge deleted earlier in the batch is not found.
- **No-op close.** Closing a segment at its own finite end plans nothing. No frame is written and the revision does not change.
- **Revisions.** Any real temporal entry sets the edge revision to the commit sequence.
- **Other operations on temporal edges.**
  - `PatchEdge` is `InvalidQuery("temporal_edge: …")`.
  - `DeleteEdge` deletes the whole series.
  - Node TTL hides and reclaims every incident edge, temporal ones included (Gap 3 cascade). Edge TTL reclaims the whole series.
  - TTL-deleted data is never restored: a reused key gets a new edge ID with only its own series.
- **Schema.** Relationship property rules apply to every segment. A new or promoted series is checked like a new edge (endpoint labels; `UniqueTriple` counts one logical series). The series must fit one graph body (`series_bytes`).
- **Storage.** Segments are held per edge ID in `Graph::segments`, ordered, with binary-search lookup. Key maps and adjacency keep one entry per logical edge. A temporal edge's own `properties` are empty.

Capability:

- **Enabling (copy-out).** `property_graph::temporal::enable_temporal(source, destination, key)` (crate-private, behind the gate) copies a closed graph-enabled file or directory store into a new single file with `FLAG_TEMPORAL`. See [Activation publication (decided: copy-out)](#activation-publication-decided-copy-out).
- **Refusal.** In a file without flag 64, or a build without support, a batch with any temporal operation returns `UnsupportedFeature(64)` before planning. This holds for valid, invalid and mixed batches, and nothing is written. No write enables the capability in place. In a capable file the first temporal write is an ordinary append that leaves the header unchanged.
- **Directory stores.** They inherit the capability through `convert_to_directory` of a capable file. WAL segment headers (`WAL_FLAGS`) and checkpoint images carry the flag. `enable_temporal` also accepts a directory store as its source.
- **Read-only handles** return `ReadOnly`.

Persistence:

- **Replay.** Replay applies tags 65..=69. A temporal entry in a file without flag 64 is `Corrupt`, both in replay and in every feed.
- **Snapshots.** Compaction, directory checkpoints and P2b precision images restate a temporal edge as one tag-65 entry, at the ordinary edge's canonical position and followed by its expiry entry.
  - The snapshot admission rule (`frame.rs` `restated`) and the image-check edge coverage (`compact/mod.rs`) admit it.
  - Snapshot load restores the series.
- **Flags across maintenance.** Compaction and upgrade carry `FLAG_TEMPORAL` with `FLAG_GRAPH`. ID high-water marks survive.
- **Revisions.** A snapshot at the same lineage restates each temporal edge's revision in its tag-65 entry: directory checkpoints, directory conversion and P2b precision images. This does not depend on precision; precision images also restore the same values through their revision frames.
  - Compaction and upgrade start a new lineage whose sequences restart. Under that existing new-lineage contract they renumber temporal revisions, as they do records and ordinary edges, because an old revision could exceed the new sequence. Nothing is refused. A same-lineage checkpoint never renumbers a temporal revision.
  - `enable_graph` refuses a graph-enabled source, so it never meets temporal state. `strip_graph` removes all graph state, temporal state included, and drops flag 64.
  - Copy-out activation copies every frame, so it keeps every revision.
  - A tag-65 revision is checked in the frame context that replay, both feeds, log export and the precision image check share (`frame::decode_frame`): zero in op 12, `1..=` the snapshot sequence in op 15.
  - Readers that do not replay a frame (both feeds, and log export of any log without flag 32, compact or not) decode it through `temporal::decode_logged`. It also refuses a temporal entry in a log without flag 64, as replay does. Export checks every graph frame, including frames before its cursor or after its size cut.
  - In a precision image, the revision supplement must equal each temporal edge's restated revision. Replay (`restore_revisions`) and the image check (`Coverage`) both enforce this. Nodes and ordinary edges still take the supplement's revision.
  - Ordinary edges and nodes in non-precision checkpoints still take the checkpoint sequence. That predates Gap 4 and is out of its scope.

Feed and serving:

- **Temporal feed.** `temporal_changes_after` reports `GraphChange::Temporal` entries at their log position, in commit order. A batch's `revision` is its commit sequence, and nothing is emitted at validity boundaries.
  - `EdgePut` carries the endpoint keys and the full series; `created = false` marks a promotion.
  - Corrections name the old segment by its exact bounds and carry only new values.
- **Other feeds.** `changes_after`, `read_log_changes`, and therefore REST/gRPC/MCP feeds and webhooks, return `UnsupportedFeature(64)` for a log with temporal entries. This includes a compacted image whose snapshot holds them; they never omit temporal changes.
- **Serving surfaces.** Every server, binding, CLI and MCP open uses the same gate, which is off outside this crate's tests. They refuse temporal files at open with `UnsupportedFeature(64)`.
  - The new `to_json` arm (`{"op": "unsupported_temporal"}`) and gRPC arm (`Op::Unspecified`) are unreachable exhaustiveness markers.
  - OpenAPI, proto, query text, the CLI and the dashboard are unchanged.

Coverage added in slice B (all `cargo test --lib`):

| Acceptance row | Tests |
|---|---|
| Identity/schema | `engine::state::temporal::tests` (put/promote identity, schema per segment, UniqueTriple per series, invariants); `engine::mutate::temporal::tests` (idempotent keyed create, promotion) |
| Corrections | `writes::corrections_are_atomic_and_revision_checked_against_the_batch_start`, `mutate::temporal::tests::series_changes_require_the_exact_series_revision` |
| Rollback | the same and `writes::limits_roll_back_the_whole_plan_including_tentative_ttl_cleanup` (vector, TTL cleanup, revisions and header unchanged) |
| Limits | 4,096 ops accepted, 4,097 rejected; 256 segments accepted, 257 rejected; byte overflow rejected; one frame with TTL cleanup, temporal change and vector (`writes::limits_…`, `state::temporal::tests::segment_cap_and_series_bytes_are_enforced`) |
| TTL | `writes::ttl_reclaims_whole_series_and_history_never_restores_them` (node cascade, edge-only expiry, compaction drops due series, key reuse gets a new ID, `verify_graph`) |
| Reopen | `writes::temporal_writes_need_a_file_copied_out_with_the_capability`, `state::temporal::tests::snapshot_restates_series_and_equals_replay`; refusal when support is off |
| Activation (copy-out) | `activate::a_copy_keeps_every_frame_under_a_new_lineage_and_the_source_is_untouched`, `activate::activation_refuses_without_publishing_anything`, `activate::publication_faults_never_leave_a_partial_copy` (plain and encrypted), `activate::an_encrypted_copy_is_resealed_under_the_same_key`, `activate::backups_and_restore_continue_on_both_sides`, `activate::replicas_follow_their_own_lineage_and_reseed_from_the_copy`, `activate::a_directory_store_copies_out_to_a_single_temporal_file`, `activate::a_precision_image_copies_out_with_its_catalog` |
| Revision context | `revisions::an_ordinary_frame_never_restates_a_revision`, `revisions::a_snapshot_revision_stays_within_its_sequence`, `revisions::a_log_without_the_capability_holds_no_temporal_entry` (plain and encrypted; replay, both feeds, and closed and open log export at cursors before and after the bad frame, whole and size-cut), `revisions::a_precision_supplement_must_match_a_temporal_revision`, `state::temporal::tests::snapshot_restates_series_and_equals_replay` |
| Graph modes | `exports::every_graph_mode_exports_only_valid_temporal_frames`: history, compact without precision, and precision images, plain and encrypted. Each corrupt case (op-12 revision 1, snapshot revision 0 or above its sequence, flag 64 cleared) is refused by replay, both feeds, and closed-file, open-log and `Database::export_log` export at four cursors, whole and cut at one byte (648 refusals). Valid logs export the same verbatim frames on all three paths |
| Revisions | `state::temporal::tests::a_snapshot_keeps_temporal_revisions`, `writes::temporal_revisions_survive_conversion_checkpoints_and_reopen` (conversion, checkpoint, reopen; compaction renumbers) |
| Compaction / Directory checkpoint | `writes::temporal_state_survives_compaction_conversion_and_checkpoints`, `writes::a_precision_image_keeps_series_and_revisions` (P2b snapshot load) |
| Encryption | `writes::encrypted_temporal_files_round_trip` |
| Feed/surfaces | `writes::only_the_temporal_feed_reports_temporal_changes_in_commit_order`; old feeds refuse, before and after compaction; missing flag is `Corrupt` |
| Format capability | `writes::builds_without_support_refuse_temporal_operations_and_change_nothing`, `writes::read_only_handles_refuse_temporal_writes`, `writes::temporal_writes_need_a_file_copied_out_with_the_capability` (legacy files unchanged, refused batches write nothing) |
| Standalone gate | `crates/vlite-graph/tests/temporal_gate.rs`: an ordinary build of the engine refuses `set_temporal(true)` and a snapshot holding a temporal edge |

Fail-first evidence:

| Suite | Result with the stated stub |
|---|---|
| Engine state (`apply_temporal` stubbed) | 8 of 9 failed |
| Planner (`Planner::temporal` stubbed) | 5 of 5 failed |
| Adapter (`mutation` always refusing) | 10 of 10 write tests failed |
| Feed (temporal arm refusing) | the feed test failed with `UnsupportedFeature(64)` |

Shared-file hooks, each marked `// Gap 4:`:

| File | Slice | Hook |
|---|---|---|
| `src/format.rs` | A | flag constant |
| `src/format.rs` | A | header gate |
| `src/format.rs` | A | flag requires `FLAG_GRAPH` |
| `src/format.rs` | B | `adopt` after replay in `load_inner` |
| `src/format.rs` | B | `adopt` in `load_graph_image` |
| `src/property_graph/frame.rs` | B | `restated` arm |
| `src/property_graph/frame.rs` | B | `compacted_bodies` emits `edge_snapshot_entry` |
| `src/property_graph/frame.rs` | review fix 3 | `compacted_bodies` takes the image `floor` and passes it to `edge_snapshot_entry` |
| `src/compact/mod.rs` | B | `ImageCheck::snapshot` edge coverage arm |
| `src/compact/mod.rs` | merge `7ff7f20f` | `adopt` for a graph state first made after a precision image |
| `src/compact/mod.rs` | merge `7ff7f20f` | `adopt` after `PendingSnapshot::finish` in `ImageCheck::load_snapshot` |
| `src/maintenance.rs` | B | compaction and upgrade carry `FLAG_TEMPORAL` |
| `src/maintenance.rs` | review fix 3 | `write_v5_image` passes `floor` to `compacted_bodies`; its test image builder passes 0 |
| `src/directory.rs` | B | `WAL_FLAGS` carries the flag; the checkpoint thread copies the test gate |
| `src/property_graph/frame.rs` | round 2 (A) | `decode_frame(snapshot: Option<u64>, …)` (was `bool`) checks `TemporalEntry::fits` |
| `src/property_graph/feed.rs` | round 2 (A) | passes the op-15 frame sequence to `decode_frame` |
| `src/format.rs` | round 2 (A) | op-15 load passes `Some(sequence)` to `decode_frame` |
| `src/compact/mod.rs` | round 2 (A) | `ImageCheck::frame` passes `Some(frame.sequence)` to `decode_frame` |
| `src/compact/catalog/tests/peak.rs` | round 2 (A) | test call passes `Some(u64_at(frame, 8))` (precision owner's test) |
| `src/compact/mod.rs` | round 2 (B) | `Coverage::temporal`: the snapshot's temporal `(edge, revision)` pairs, compared with the revision run |
| `src/property_graph/mod.rs` | round 2 | `write_graph_batch` refuses temporal operations before planning; the `commit_graph` header hook is removed |
| `src/logops.rs` | round 3 (A), round 4 | `read_segment` decodes each op-12 and op-15 frame of a log without flag 32 with `temporal::decode_logged`, after any compact image check and before the cursor and size cut |
| `src/property_graph/feed.rs` | round 3 (A) | `FeedGraphs::frame` decodes with `temporal::decode_logged`; its own capability check is removed (the `capable` field became `flags`) |

The precision baseline `5080b28d` (`SnapshotBuilder`, `PendingSnapshot`,
`GraphRecords`, `check_frame_writes`) is merged in `7ff7f20f`. In
`engine/state/snapshot.rs`, the temporal arm of `SnapshotBuilder::load_entry`
admits tag 65 at the ordinary edge's canonical position, checks the gate and
the revision range, and restores the series.

Known gaps and follow-ups (not yet tested unless stated):

1. **Activation surface.** Activation is a gated, crate-private copy-out. The public `maintenance::enable_temporal` is proposed, not added, and production activation is not approved. Replicas of the source must be reseeded from the copy, which starts its own backup chain.
2. **Edges created in the batch.** An edge created earlier in the same batch has no batch-start revision, so it cannot be corrected in that batch; create it with its full series.
3. **Revisions after compaction.** Compaction and upgrade renumber temporal revisions under the existing new-lineage contract (see Persistence).
4. **Ordinary reads.** Private/gated slices C1 and C2 now apply one fixed T/C to get, find nodes, neighbors, traverse, shortest path, retrieve expansion, and typed/text query and explain reads on temporal files. Temporal edges and endpoints hidden at T/C are neither returned nor traversed. Retrieval's pivot search, embeddings, ranking, payloads, attachments, and full historical recall remain current/nonhistorical. No production temporal reads are enabled.
5. **Feed gaps.** The feed does not report TTL deadlines or reclaims, as in Gap 3. No surface outside Rust reports temporal changes.
6. **Measurement.** Memory per segment and production caps are not measured, and benchmarks are unchanged.

Slice C2 implementation and its acceptance evidence are a candidate for
independent acceptance of the private, unreleased gate. Independent acceptance
is pending. Release activation, serving approval, and production qualification
remain outstanding; flag 64 stays closed in release builds.

Slice C1 (unreleased, private/gated) implements:

- a shared edge visibility predicate: binary segment selection at signed T,
  edge TTL and both endpoint TTLs at the independent clock C
- crate-private as-of get by ID/key, node get, find nodes and neighbors; node
  metadata remains current, and selected edge properties never fall back to
  the latest segment
- effective `as_of_ms`, `ttl_clock_ms` and `view_sequence` metadata in private
  results; default T=C is checked for i64 overflow, while explicit T permits
  the full unsigned TTL clock
- timeline inspection with explicit segment-index bounds up to 256; validity
  is ignored, but edge and endpoint TTLs are always enforced

C1 initially rejected cursor inputs. C2 now binds resolved T/C (including
defaults) into temporal page cursors, resumes with those captured selectors,
rejects conflicting overrides, and preserves graph-sequence stale detection.
Invalid-at-T adjacency entries still consume the read budget.

C1 tests exercise oracle boundary cases through actual edge reads, gaps and
touching intervals, 256-segment reads, rejected changes without mutation, TTL
before/equal/after expiry independently of T (including both endpoints and
post-purge non-resurrection), bounded timelines, and equal immediate/reopened
plain and encrypted reads. C2 replays the retained read-cases oracle through
real Rust neighbors, traverse, and shortest-path reads, and covers fixed-T/C
continuation, stale graph detection, certified budget exhaustion, query-text
explain selectors, retrieve expansion, and plain/encrypted reopen equality.
These tests are candidate acceptance evidence for the private C1/C2 read
implementations only, not release activation, serving approval, or production
behavior; independent acceptance remains pending.

Turning the gate on for release builds also waits for slice C.

### Proposed decisions for review

1. Capability and error: flag 64 with the existing `UnsupportedFeature(u16)`
   carrying 64, not a new error. Like the P3a precision gate, release builds
   refuse it until reads (slice C) qualify; only this crate's tests switch it
   on per thread. Without that gate a release build would serve temporal
   edges as empty current edges.
2. Identity: one edge ID and optional key per series; source, destination and
   type are fixed; no segment ID. A segment is addressed by its exact old
   bounds plus `Expected::Revision` on the series, as the design proposes
   (Graphiti stores validity per fact edge; XTDB and Datomic address history by
   entity plus time, not by a version ID).
3. Bounds type: `Option<i64>` milliseconds on the wire with a presence byte, so
   no integer sentinel represents infinity.
4. Segment cap: 256, enforced by the decoder, by every mutation and by the
   oracle. A series must also fit one 16 MiB body (slice B), because the whole
   series is restated in snapshots.
5. Category-prefixed reason strings keep oracle categories testable without a
   public error enum.
6. API shape: wrapper variants `GraphOp::Temporal(TemporalOp)` and
   `GraphChange::Temporal(TemporalChange)`, with `#[non_exhaustive]` nested
   enums, so later temporal operations do not break callers again. Because
   `GraphChange` is exhaustive, the new variant is a next-major change.
7. Promotion: a temporal `PutEdge` (tag 65) over an ordinary edge with the
   same ID, endpoints, type and key turns it into a temporal edge. This
   follows Graphiti, which adds validity to an existing fact edge instead of
   copying it. Promoting an edge that is already temporal is
   `already_temporal`.
8. Batch revisions: inside one batch, a temporal precondition is checked
   against the edge's revision before the batch, recorded before the
   batch's first mutation of the edge of either kind. Supersession (close
   plus append) is then one revision-checked unit, like a Datomic
   transaction against one basis.
9. Enabling: an explicit copy-out into a new lineage (user-approved,
   replacing first-write enabling); see
   [Activation publication (decided: copy-out)](#activation-publication-decided-copy-out).
   Directory stores inherit the capability only by conversion.
10. Feed: rather than mixing temporal operations into existing feeds,
    existing feeds refuse temporal logs and an opt-in Rust feed reports
    them. This holds until a versioned feed extension and an approved
    OpenAPI/proto diff exist (serving, slice E). Corrections carry the old
    bounds and only the new values, never guessed old values.
11. Engine gate: the standalone engine (`vlite-graph`) admits temporal
    state only in test builds (`engine::temporal::supported()`, which is
    `cfg!(test)`). In any other build, `GraphState::set_temporal(true)`
    fails and snapshot load refuses a tag-65 entry. Engine callers outside
    `vlite` therefore cannot create temporal state before slice C either.
12. Tag-65 revision: the revision is part of the payload of the unreleased
    tag 65. No flag or tag allocation changes. Zero means "the commit
    sequence"; only snapshots carry a nonzero value. Replay rejects a
    nonzero value, because a restated op-12 body followed by an expiry
    entry would otherwise diverge from replay.
13. Revisions across new lineages: compaction and upgrade renumber temporal
    revisions like every other revision (the existing new-lineage
    contract). Callers holding an `Expected::Revision` from before such a
    rewrite get a precondition failure, as they already do for ordinary
    edges. `enable_graph` never meets temporal state, and `strip_graph`
    removes it. Copy-out activation and same-lineage checkpoints keep
    revisions.
14. Series bytes: every series change except a removal rechecks the
    whole-series byte limit, closing included. The check runs as each
    entry is applied, so a failure rolls back the whole batch through the
    ordinary undo log.
15. Activation surface: the proposed public form is
    `maintenance::enable_temporal(source, destination, key) ->
    Result<Inspection>`, beside `enable_graph`. Until it is qualified it
    is the crate-private `property_graph::temporal::enable_temporal`, which
    builds without support refuse with `UnsupportedFeature(64)`. It has no
    `site/api.html` row.
16. Activation is not compaction: the copy keeps every frame (re-sealed
    under the same key with a fresh salt when encrypted, like
    `reencrypt`), not a rewritten image. Sequences, revisions, absolute
    deadlines (due entities included), history, imports and precision
    blocks are kept as they are.
17. Revision context: `decode_frame` takes the op-15 sequence (`None` for
    op 12), so every reader applies `revision_fits` to tag 65 the same
    way. Replay and snapshot load reuse the same function. Readers that do
    not replay a frame, both feeds and log export of any log without flag
    32 (compact or not), decode it through `temporal::decode_logged`, which
    adds the capability check that replay applies through `adopt`. A
    precision image check (flags 16 and 32) replays graph frames with the
    same decoder and `adopt`, so precision export does not decode twice.
    Export decodes every graph frame, even before its cursor: a valid log
    exports byte-identically, and only a log that replay and the feeds
    already refuse is newly refused.

### Review fixes, round 1

| Blocker | Fix | Fail-first evidence (before the fix) |
|---|---|---|
| 1, high: a close could outgrow the series byte limit | `apply_temporal` checks the series bytes after every entry except a removal, and undoes on failure | Engine `closing_cannot_outgrow_the_series_bytes`: `InvariantViolation("invalid series")` instead of `series_bytes`. Adapter `a_close_that_outgrows_the_series_bytes_rolls_back_its_batch`: the close was accepted |
| 2, high: in-place activation is not atomic | Resolved in round 2 by the user-approved copy-out activation; see [Activation publication (decided: copy-out)](#activation-publication-decided-copy-out) | Probes listed in that section |
| 3, medium: non-precision snapshots reset revisions | Tag 65 restates the revision in checkpoint, conversion and P2b images; snapshot load restores it | Engine `a_snapshot_keeps_temporal_revisions`: `[50, 50, 50]` instead of `[2, 3, 50]`. Adapter `temporal_revisions_survive_conversion_checkpoints_and_reopen`: 9 instead of 7 after `convert_to_directory` |
| 4, medium: the batch-start revision was captured lazily | `Planner::push` records an edge's revision before the batch's first mutation of it, of either kind | Planner `the_batch_start_revision_survives_earlier_ordinary_mutations`: `PreconditionFailed { current: Some(4) }`. Adapter `an_earlier_ordinary_mutation_keeps_the_batch_start_revision`: `RevisionMismatch { current: Some(8) }` |
| 5, high: the standalone engine could enable temporal state | Engine gate (decision 11): `set_temporal(true)` and snapshot admission refuse outside test builds | `crates/vlite-graph/tests/temporal_gate.rs`: the ordinary-build snapshot load returned a temporal state, and `set_temporal(true)` succeeded |

### Review fixes, round 2

| Finding | Fix | Fail-first evidence (before the fix) |
|---|---|---|
| Blocker 2: activation publication | The user approved copy-out activation. `enable_temporal` publishes a new temporal-enabled file. The in-place header rewrite, its `FAIL_HEADER` failpoint and the `commit_graph` hook are removed. Temporal batches on files without flag 64 are refused before planning | With `enable_temporal` stubbed to refuse, 22 of 28 `property_graph::temporal` tests failed with `UnsupportedFeature(64)` |
| A, medium: the feed decoded a nonzero op-12 revision | `decode_frame` checks `TemporalEntry::fits` with the frame's context, so replay, both feeds and the image check refuse it alike (log export: round 3) | `an_ordinary_frame_never_restates_a_revision`: the temporal feed accepted an op-12 `PutEdge` with revision 1 (plain). `a_snapshot_revision_stays_within_its_sequence`: the feed accepted snapshot revision 0 |
| B, medium: the precision supplement overwrote a temporal revision | `restore_revisions` requires equality for temporal edges. The image check's `Coverage` compares the snapshot's temporal revisions with the revision run | `a_precision_supplement_must_match_a_temporal_revision`: an image whose tag 65 says 7 and supplement says 8 opened. Engine `snapshot_restates_series_and_equals_replay`: `restore_revisions` returned `Ok(())`. With only the replay fix, the temporal feed still accepted the image (line 266) |

Committed state of review round 1 is `bf63d83f`. Number allocations (flag
64, tags 65 to 69) are unchanged.

### Review fixes, round 3

The execution re-review of `a79a8939` closed blockers 1 to 5 and finding B,
and reproduced finding A at the non-precision log exporter.

| Finding | Fix | Fail-first evidence (before the fix) |
|---|---|---|
| A, medium: non-precision log export did not decode graph frames | Without a precision image check, `read_segment` decodes every op-12 and op-15 frame through `temporal::decode_logged` before its cursor and size cut. Export now refuses what replay and the feeds refuse: a nonzero op-12 revision, a snapshot revision of 0 or above the snapshot sequence, and a temporal entry in a log without flag 64. The feeds use the same function instead of their own capability check | The reviewer reproduced six successful exports of corrupt logs: ordinary revision 1, and snapshot revision 0 and above the sequence, each plain and encrypted. Each of the three revision tests panicked at its first bad export (`revisions.rs:144`, no size cut): at cursor 0 for `an_ordinary_frame_never_restates_a_revision` and `a_log_without_the_capability_holds_no_temporal_entry`, and at cursor 8 for `a_snapshot_revision_stays_within_its_sequence`. Replay and both feeds already refused every case. With only `decode_logged`'s capability check disabled, the capability test fails at its feed assertion |

Valid logs export byte-identically. The scan already read every frame to
the end of the log. It now also decodes each graph frame and drops the
body before the next one, so export memory stays bounded by one frame.
Legacy logs with corrupt graph bodies are newly refused, as replay and the
feeds already refused them. The committed state of round 2 is `a79a8939`.

### Review fixes, round 4

The execution re-review of `4ed8bca2` closed blockers 1 to 5 and finding B,
and passed the non-compact matrix, but found finding A still open for
compact images without precision.

| Finding | Fix | Fail-first evidence (before the fix) |
|---|---|---|
| A, medium: log export skipped graph-frame checks in a compact image without precision (flag 16 without 32) | `ImageCheck` exists for every flag-16 log, but it replays graph frames only with flag 32; round 3 decoded them only without an image check. `read_segment` now runs any image check first, unchanged (table, derived blocks, partitions, end digest). Then it decodes each op-12 and op-15 frame with `temporal::decode_logged` whenever the header lacks flag 32. Precision export is unchanged and gains no second decode | `exports::every_graph_mode_exports_only_valid_temporal_frames` on `4ed8bca2`: 192 of 648 refusals missed. All were compact-without-precision exports: 2 keys, 4 corrupt cases, 4 cursors, 2 cuts and 3 export paths. Replay and both feeds refused every case in every mode, and history and precision exports refused every case. Valid logs exported identically on all three paths in every mode |

Graph-mode audit of log export. The header refuses flag 16 without flag 8,
flag 32 without flag 16, and flag 64 without the graph flag, so these are
the graph modes a reader can meet. Each runs with or without flag 64 and
with or without encryption:

| Header | Image check | Graph-frame checks in export |
|---|---|---|
| no 16 (plain log or history image) | none | `decode_logged` (round 3) |
| 16 without 32 (compact image) | blocks only | `decode_logged` after the image check (round 4) |
| 16 and 32 (precision image) | replays graph frames | `ImageCheck::frame`: `decode_frame` with the frame context, `adopt` for the capability |

Replay and the feeds decode every graph frame themselves in every mode.
`Database::export_log`, `export_log_segment`, replicas and the server's
log export all cut segments with `read_segment`. The fixture builds a
compact image from a valid history image: a capability table with one
declared derived kind after the collection frames, one derived block, and
an end block over every image frame's prefix. Replay, the temporal feed
and all three exports accept it before any mutation. The other feed refuses
a temporal log at its first temporal entry with `UnsupportedFeature(64)`.
For the corrupt op-12 case that is the image's valid entry, which comes
first. The committed state of round 3 is `4ed8bca2`.

### Activation publication (decided: copy-out)

Decision (user-directed, during the slice A/B review): replace automatic
first-write activation with an explicit copy-out into a new temporal-enabled
database with a new lineage. The original is untouched, and its replicas are
reseeded from the copy. The decision approves this design and a gated,
private implementation. It does not approve production activation, and the
additive Rust conversion API stays gated until it is reviewed and qualified.

Implemented (`property_graph/temporal/activate.rs`, crate-private
`enable_temporal(source, destination, key)`):

- It follows `logops::reencrypt`. The source is read under its exclusive
  lock (`maintenance::source`), so an open source is `Locked`, and a
  directory store is read as its logical stream. The source must load
  completely: an incomplete tail is `IncompleteTail`, never repaired.
- Its header must have `FLAG_GRAPH` (otherwise `GraphsNotEnabled`) and not
  already flag 64 (otherwise `GraphInvalidOptions`).
- The copy's header keeps the version, dimensions, collection floor and
  flags, adds flag 64 and draws a new lineage. Every frame follows
  unchanged. An encrypted source is resealed under the same key with a
  fresh salt (its own cipher domain), so its plaintext frames are
  identical.
- Output goes through `maintenance::publish_with`. It is staged beside the
  destination, reloaded and validated, synced and locked, then published
  without overwrite. A failure before the name appears leaves nothing. A
  failure after it is `PublicationIndeterminate` with a complete copy that
  a retry never replaces.
- Activation is not compaction. Retained logical content, absolute TTL
  deadlines (due entities stay until a write reclaims them), revisions,
  sequences, imports, precision and encryption are kept.
- The old lineage is unaffected. Its backups and point-in-time restore keep
  working. A backup of the source cannot anchor a restore of the copy
  (`InvalidRestoreTarget`). A follower of the source keeps following it,
  refuses a segment of the copy (`ReplicaDiverged`) and stays unchanged.
  New followers are seeded from the copy.

Tests (`property_graph::temporal::tests::activate`):

- source bytes are unchanged and the copy's header differs only in lineage and flags;
- refusals publish nothing: an existing destination, `Locked`, `NotEncrypted`, `GraphsNotEnabled`, `IncompleteTail`, the gate off, and an already temporal source;
- staged-write faults, plain and encrypted: a partial header (16 bytes), a header with no frames, a torn first frame and a mid-log cut;
- the `Staged` and `Visible` failpoints, and a no-clobber retry;
- an encrypted reseal, with a missing key and a wrong key;
- PITR on both lineages, with and without a base, including a cut at the activation sequence;
- replica continuity and reseed;
- a directory-store source;
- a precision image source (catalog and flags kept).

Evidence that led to the decision, from scratch probes of the former
in-place header rewrite (single-file store, support on unless stated; the
probes were not committed):

| Probe | Result |
|---|---|
| Replica follower bootstrapped before activation applies the next segment | `ReplicaDiverged("segment header differs from follower")` |
| `restore_to` with a base backup taken before activation | `InvalidRestoreTarget("base backup is not a prefix of the log")` |
| `restore_to` without a base, cut before activation | the output still has flag 64 |
| That output, opened by a build without support | `UnsupportedFeature(64)` |
| Crash after the header sync, before the frame | opens as temporal with no temporal edge; a build without support refuses it with `UnsupportedFeature(64)` |
| Torn header, either half (flags and checksum from different versions) | `Corrupt { offset: 0, reason: "database header checksum mismatch" }`, unrecoverable |

Constraints from existing contracts:

- Required flags so far are set when a store is created or by copy-out:
  `enable_graph` (REQ-003) publishes a new file with a new lineage and never
  modifies its source.
- P3 Decision W states that flag 32 "is never added by in-place header
  mutation". The P3 analysis rejected in-place activation because it needs
  "platform-safe file replacement while locks/read generations remain
  pinned".
- The publication helpers (`maintenance::publish_held`, `publish_with`)
  publish a validated, synced staged file under a new name and never
  replace an existing one.
- Point-in-time restore proves lineage by byte prefix, from byte 0, and
  replicas compare headers. Any same-lineage header change breaks both.

Alternatives considered, smallest first (1 was chosen):

1. **Copy-out activation (chosen).** Add
   `enable_temporal(source, destination, key)` beside `enable_graph`. It
   writes the source's graph image with flag 64 through `write_v5_image`
   and `publish_held`. The source is unchanged, and the output gets a new
   lineage.
   - A temporal write to a file without flag 64 returns
     `UnsupportedFeature(64)` before planning. The first temporal write is
     then an ordinary atomic append, with no header write. The in-place
     header code and its failpoint are removed.
   - Semantics:
     - Publication is atomic and already fault-tested (`Staged` and
       `Visible` failpoints).
     - Backups and point-in-time restore of the old lineage keep working
       and restore pre-activation files.
     - The new file starts a new backup chain.
     - Replicas are reseeded, as after compaction.
     - Directory stores convert from the activated file.
   - Cost: one function (kept crate-private and gated until qualified; the
     planned image rewrite became a frame-preserving copy, decision 16),
     8 tests, and no change to `logops.rs` or `replica.rs`.
   - Contract change: the spec sentence becomes "enable the capability with
     an explicit copy-out step; the first temporal write requires it". An
     optional follow-up is a creation-time option.
2. **In-place activation with defined recovery.** Keep "first write
   enables" and make every window recoverable. This needs:
   - In `format.rs`, a header whose checksum matches only with bit 64
     toggled is read as the pre-activation header. A writable open repairs
     it. Corruption detection weakens slightly for that one bit.
   - A supporting writable open clears flag 64 when the log holds no
     temporal entry, to close the crash window.
   - In `logops.rs`, `is_prefix` ignores the activation difference.
     `restore_to` writes the pre-activation header when the cut precedes
     the first temporal entry.
   - In `replica.rs` and `lib.rs`, followers accept the 0 to 64 transition
     in the segment holding the first temporal frame and rewrite their own
     header the same way.
   - Fault tests for each torn state.

   This changes the restore and replica contracts and contradicts the
   Decision W principle. It also edits `format.rs`, which another owner is
   changing concurrently.
3. **Same-file replacement on first write.** Stage a full copy with the flag
   and the frame, then rename it over the open file. This is the
   file-replacement transaction engine that P3 declined: O(file) work per
   activation, renaming over open and locked files on Windows, and pinned
   read generations.
   - With the same lineage, restore and replicas still break.
   - With a new lineage, it behaves like alternative 1, but an ordinary
     write silently invalidates backups and replicas.
4. **No header flag; entries only.** Old builds would meet unknown tag 65
   and report `Corrupt`, not `UnsupportedFeature(64)`. That contradicts
   "readers without the capability must refuse before publishing partial
   state" with a precise error. Rejected.

The chosen mechanism's fault, restore and replica tests are listed above.
