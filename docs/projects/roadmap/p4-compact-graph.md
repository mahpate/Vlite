# Speed P4: compact graph

**Status:** approved design; private test-only CSR, ordinary typed-property,
label-posting, attachment and schema-declaration codec slices are implemented.
Private parallel import staging is independently accepted and integrated, but
unreleased.
Private decoded-CSR identity, adjacency, TTL and temporal reads are implemented
and independently accepted.
P4 slice 1b has a separate private, test-only graph-base checkpoint-framing
candidate pending independent acceptance: critical kind `0x0300`, codec
version 1, unchanged `VLGCSR06` payload, one block per graph, and a 64 MiB
total block-body cap. Multi-block graph partitioning is out of scope; both
precision activation and the separate P4 test gate are required to write, and
gate-off/release readers refuse the image. No performance or size claims are
made. Full P4 implementation and measured results remain outstanding.
The codec's own 64 MiB payload cap reports `invalid graph base source` before
the envelope body-cap check, so the writer's `graph base block exceeds body cap`
reason is reachable only with a lowered test-only `BodyLimit` or an otherwise
valid encoded payload in the final 40-byte window of the codec cap.
The private fix-round candidate carries the gate into directory checkpoint
workers, shares writer/preflight/declaration admission, and rejects graph
blocks without history/graph flags or with extra sections. Named regressions
cover multi-body snapshot/feed parity, expiry cascade, empty graph registries,
exact body-cap boundaries, codec versions and mixed op-15/block rejection;
independent acceptance remains pending.
Depends on [P2](p2-directory-store.md); exits remain
[S3/S4/F6](../performance/storage-goals.md): **<=24 B/edge disk,
<=100 B/edge resident, >=1M edges/s bulk load, <=300 ms reopen at 1M edges**.
No target is relaxed.
Shared framing and capability allocation follow the
[compact-storage contract](compact-storage-contract.md). P4 owns its graph
codec; shared format and recovery changes have a single storage owner.

## Existing base and boundary

`engine/state.rs` owns B-tree records/indexes and sorted adjacency vectors with
shared relationship strings; `Properties` already uses a sorted vector.
P2's op-15 snapshots (`property_graph/frame.rs`, `engine/state/snapshot.rs`)
bulk-build that same heap state; they are not CSR. `directory.rs` maps immutable
files but exposes a logical frame stream. Mapping alone therefore cannot meet P4.

Propose compact, versioned graph blocks **inside the checkpoint image**, not
sidecars or a second manifest. Add a new format operation; do not reinterpret
op 15. Keep op-12 WAL writes and support reading existing single-file/P2 data.
Older binaries must reject the new operation. Require explicit conversion/format
opt-in until qualified.

## Immutable representation

Per graph, preserve its ID, schema, sequence and allocation high-water marks.
Map sorted live u64 node/edge IDs to dense local ordinals: never allocate by
maximum ID, renumber public IDs, or reuse deleted IDs. Use implicit consecutive
ID runs, otherwise blockwise delta/varint IDs with sparse seek anchors.
Use checked 32-bit ordinals, widening explicitly when necessary.

Store endpoints and relationship codes once in an edge table ordered by edge
ID. Two CSR indexes contain row offsets and edge ordinals, outgoing and incoming.
Group rows by relationship then edge ID; delta-code ordinals with restart anchors.
Do not sort neighbors by destination: existing
`Graph::adjacency` orders by `(relationship string, edge ID)`. A lexicographic
dictionary makes codes preserve that ordering. Keep distinct parallel edges;
store self-loops in both directions but merge them once for `Both`.

Dictionary-encode node labels, relationships and property names per checkpoint;
store sorted label sets and label postings. Store typed property columns with
presence maps, packed small integers and optional string dictionaries, choosing
raw blocks when smaller. Preserve f64/GeoPoint bits, property order,
keyed-edge equality and schema/index behavior. Keys, attachments and uncommon
properties live in indexed byte blocks, not one allocated object per entity.
Decode projected values into existing public views.

