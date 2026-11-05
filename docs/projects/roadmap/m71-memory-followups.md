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
