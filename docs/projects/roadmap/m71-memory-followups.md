# Agent memory follow-ups after 0.7.0 (0.7.1, lane M71)

Status: **design notes written slice by slice, before the code of that slice.**
Each section freezes when its slice is integrated. The decisions below were
delegated by the owner to the release coordinator (D0 to D6). Nothing here adds
a public Rust API, an engine `FormatVersion` or a route.
Authority: the [serving contract](serving-memory-contract.md), the
[adapter mapping](gap1-adapter-mapping.md#4-binding-configuration-prerequisite-2),
the [0.7.0 handover](handover-0.7.0.md#limits-shipped-in-070).

## 1. Binding record (`binding:v1`): F-01, F5, F6 (slice S2)

**Problem.** 0.7.0 reads a binding's identity (receipt window, generation, names)
from the bindings file at each start and keeps nothing in the database, so a
changed file silently breaks replay (a retry inside its original window answers
409 `idempotency_conflict`), one node labelled `VliteMemoryScope` makes any graph
adoptable, and a restart without a binding ends its reservation. The rotation
rules of the mapping (section 4) exist as `BindingSet::validate_rotation` but no
production code calls it.

**Decision (D0, approved).** Each bound graph holds one crate-private record
node. It is a persisted memory schema, written as an ordinary graph node: no
engine format change, no new frame, no public name.

### 1.1 The record

| Item | Value |
|---|---|
| Node key | `binding:v1` (exactly one per graph) |
| Label | `VliteMemoryBinding` (the only label) |
| Expiry | none |
| Properties | all `String`, each at most 4,096 bytes, exactly the set below |

| Property | Value |
|---|---|
| `record_schema` | `memory-binding-record:v1` |
| `tenant` | `@root` or a 32-character lowercase hexadecimal tenant ID |
| `binding` | the binding name (`^[a-z][a-z0-9_-]{0,62}$`) |
| `generation` | canonical decimal, 1 to 2^63-1 |
| `state` | `active` or `retired` |
| `retired_at_ms` | canonical decimal at least 1; present exactly when `state` is `retired` |
| `graph` | the file's graph name (not the tenant-qualified stored name) |
| `evidence` | the file's evidence collection name; absent when the binding has none |
| `memory_schema` | `memory-schema-v1` |
| `ranking_version` | `memory-ranking-v1` |
| `request_digest_version` | `memory-request-v1` |
| `receipt_window_ms` | canonical decimal, 1 to 2,592,000,000 |
| `grants_digest` | `v1:` followed by 64 lowercase hexadecimal characters |

A canonical decimal has digits only, no sign, no leading zero and no whitespace.
The record stores a digest of the grants, not the grants: they can exceed the
4,096-byte string limit, and rotation rule 2 needs only to detect a change.

`grants_digest` is SHA-256 over this byte string (all integers big-endian):

```text
"vlite.memory.binding.grants.v1\0"
u32 grant count
per grant, in file order:
    u32 principal length, principal bytes
    u8 raw_access (0 or 1)
    u32 scope count
    per scope, in file order: u32 length, the scope encoding
```

The scope encoding is the one the scope node already hashes: `scope:v1`, then
for tenant, agent, user and session in that order `00` when absent, otherwise
`01`, the u32 length and the UTF-8 bytes. Grants compare as written, so
reordering them is a change (mapping rule 2).

**Strict decoder.** Anything that is not exactly this shape is corrupt: a
different label set, an expiry, an unknown, missing or mistyped property, an
unknown schema or version string, a non-canonical number, `retired_at_ms` on an
active record or missing on a retired one, a name that fails the file's grammar,
or a record whose stored graph name (the tenant-qualified `graph`) is not the
graph that holds it. A corrupt record is never repaired, and startup refuses it
with a fixed message that names nothing. A node with key `binding:v1` and any
other label is not a record; it is ordinary data (see 1.3).

### 1.2 Startup order

`prepare` (server startup, before any listener) runs these steps and stops at
the first failure. Every message is fixed and names no binding, graph or tenant.

1. Parse the file (unchanged). Check that graph support exists and that every
   named graph and evidence collection exists (unchanged).
2. Collect the previous state: for every live graph (at most 64), read the
   record `binding:v1` and decode it strictly.
3. When the file names at least one binding, run the rotation rules against that
   previous state at the current clock: window, graph and evidence names never
   change (rule 1); a changed entry advances its generation and an unchanged one
   keeps it (rule 2); `retired` never returns to `active` and `retired_at_ms`
   never moves or precedes the change (rule 3); only a retired entry whose
   window has ended may disappear (rule 5); the storage of a removed entry is
   not reused (rule 6). A record in a graph the file does not name is a removed
   entry. Any violation refuses startup.
4. Adopt each named binding's graph (1.3).
5. On a writable primary only: write the record of every named binding (active
   or retired) whose stored record is absent or differs, under the graph's own
   write, then provision the granted scopes of active bindings (unchanged).
   An unchanged record is not rewritten, so a restart with the same file adds no
   commit. Records of removed entries are never touched.

A read-only database or a replica runs steps 1 to 4, writes nothing, and keeps
using the file where a record is still missing.

### 1.3 Adoption (replaces the `VliteMemoryScope` marker rule)

A named binding adopts its graph in exactly three cases:

- **(a) Fresh.** The graph has no live node and the evidence collection has no
  record.
- **(b) Recorded.** A strictly decoded record exists for the same tenant and
  binding, and step 3 passed.
- **(c) Legacy (one time, for graphs adopted by 0.7.0).** There is no record and
  the graph has at least one node, and every node has exactly one label and a
  key prefix that belong together: `VliteMemory` with `mem:v1:`,
  `VliteMemoryScope` with `scope:v1:`, `VliteMemoryReceipt` or
  `VliteMemoryErasureJob` with `receipt:v1:`, `VliteMemoryReceiptOutcome` with
  `receipt-outcome:v1:`, `VliteMemoryContent` with `content:v1:`. The scan pages
  through the whole graph at startup. A writable primary then writes the record.
  The evidence collection is not inspected: 0.7.0 let an operator load evidence
  before attaching it, and refusing that on upgrade would strand valid data.
  This deliberately differs from the first design report, which also asked that
  the collection hold only attached records (owner-approved). The tests
  `raw_loaded_evidence_does_not_stop_a_legacy_upgrade` (core) and
  `a_graph_adopted_before_records_existed_is_adopted_once_and_gets_its_record`
  (integration) pin it.

Anything else is refused as holding ordinary data. 0.7.0 adopted any graph that
held one scope node, so a graph that a `raw_access` holder wrote ordinary nodes
into stops the upgrade until those nodes are removed (1.6).

**Residual risk.** A forgery that consists only of well-formed memory nodes, or
a graph that holds a forged record (a record is an ordinary node, and (b) does
not scan the graph), is indistinguishable from memory. The raw-route guard
(section 2 of this document, slice S3) stops new network writers from planting
either; the slices ship together, and a writer that bypasses the route (the
Rust API, the file tools) is the operator.

### 1.4 No bindings in the file (D1, approved)

When the file names no binding (or there is no file), startup still reads the
records of step 2. A graph that holds a record, and the evidence collection the
record names, stay reserved from the raw routes **with no grants**: nobody
reaches them raw and no memory route is served. Nothing is written, nothing is
refused except a corrupt record, and the aggregate refusals that apply while any
binding is configured (gRPC, import, replication export, automatic maintenance)
apply too, because they protect the same reservation. The same reservation holds
for a record-bearing graph a non-empty file no longer names (after a valid
removal). To reuse such a graph, name its entry again as retired with a raw
grant, empty the graph through the raw routes and drop it; its record goes with
its last node.

### 1.5 Per-write recheck

Inside the write closure that every memory mutation already runs (remember,
consolidate and forget, under the database write lock and after the writable
check), the handler reads `binding:v1` and compares its tenant, binding and
generation with the binding the request resolved. A missing, corrupt or
different record answers 409 `memory_capability_unavailable` with
`details.feature` `binding_generation` and writes nothing. The recheck is a read,
so no operation is added to a batch and the 4,096-operation boundary does not
move. Reads (recall, receipts) do not recheck.

### 1.6 Compatibility

- **Files.** No engine change. A graph that never held memory is untouched. A
  graph adopted by 0.7.0 gains the record at its first 0.7.1 start (legacy path).
- **Rollback to 0.7.0.** 0.7.0 ignores the unknown node: memory nodes are found
  by label and scope nodes by key, and only its adoption check looks at the whole
  graph. That check accepts any graph with a scope node. A bound graph that never
  received a scope (a binding whose grants are empty, or one that was retired
  before it had any) holds only the record, and 0.7.0 refuses it as holding
  ordinary data; empty that graph or remove the record node first. The release
  notes carry this.
- **Upgrade.** An entry that is already retired in the file and has no record is
  "first seen retired": its `retired_at_ms` must not precede the start, as rule 3
  says, so move it to the current time. A file whose active entry changed its
  receipt window, names or grants (without a higher generation) since the record
  was written is refused. A removed active entry is refused until it is retired
  and its window has ended. A bound graph that holds anything besides memory
  nodes (ordinary nodes that a `raw_access` holder wrote under 0.7.0) is
  refused as holding ordinary data: start once without the file, remove those
  nodes through the raw routes, and start again. The operator text is in
  API.md, "Upgrading from 0.7.0".
- **Cost.** One point read per live graph at startup, one more commit per newly
  bound graph, one point read per memory write. A full scan only on the legacy
  path.

### 1.7 Tests and campaigns

Compatibility first: a golden record (literal key, label and properties, and a
`grants_digest` computed outside the Rust code from the byte layout above), a
strict-decoder table (every malformed shape above), round trip, and the
memory operations run unchanged on a graph that holds a record. Then startup
cases for each rotation rule, no-op restart, read-only and replica, legacy
adoption, forged marker, record-only graph, no-file reservation, corrupt record,
and the recheck (including a change made while the request waits for the write
lock, and the exact 4,096-operation frame boundary). Mutants: at least 24 per
route family touched, killed by those tests.

## 2. Raw reserved-name guard: F5, residual risk of 1.3 (slice S3)

**Problem.** The adoption rule of 1.3 accepts a graph that holds only well-formed
memory nodes. Any holder of the raw graph `write` grant can plant such nodes in
a graph nobody has bound yet (a node labelled `VliteMemoryScope` under
`scope:v1:...`, a forged `mem:v1:...` memory), and an operator who later binds
that graph adopts the forgery as the agent's memory.

**Decision (D2, approved): the guard covers every graph.** The raw write route
refuses to create or change a node that looks like memory, on a bound graph and
on an unbound one alike. The memory core writes through its own code, not
through this route, so it is unaffected.

### 2.1 The rule

A raw graph write batch is refused as a whole, before any operation runs, when
any of its operations is:

- `create_node` or `upsert_node` with a reserved key or a reserved label;
- `patch_node` of a node with a reserved key, or one that adds a reserved label.

A **reserved label** is any label that starts with `VliteMemory` (case
sensitive). A **reserved key** is a node key that starts with `scope:v1:`,
`mem:v1:`, `receipt:v1:`, `receipt-outcome:v1:` or `content:v1:`, or is exactly
`binding:v1`. Nothing else changes: `delete_node`, `set_node_expiry`, edges,
attachments, vectors, schemas, and the removal of a reserved label with
`patch_node` stay allowed, so a node that was planted before 0.7.1 can still be
cleaned up through the raw route (a bound graph still needs a raw grant).

The answer is 422 `invalid_input` with the fixed message "a node key or label is
reserved for agent memory". It never echoes the key or the label, and the batch
writes nothing. The check runs on the request alone, after the credential's
graph scope and before the graph is looked up, so the answer is the same for a
graph that does not exist, one that is reserved and one that is not: it reveals
nothing about reservations. An ordinary batch to a reserved graph still answers
409 `graph_not_found` as before.

### 2.2 Scope and limits

- The only network route that writes graph nodes is `POST
  /v1/graphs/{graph}/batch` (REST). The gRPC service has no graph write. The
  direct file tools (`vlite-graph-cli`, `vlite-mcp` graph tools) open the file as
  its operator and are not guarded.
- Edge keys (`content-edge:v1:`, `reference:v1:`) are not reserved: adoption
  reads nodes only, and an edge needs its two nodes to exist already.
- Nodes planted before 0.7.1 are not found by the guard. A planted node that is
  not a well-formed memory node still stops adoption (1.3); a forgery made only
  of well-formed memory nodes is indistinguishable from memory. Check a graph
  before binding it.
- **Upgrade note.** A raw caller that wrote a label starting with `VliteMemory`
  or a key with one of these prefixes now gets 422. This is the one visible
  change of the slice.

## 3. Recall distance: cosine for `exact` and `mmr` (slice S4)

**Problem.** 0.7.0 measures every recall with the squared Euclidean distance of
the query, the default metric of the hybrid query. `exact` orders its candidates
by it, and every selection reports it as `scores.distance`, including `mmr`,
which selects by cosine internally and which the serving contract (section 4)
says retains cosine distance. For vectors that are not unit length the order
differs from a cosine ranking. Three wire goldens that expect cosine values were
left unreplayed: `recall-vector-exact`, `recall-vector-mmr` and
`recall-mmr-diversity-not-weighted-score-order`.

**Decision (D4, approved).** `exact` and `mmr` use the cosine distance, and
report it as `scores.distance`. `weighted`, the default selection, is unchanged:
it keeps the query's own metric (squared Euclidean), so no existing ranking
changes.

### 3.1 The rule

- The dense leg of an `exact` or `mmr` recall is enumerated by cosine distance
  `1 - cos(q, v)`, computed in f64 from the f32 operands (0 for the same
  direction, 1 for orthogonal, 2 for opposite). Ties keep the exact reranker's
  order: distance, then record ID. The `mmr` candidate pool (the best
  `candidates` by dense distance) is therefore a cosine pool, and `mmr` reports
  the value its own selection uses.
- A query vector with zero norm has no direction: 422 `invalid_input`, before
  any record is read. A stored vector with zero norm has no cosine distance and
  is never a candidate of `exact` or `mmr`. (0.7.0 accepted a zero query under
  `exact`; `mmr` already refused it.) The oracle leaves this convention open
  (OPEN-G1W-10); the rule above is the one the engine's cosine metric already
  has.
- Nothing else changes: selection names, refusals, work charges (`meta`), ranks.

### 3.2 Compatibility

A visible change for `exact` and `mmr` callers whose vectors are not unit
length: the order of `exact` hits and every `scores.distance` of both selections
change. For unit-length vectors the order is the same and the new distance is
half the old one. A caller that keeps a threshold on `scores.distance` must
recalibrate. API.md carries the upgrade note; the release notes repeat it.

### 3.3 Goldens (D5)

The three goldens describe their candidates by numeric record IDs and vectors
the server cannot be seeded with: the ID of an evidence record is derived from
its evidence ID (the first 8 bytes of the SHA-256 of the ID, big-endian). They
are regenerated: each candidate names its `evidence_id` and the record ID that
follows from it, and the independent oracle (`tests/memory_wire_oracle.py`)
emits the evidence list and the raw scores. The replay harness seeds each such
candidate as a memory with an attached evidence record carrying the vector, and
checks that the record ID it got is the one the golden names. Cosine values,
orders and the MMR objective stay the oracle's, not the server's.

### 3.4 Tests and campaigns

Contract first: the probe of the first design report (A = [10, 0],
B = [0.6, 0.6], query [1, 0]: `exact` answers A, B with distances 0 and
1 - 1/sqrt(2); `mmr` with lambda 1 answers A, B), cosine values for orthogonal
and opposite vectors, the zero-norm query, a stored zero vector, the MMR pool
chosen by cosine when `depth` is smaller than the collection, and the three
goldens replayed (the skip list shrinks by three). Mutants: at least 24 for the
recall route family, covering the metric of each selection, the pool, the
reported value, the zero-norm rules and the golden harness.

## 4. `exact` with text: F-03 (slice S5)

**Problem.** 0.7.0 refuses a request that carries `text` under `exact` or
`mmr` (422 `memory_unsupported_ranking`, F-03). The serving contract (section 4)
treats the two differently. `exact` "recomputes vector-candidate distances,
reorders their dense leg before fusion/weighting, preserving text-only
candidates", so a hybrid `exact` request is defined. `mmr` is "a separate
vector-only cosine selection" that rejects text, so the `mmr` refusal is the
contract and stays. Only the `exact` refusal is a gap.

**Decision.** Lift the refusal for `exact`, for text only. `mmr` keeps refusing
text. `exact` keeps refusing a missing vector, entity seeds, expansion, weights
other than vector-only and a half-life: the contract does not forbid them for
`exact`, but each needs its own tests, and the report of this lane keeps them
for a later slice. Approximate mode is not part of this slice (section 4.3).

### 4.1 The rule

- `exact` with `vector` and `text`: the dense leg is the cosine enumeration of
  section 3, the text leg is the BM25 leg of weighted recall, and the two are
  fused by weighted reciprocal rank fusion exactly as in weighted recall
  (`rrf_k` 60, weights 1 and 1 in the REST adapter). The fused order is the
  selection order and `R` is `1 / position` as for every selection. In other
  words `exact` is a weighted recall whose dense leg is measured by cosine.
- A candidate found by the text leg alone (for example a record whose vector is
  zero) is kept. It carries no `distance` and no `dense_rank`, its `text_rank`
  and `bm25`. A vector candidate carries its cosine distance and its
  `dense_rank`; a candidate of both legs carries both ranks.
- The work is charged as in weighted recall: both legs charge the candidates
  they examine, and `meta.mode` stays `exact_enumeration`.
- `exact` without text is unchanged: the fused list of one dense leg is that
  leg, so the candidates, ranks and distances are the ones of section 3.

### 4.2 Compatibility

A request 0.7.0 refused now succeeds, and no request that 0.7.0 served changes
its answer because of this slice. Recall is a read: no receipt, record or
format changes.

### 4.3 Approximate mode

`mode: approximate` keeps answering 422 `memory_bounded_plan_unsupported`. It
needs an approximate index that a binding owns and that is ready, and a way to
prefilter an approximate search by the visible owners (the reason the core
refuses an approximate vector leg). Both are engine work outside this lane, so
the refusal and the contract's "no lazy index build" rule stand.

### 4.4 Tests and campaigns

Contract first, with the reciprocal-rank arithmetic worked by hand and no
ties: four memories (vectors [10, 0], [0.6, 0.6], [0, 1] and the zero vector)
whose documents carry the term "lore" three, zero, two and one times answer
pa, pc, pb, pz, the zero vector as the text-only hit without a distance; a
probe in which a small text weight lets cosine and squared Euclidean order the
fused list differently; the refusals that remain (`mmr` with text; a missing
vector, seeds, expansion and other weights for both selections). Mutants: at
least 24 for the recall route family, covering the validity rule for text, the
selection block, the fusion of the two legs and the adapter's refusal.

## 5. Evidence references on items (slice S6)

**Problem.** The REST adapter refuses every nonempty `evidence` list of an item
(409 `memory_capability_unavailable`, feature `evidence`). The contract
(`MemoryItem.evidence`, a list of `MemoryEvidenceRef`) names a record of the
binding's evidence collection by `{collection, record}`. The core already
attaches a record to a memory, but it names the record by an evidence ID string
whose SHA-256 gives the record number, so a number from the wire has no ID.

**Decision (D6, with one deviation).** Accept an item's references to a record
that already exists in the binding's evidence collection. The deviation from
D6: no new digest version. The core's request digest, `memory-request-v1`,
already hashes each evidence ID and its scope. A reference becomes the reserved
evidence ID `record:<decimal>`; the digest hashes it like any other ID, so no
existing digest changes and no receipt carries a second version. The rest of
the core (ownership checks, attachment, recall, forget) keeps working on IDs
and derives the record number through one function, which the reserved form
short-circuits. Creating records inside a remember request (the request-level
`evidence` list) is a different feature and stays refused.

### 5.1 The wire form

- `evidence: [{"collection": "<binding name>", "record": <MemoryU64>}]` on an
  item of `remember`. The collection is the alias of the binding, which is its
  name, as `recall` already reports it. `record` is a canonical decimal string
  or an exact JSON integer in the `u64` range.
- At most one reference per item: the engine attaches at most one record to a
  memory. A longer list, an unknown or missing field and a record that is not a
  `u64` are 422 `invalid_input` with `details.field` `evidence`; a collection
  that is not the binding's name is the single 403 of 5.2. An empty list is the
  same as none (unchanged).
- A binding without an evidence collection keeps refusing a nonempty list
  (409 `memory_capability_unavailable`, feature `evidence`), and so does the
  request-level `evidence` list on every binding. The `evidence` of a
  consolidate output stays refused.

### 5.2 Authorization

A reference claims a record: the record exists, no memory owns it, and the
attachment makes it the memory's evidence, which recall then returns. A record
that no memory owns belongs to no scope, so a principal that reaches the
collection only through memory routes must not be able to claim a record by
guessing its number. The route therefore requires that the principal's binding
grant carries `raw_access`, the capability that already reads and writes the
collection directly; the reference then adds no read access. Without it the
answer is the single 403 body, before the record is looked at, so the answer
does not tell whether a record exists. A `collection` that is not the binding's
name gets that same body (it is compared before any record is looked at). A
record that is missing, hidden, or already held by a memory of any scope is one
409 `memory_attachment_conflict` with one body, so a refusal does not say which
or who holds it. The credential still needs the write grant on the evidence
collection that `remember` requires and the exact scope grant.

### 5.3 The core

- `evidence_record_id` returns the number of a reserved ID and hashes every
  other ID, as before. A reserved ID is `record:` and the canonical decimal of a
  `u64` (no sign, no leading zero, no overflow): `record:7` and `record:07`
  cannot name one record. Anything else with the prefix is `InvalidEvidence`
  (422 `invalid_input`), and so is a consolidate output whose evidence ID starts
  with it. The REST remember adapter, from an item's wire reference, is the only
  producer of a reserved ID: the request-level `evidence` list stays refused and
  no other path writes one.
- The node keeps its evidence IDs in the existing `evidence` property, one
  token per ID. An ordinary ID is stored as hex, as before; a reserved ID is
  stored as itself, `record:7`, never as hex. A stored token that starts with the
  prefix and is not canonical is `InvalidBundle`, as a token that is not hex is.
- The attachment checks answer once for an ID that names a record: a record
  that is missing, has an expiry, is held by another memory of the same scope or
  is held by a memory of another scope is `RecordAlreadyAttached` (409
  `memory_attachment_conflict`), and nothing is written. An ordinary (hashed) ID
  keeps its checks: a record held in another scope is `CrossScope` (422
  `invalid_input`). Only an ID that names a number is a claim whose owner must
  not be disclosed.
- Recall already reports `{collection, record, snippet}` for an attached
  record. Forget detaches the record and deletes it unless another retained
  memory uses it, as for every evidence record, so a referenced record belongs
  to the memory from the attachment on; consolidate is unchanged.

### 5.4 Replay and compatibility

The digest of a request that carries a reference hashes the reserved ID, so an
exact retry replays, a retry that names another record is 409
`idempotency_conflict`, and a request without references hashes as before. The
stored memory keeps its ID in the existing `evidence` property: no file format
changes for files that never use a reference, and nothing writes a reserved ID
until a client sends an item reference.

**Downgrade.** A file that holds such a memory is a one-way change, and the
encoding in 5.3 is chosen so that 0.7.0 refuses it loudly. A 0.7.0 server
decodes the `evidence` tokens of every memory node of a scope as hex, so it
cannot read the literal token. Hex-encoding the reserved ID like the others
would be worse: 0.7.0 would read it as an ordinary ID, hash it to a record that
does not exist and silently report the memory without its evidence, and its
forget would refuse with 409. Measured with the real binaries (a 0.7.1 build
wrote one memory with a reference, a v0.7.0 server then opened the same file and
bindings file):

| 0.7.0 operation | Answer |
|---|---|
| recall by seed, recall by vector, new `remember`, `forget`, `consolidate`, in the scope holding the memory | 500 `internal_error` |
| exact replay of a `remember` that carried no reference | 200 `replayed` |
| replay of the request that carried the reference | 409 `memory_capability_unavailable`, feature `evidence` |
| recall and `remember` in any other scope | 200 |
| raw read of the evidence record | 200, intact |
| recall by seed, recall by vector and new `remember` in the scope, after a 0.7.1 server forgot the memory | 200 |

The server stays up and writes nothing wrong; the failure is explicit but it
covers every memory operation of that one scope, not only the memory with the
reference. To recover, run 0.7.1 or later and `forget` the memory: that deletes
the node and the referenced record (no token is left behind), and 0.7.0 then
serves the scope again. The documents say so next to the `raw_access`
definition, which is the explicit opt-in for a reference, and in the upgrade
notes (API.md, SECURITY.md). `scripts/memory_evidence_downgrade_check.py
--new-bin PATH --old-bin PATH` repeats the measurement with two server binaries,
asserts every row above and exits 1 on a difference.

### 5.5 Tests and campaigns

Contract first: the reserved-ID rules (canonical forms only, no overflow, the
ordinary IDs still hash to their pinned values); a reference attaches an
existing record and recall reports it; a missing record, a record of another
memory and a record of another scope are refused without a write; the digest of
a request with a reference derived independently (the contract's canonical JSON
text, hashed outside the engine); an exact replay and a changed record; the
wire refusals of 5.1 and the 403 of 5.2. Mutants: at least 24 for the remember
route family, covering the alias, the record form, the limit of one, the
authorization, the refusals that remain and the core rules.