Replace borrowed-record assumptions behind `GraphRef`, query, traversal, schema
and mutation planning with compact-base accessors, without retaining a duplicate
heap copy of the base. Preserve cursor ordering, canonical
paths, limits, metering and default query results. P4 does not change vector
precision or exact-search defaults; P3 decides those separately.

## Mutations, expiry and attachments

Layer bounded mutable records, adjacency changes and tombstones over the base.
Merge by public ID/order, newest committed version winning. The planner still
validates ordered batches tentatively, appends one durable op-12 frame, then
applies atomically; retain 4,096-user-operation/16 MiB limits and rollback.
Ordinary node deletion still requires no incident edges. Checkpoints fold the
overlay into a new base off the read path; bound overlay/WAL replay work with
explicit writer backpressure, not query stalls.

Preserve [Gap 3](../product-gaps/product-gaps.md) graph TTL semantics.
Encode optional node/edge deadlines sparsely in the base and overlay, not as
mandatory per-entity fields. At `now >= deadline`, an entity is absent, including
key lookups and conditional writes; an expired node hides **all** incident edges,
even edges without deadlines. Edges can independently expire. Apply the same
visibility rule to scans, indexes, traversal, counts and hybrid retrieval, using
one time per operation and respecting an explicit graph read clock.
Persist deadlines; reopen must not revive data.
Reclaim expired entities and incident edges at checkpoint/compaction or in a
committed graph write, without requiring a degree-sized user batch.

Preserve one record per `(node, collection)` and one owner per record globally.
Attached records must exist without record expiry; ordinary vector delete/expiry
guards and atomic graph/vector batches remain. Node expiry hides its attachments,
not the underlying records. Ownership protection remains until committed
reclamation removes the ownership entries; do not silently free an attached
record while its owner is still physically present. Validate attachments after
loading records and before publishing the graph.

## Parallel import and durable publication

Bulk import initially targets a new, exclusively locked directory, not concurrent
replacement of a live graph. Partition decoding/validation, resolve keys,
assign IDs deterministically in input order, build dictionaries, parallel-sort
adjacency partitions, prefix-sum degrees and encode disjoint blocks. Use bounded
buffers and sorted spill runs for skew/high-degree nodes. Globally check duplicate
keys, endpoints, schema multiplicity and attachment ownership before installation.
Publish only the complete durable import; normal batches retain their contract.

The independent standard-library
[`compact_graph_import_oracle.py`](../../../tests/compact_graph_import_oracle.py)
checks 19 hand-authored logical cases against sequential admission and partitioned
staging with sorted-run merging. It covers deterministic IDs, dictionaries,
adjacency, degree prefixes, attachment ownership and first-error ordering across
worker-count, partition-layout and spill-size choices. Its bounded model admits
nodes before edges into an empty ownership domain, assumes referenced records
already exist without expiry, and retains physical deadlines without evaluating
a write clock. It does not model interleaved writes, schema/property admission,
worker-local spill buffers, actual concurrency or durable publication. This is
reference coverage, not performance qualification.

`src/compact/graph/import.rs` is private test-only staging, independently
accepted and integrated but unreleased. Rayon partitions decode and validate logical JSON rows,
then global ordered admission checks duplicate keys and attachment ownership
before parallel endpoint resolution. IDs follow input order. Sorted dictionaries,
worker-local bounded membership runs, tuple-ordered k-way merging and degree
prefix sums construct the same CSR as sequential admission. Workers encode
disjoint node/edge byte blocks; checked concatenation retains `VLGCSR06` bytes.
Rust consumes all 19 retained goldens, sweeps worker/partition/layout/run choices,
and compares seeded skew, hubs, self-loops, parallel edges, sparse/duplicate keys
and attachments with sequential admission. Representable sources also compare
with the engine/codec path; anonymous nodes require an explicit test-only key
remap, not a claim of unchanged-source engine parity.
Independent acceptance disabled the cross-node record-ownership check: the
`duplicate-owner-boundary` golden and seeded first-error parity tests failed,
then passed after restoring it. The integrated branch still requires full CI
qualification.

The staging admits nodes before edges into one new empty ownership domain.
Its fixture graph has ID 0, name `import`, sequence/revisions 1 and count-based
allocation marks; it is not an API for choosing graph lifecycle metadata.
It assumes attached records exist without record expiry and preserves deadlines
without clock filtering. Properties, schemas, edge keys, temporal series and
other unrecognized fields are explicitly refused; full engine write admission,
schema multiplicity and cross-graph ownership remain outstanding. Partitions
are limited to 1..256, run size must be positive, ordinals are checked u32 and
the assembled payload retains the codec's 64 MiB limit. Sorting buffers and
node/edge encoding blocks are bounded by run size in entries, not bytes; input,
staged rows, in-memory sorted runs and output coexist. There is no total-memory
bound, disk spilling, durable publication, live replacement, production
activation, or performance/size qualification.

For online checkpoints, build against a sealed P2 prefix at floor `S`; later
commits remain in the rotated WAL. Validate/fsync the temporary image, rename,
then atomically publish the existing checksummed manifest. No acknowledgement
precedes the applicable durability boundary. Keep lineage, high-water marks
and P2 sequence semantics: graph revisions restate at `S`, record revisions
survive, and restatement emits no change-feed events. Backup, packed export,
restore, replicas and re-encryption must understand the new blocks before
enabling writes.

Readers pin their base generation while it is in use. Publish the new base with
a short synchronized swap; retire old mappings only after the last reader
releases them. Building a checkpoint must not hold the query lock.

Extend loader/mapped-range access with indexed block offsets. Verify lengths,
overflow, offsets, canonical IDs, CSR coverage,
dictionaries, endpoints, checksums and attachment invariants before use. Corrupt
immutable data fails explicitly; only the active WAL permits torn-tail repair.
Reuse frame CRC and encryption envelopes: encrypted bodies require authenticated
decryption into bounded buffers, not plaintext mmap access or plaintext spills.
P2 shares a cipher descriptor across its chain, not per-block key IDs.
Count authentication, KDF and decrypt costs in reopen. Unmap before
Windows deletion; retain file-handle fallback.

## Accounting and acceptance

For `E` live edges and `N` nodes, estimate compact graph bytes as
`E*(8 endpoints + 1 type + 2 ID coding + 6 bidirectional membership)
+ 32*N + P + H + W`.
Here `P` includes keys, properties, labels, attachments and their indexes;
`H` includes dictionaries, anchors, padding, headers/checksums/encryption;
`W` is retained WAL. At `N/E=0.1`, fixed terms are about 20.2 B/edge,
leaving only 3.8 for everything else. The existing workload's four types and
integer `w` in `0..99` allow packing, not an assurance of passing.
Sparse IDs, strings, many labels, TTL, low degree and history can exceed budget;
report those costs. Topology-only numbers cannot pass S3.
Report both incremental edge cost and all graph-attributable bytes/live edge,
including steady-state WAL; report temporary checkpoint duplication separately.

Resident accounting includes heap, allocator overhead, resident mapped pages,
derived indexes, overlay and decrypted caches. A provisional 100 B/edge envelope
is 24 base + 20 indexes + 20 overlay + 16 caches + 20 other overhead.
Report import/merge peaks and crypto scratch separately.
Existing `graph_memory` heap counts alone cannot establish S4.

The private codec slices implemented so far are in
`src/compact/graph.rs` behind `cfg(test)`. It preserves graph ID/name, graph
sequence and ID high-water marks; sorted public node/edge IDs and checked
32-bit dense ordinals; node/edge keys and revisions; labels, relationship
strings and sparse deadlines; bidirectional CSR; and ordinary node/edge
property maps, label postings, and per-node attachment maps. Attachment rows
are keyed by dense node ordinal and ordered by ordinal then collection; each
node has at most one record per collection. Decode reconstructs the graph-local
`(collection, record) -> node ID` owner index and rejects duplicate owners
within this graph. Cross-graph owner uniqueness remains a checkpoint-level
invariant and cannot be established from one graph block. Each label dictionary
code carries ascending dense node ordinals, checked against the per-node label
codes in both directions.
Decoded label scans emit ascending public IDs strictly above an optional cursor,
without expiry filtering, matching the engine label index. A private intersection
query supports 1..8 distinct labels, an exclusive cursor and a positive limit;
duplicate query labels collapse and unknown labels return no matches.
Property names are dictionary-coded in UTF-8 byte order, while
per-record typed values preserve strings, signed integers, finite float/GeoPoint
bits, and booleans. Each map retains the engine's existing field, string and
byte limits; this is a correctness format, not a columnar compression or size
claim. The versioned private payload magic is `VLGCSR06`; old `VLGCSR01`
through `VLGCSR05` bytes are refused, not reinterpreted. Its decoder
rejects noncanonical dictionaries/IDs, invalid endpoints or codes, incorrect
row ordering, missing or duplicate CSR membership, overflow, truncation and
trailing bytes, noncanonical label posting codes/ordinals, and missing or extra
posting membership. It rejects unordered/duplicate collections, invalid node
ordinals and duplicate `(collection, record)` owners within this graph. It also
checks high-water marks against live IDs, revisions against graph sequence,
and unique present node keys and edge keys in their separate namespaces.
Logical visibility uses `deadline <= clock` as expired, hides edges incident
to expired nodes, and emits the existing
`(relationship, edge ID)` adjacency order; the accepted
`graph-csr-cases.json` remains the oracle. Its full-u64 logical-only fixture
uses placeholder lifecycle metadata and exercises CSR construction directly,
not the persisted codec. Label access tests likewise cover 0 and `u64::MAX`
logically without accepting impossible persisted high-water metadata. A generated
engine graph checks decoded scan/intersection parity, including retained deadlines.

The schema-declaration slice stores the graph's existing `GraphSchema`
losslessly after the high-water marks: label and relationship definitions
with descriptions, typed property definitions (the engine's five types,
required/unique flags and exact/range index kinds), relationship endpoints
and multiplicity. Names, endpoints and properties are written in ascending
order, so equal schemas encode identically. The decoder bounds every count
and string by the remaining bytes and the existing schema limits, and it
rejects unknown type, flag and multiplicity codes. It also rejects duplicate
or unordered declarations, invalid UTF-8, overflow, truncation and trailing
bytes. Every accepted schema must pass the engine's own `GraphSchema::validate`.
Derived property indexes are not stored. A schema-bearing graph is loaded
through the engine snapshot loader on encode and decode. That loader
rebuilds the exact/range indexes and checks every declared type, required,
unique, endpoint and unique-triple rule. Data that violates the schema is
refused. Tests compare the rebuilt engine state with the original: the same
state, the same index choices and results from engine queries, and the same
accept/refuse outcome for later writes. A
schema needs keyed nodes, so synthetic keyless sources with a schema are
refused; engine nodes always have keys. This check reruns the full engine
load and is not a compact index. The compact base does not serve or enforce
schema rules by itself.

The independently reviewed `tests/compact_graph_overlay_oracle.py` and
`graph-overlay-cases.json` model committed-state projections, not an ordered
write API or a Rust overlay implementation. They check immutable edge identity,
retained allocation high-water marks and no ID reuse, newest-version precedence,
deadline preservation or explicit removal, TTL cascade and ordered adjacency.
CI runs the reference and its malformed-fixture checks. Write-time expiry,
keys, full graph revisions, properties, schema, attachments and temporal state
remain outside this logical reference; it cannot establish full engine parity.

The independently reviewed `tests/compact_graph_property_oracle.py` and
`graph-property-cases.json` model typed node/edge property snapshots: the five
existing value types, UTF-8 byte-ordered unique names, last-insert-wins
insertion, exact float/GeoPoint bit patterns, coordinate ranges and the
existing field, name, string and aggregate limits. CI runs the reference, its
malformed fixtures, shuffles and mutation rejections. It is not a serializer,
on-disk layout or Rust parity proof, and it models no topology, overlay, TTL,
schema, attachments, revisions or temporal state.

The independently reviewed `tests/compact_graph_label_oracle.py` and
`graph-label-cases.json` model label dictionaries, sorted per-node code sets,
ascending postings, exclusive-cursor label scans and bounded all-label
intersections, including IDs 0 and `u64::MAX`. CI runs the reference, its
malformed fixtures, shuffles and mutation rejections. It is not a byte format
or Rust parity proof, and models no expiry, properties, edges, keys or overlay.

The independently reviewed `tests/compact_graph_attachment_oracle.py` and
`graph-attachment-cases.json` model node-to-record attachments: at most one
record per node and collection, one owner per collection record, attach/detach
error precedence, retention on node replacement, release on deletion and
all-or-nothing batch rollback. CI runs the reference, its malformed fixtures,
shuffles and mutation rejections. It is logical test evidence for attachment
semantics, not a byte format or a proof of checkpoint-level owner uniqueness.

The reviewed `tests/compact_graph_schema_oracle.py` and
`graph-schema-cases.json` model existing declarations and snapshot conformance:
required/type rules, label-scoped uniqueness, directed endpoints, multiplicity
and single-condition exact/range index choices and results. Typed values retain
integer precision and normalize signed zeros for equality without coercion.
CI runs the hand-authored goldens, malformed inputs, seeded shuffles, limit
boundaries and effective mutations. This logical reference reports all
violations in a deterministic order, not the engine's first-error precedence.
It models no persisted bytes, TTL, temporal history, ordered writes, rollback
or full query planning, and is not a Rust or checkpoint parity proof.

This private, test-only codec is not wired to the checkpoint block registry,
shared framing, checkpoint reads or production builds. VLGCSR06 round-trips
retained temporal series with 0–256 ordered, non-overlapping segments, optional
signed bounds, typed per-segment properties, and the edge's identity, revision
and deadline; decoding reconstructs the equivalent tag-65 snapshot entry.
Temporal data is stored in a separate section, whose membership distinguishes
an empty series from an ordinary edge. Ordinary edge records remain on the
existing VLGCSR path apart from the magic/version change. Cross-graph
attachment-owner uniqueness is a checkpoint-level invariant; one graph codec
cannot validate it. The codec does not persist or serve property indexes;
schema declarations are preserved and their indexes and rules are rebuilt and
checked only through the engine snapshot loader. No changes were made to
op-12/op-15, public APIs, or
`src/openapi.json`.

Private reads in `src/compact/graph/read.rs` serve decoded nodes and edges by
public ID or key and merge CSR adjacency in relationship/edge-ID order, keeping
parallel edges distinct and self-loops single for `Both`. Reader-local lookup
indexes are built once; reads do not rebuild engine state. TTL uses clock C for
edges and both endpoints; temporal selection independently binary-searches the
half-open series at signed T, hiding gaps and empty series and using only the
selected segment's properties. Ordinary reads retain engine retention-only
semantics. These test-only reads do not wire GraphRef or query execution to CSR,
change VLGCSR06 bytes, or establish size/performance results.
Independent acceptance compared retained fixtures and seeded reads with the
engine, then changed the node TTL boundary from `<` to `<=`: the ID/key regression
failed by returning a node at its expiry, and passed after restoring the boundary.
The integrated branch still requires its full CI qualification.

The mutation overlay in `src/compact/graph/overlay.rs` is an unreleased,
private, test-only candidate pending independent acceptance. It consumes full
committed-state projection deltas in strictly increasing sequence order, not
ordered user writes; stale/duplicate sequences are refused. Separate node/edge
version maps retain tombstones and allocation high-water marks. Identity is
immutable (including keys); fresh IDs must reach the retained mark and cannot
allocate `u64::MAX`. Deadlines have explicit preserve/set/remove semantics.
Merged ID/key, ordered adjacency and count reads use one caller-supplied clock,
including endpoint-expiry cascade and temporal selection. Base key indexes are
built once; reads do not rebuild CSR or engine state.

The configurable entry cap counts distinct touched node plus edge IDs, including
tombstones. Replacements reuse their slot; exceeding the cap returns typed
`Capacity { cap, needed }` before changing state. Overlay adjacency has at most
two memberships per live overlay edge. Admission is atomic and validates a
materialized physical source through the existing VLGCSR06 codec, including
properties, schema/temporal conformance and graph-local attachment ownership.
Node removal requires incident edges to have been explicitly removed in the
same or an earlier committed projection, including physically retained expired
edges; this is stricter than a visibility-only removal and is not TTL
reclamation. Full record payloads carry properties, attachments and temporal
segments; they are not partial property patches. Schema/graph metadata is
fixed by the base; temporal-section membership follows the resulting physical
series, as in a direct engine source build. Invalid payloads return typed
`Invalid(CodecError)`; missing rows, identity/ID reuse, sequence and high-water
errors are also typed refusals.

The private fold produces a new decoded VLGCSR06 base, retaining expired
physical rows, deadlines, revisions and allocation marks. It neither reclaims
TTL data nor publishes a replacement. Entry bounds are not total-memory or
latency claims: admission/fold materialize and validate the source (the codec's
64 MiB image bound still applies), with base, overlay, candidate and encoding
scratch coexisting; counts scan the merged ID space and adjacency sorts its
result. There is no spill, backpressure scheduler, write planner, vector-record
existence check, cross-graph owner check, WAL/checkpoint/query wiring or
production activation. Retained overlay goldens and deterministic engine-history
comparisons are candidate evidence, not independent acceptance or performance
qualification.

Remaining P4 slices are shared framing and block dispatch/publication,
checkpoint/read integration, production mutation integration, full parallel
import (property and schema admission, spilling and durable publication), and performance and
cross-platform qualification.
Columnar property storage, native compact-index serving and schema enforcement
also remain outstanding.

Next roll out opt-in checkpoint reads, mutation/TTL integration, then parallel
import; enable defaults only after acceptance.
Extend existing graph storage/recovery/integrity/surface and directory recovery
matrices with old/new conversion, sparse IDs, parallel edges, self-loops, skew,
properties, attachments, deadline boundaries, checkpoint races, corruption,
crash points, encryption/key rotation and deterministic worker-count parity.

Qualify all four targets on the existing seeded 100k-node/1M-edge property
workload, unchanged durability/settings. Include parsing, validation, sorting,
encoding, sync and publication in import throughput; report node setup separately.
Time reopen until validated graph reads are usable, including WAL and encrypted
work, with cold/warm caches and WAL sizes disclosed. P2's 64 MiB threshold alone
does not guarantee 300 ms. Full verification and decryption remain linear;
do not hide work after the timer. F4's separate <=50 ms goal is unchanged.

**Validation:** the private codec slices and logical references have independent
review and local correctness checks; earlier slices passed the cross-platform CI
matrix, and each new slice is held to that matrix after integration.
Remaining implementation and performance qualification are outstanding.
DO-005 remains deferred to the major release under its
unchanged quiet-host gate (mean background CPU <=10%; no half-second sample
above 50%). `src/openapi.json`, benchmarks and dashboards remain unchanged.
