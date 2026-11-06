# Key-value batch and get-many: atomicity review and contract

| Status | Decisions D1 to D5 approved on 2026-11-05 (the owner's delegate). Implementation is in progress; nothing is activated or released. |
|---|---|
| Slice | F2 phase 1, key-value v2 |
| Amends | Decision (p) of the [KV and TTL serving contract](serving-kv-ttl-contract.md#12-delivery-slices-ledger-and-owner-decisions) ("omit; a future batch needs independent atomicity and schema review"). This document is that review. |
| Scope | An atomic multi-key write batch (put and delete with the existing preconditions) and a multi-key read, on the Rust API, REST, gRPC, the MCP and shell tools and the clients |
| Not in scope | Leases (phase 2), delete by prefix (decision (p)), batches across collections, check-only operations, any file format change, any benchmark or size claim |

Everything is additive. A 0.7.1 client, file or server keeps working, and a
file that a batch wrote opens in 0.7.1 (section 2.6).

## 1. What a batch is

A batch names one collection and a list of operations. Each operation is a
put or a delete of one `(namespace, key)` with the same `expected` condition
a single put or delete takes (`any`, `absent` or a revision). The operations
may use different namespaces of the collection. A batch either commits every
operation or none, and a reader sees the state before it or the state after
it.

## 2. Atomicity review

### 2.1 What is atomic

The unit is one collection. The batch becomes one `Mutation` list handed to
the engine's batch commit (`Database::write_batch_in`), which validates every
operation, appends one checksummed op-3 frame, syncs it once under
`--durability full`, and changes memory only after the commit succeeded. The
writer owns the database exclusively (`&mut Database` in Rust, the owner write
guard in the server), so no other write can fall between the checks and the
commit and no reader can see part of the batch. No new frame type, header flag
or format version is needed.

### 2.2 Semantics: sequential order against pending writes

A batch behaves as if its operations ran one at a time, in order, against the
live state, with one expiry clock for the whole batch, and the result were then
committed once. Two rules make that true:

- **Distinct keys.** A batch names each `(namespace, key)` at most once;
  otherwise 422 `invalid_input`. Every put of one commit gets the same version
  (section 2.4), so a second operation on a key could not name the revision of
  the first. etcd and DynamoDB transactions refuse the same thing.
- **A pending-write overlay for slots.** A key may occupy any of 16
  consecutive record IDs, and `put` takes the first free or expired one.
  Probing the live state alone would let two new keys with overlapping windows
  choose the same free slot (the later put silently replaces the earlier
  entry), and would let a key find its own expired leftover in a slot that an
  earlier operation of the batch has just reused. Each probe therefore reads
  the batch's own pending writes first: a slot written by an earlier
  operation is occupied by that key, and a slot freed by an earlier delete is
  free. The outcome equals running the same operations one by one on a copy.

Preconditions are judged per key against the state before the batch. Because
keys are distinct, no operation can change another key's live version, so this
equals judging them in order. A delete of a key that has no live entry
succeeds with `deleted: false`. A delete of a key whose entry is due removes
that expired record, like a single delete, and also reports `false`. A batch
that changes no record (every operation a delete that finds nothing) commits
nothing and writes no frame.

Every check that needs no state is made first, over the whole request: the
operation count, then each operation in order for its namespace and key, the
absence of a deadline (a Rust caller only: no other surface can send one), the
size of its value, the running total of value bytes and whether an earlier
operation names the same key. An invalid request is refused with the error of
the first operation that fails a check, whatever the state of the collection.
That holds once the collection is open: in Rust, opening the store comes first
(`Database::kv_store` creates a missing collection and fails with
`NotKvCollection` for another kind), while REST and gRPC check the request
before they open the collection. Then the operations run in order against the
state, and the first
that cannot run fails the whole batch and nothing is written: a failed
condition (`precondition_failed`), 16 occupied slots (`kv_slots_exhausted`) or
an attached record (`graph_attached_record`).

### 2.3 Crash behavior

The batch is one frame, and a frame is the engine's unit of recovery. After a
crash `open` either finds the whole frame or finds an incomplete final frame
and returns `IncompleteTail`; the explicit repair truncates only that frame.
No state holds a prefix of a batch. Under `periodic` an OS crash can lose
whole recent frames, as for a single put. A group-commit window shares one
sync, and each batch remains one frame. A failed write or sync is
`commit_indeterminate` (503, `UNKNOWN`) and poisons the handle: the batch is
all or nothing, but which is not known. Read the keys back with get-many
before deciding anything, and do not retry blindly.

### 2.4 Versions and expiry

- **Versions.** The version of a put is the commit sequence, as for a single
  put, so all puts of one batch share one version and each is a valid
  compare-and-set token for its key. A later write to a key gets a larger
  version. Compaction, upgrade and restore still rewrite revisions.
- **Expiry.** The batch frame has no per-entry deadline. A deadline needs a new
  entry kind in the frame: a format change (decision D1). In this release a
  batch put has no `expires_at_ms`, and like a put without one it clears the
  deadline of the entry it replaces. Due entries still behave as absent: they
  satisfy `absent`, never match a revision, are reused as free slots and are
  removed as leftovers by a delete.

### 2.5 Errors, limits and accounting

No new engine error variant is needed. Existing codes map as for the single
operations: 412 `precondition_failed` (the message stays `revision
precondition failed`, so a failed condition tells a caller that may write but
not read no more than it does for a single put, apart from the difference
described below), 409 `kv_slots_exhausted`, `not_kv_collection`,
`upgrade_required` and `graph_attached_record`, 422 `invalid_input`
(bounds, duplicate keys, too many operations or bytes; the message names the
operation), 403 `read_only` and `forbidden_scope`, 404 `collection_not_found`,
429 `rate_limited` and `quota_exceeded`, 503 `commit_indeterminate`.

One difference from a single put remains, and it widens an oracle that a single
put already has. A refused request writes nothing, so a caller that may write
but not read can test a condition on an entry without changing it, whenever it
can make the request fail in a way that differs from the failed condition: the
answer is 412 when the condition (`absent` or a guessed revision) does not hold
and the other failure when it does. A single put does this at a live-record
quota (429 `quota_exceeded`), and a single delete on a record that is attached
to a graph node (409 `graph_attached_record`). A batch widens the class,
because it can add the failure on any other key after the operation under
test: a put of a key whose 16 record IDs the caller has filled (409
`kv_slots_exhausted`), or either failure above. The first operation that cannot
run decides the answer (`plan` in `src/kv/batch.rs`), and a quota or
attached-record refusal comes only after every condition held. That learns only
whether a key is live and whether a guessed revision (a commit sequence) is its
current one, never its value, for a key that the caller may already overwrite.
Each 409 `kv_slots_exhausted` counts in `kv_slots_exhausted_total`.

A batch that changes at least one record on a file of format 1 is refused with
409 `upgrade_required` (engine error `BatchRequiresUpgrade`), although single
puts work there, because format 1 predates atomic batches: compact the closed
file to a new path, as the error message says. A batch that changes no record,
and an empty batch, still succeed.

| Bound | Value | Why |
|---|---|---|
| Operations per batch | 1,000 | Short owner-guard time and bounded memory; the engine allows 4,096 |
| Value bytes per batch | 4,194,304 | Keeps the frame near 5.5 MB with the largest keys and namespaces; the engine allows 16 MiB; fits the 8 MiB gRPC request, which carries raw bytes; fits the 8 MiB REST body as base64 only while the keys and namespaces stay plain text, because a JSON encoder writes a control character in up to 6 bytes, the Python client writes every non-ASCII character as a 6-byte escape (12 bytes outside the Basic Multilingual Plane), the .NET client does the same and also escapes ASCII characters such as `<`, `>`, `&`, `'`, `+` and `"`, and the Go client escapes `<`, `>`, `&`, U+2028 and U+2029, so a batch at every limit at once (1,000 operations with 1,024-byte keys and 4,194,304 value bytes) can pass the body limit and is then refused with 413 `invalid_json` |
| Keys per get-many | 1,000 | Same count bound |
| Get-many response | 16,777,216 bytes of compact JSON | The page cap of a list |
| Namespace, key, value | 255, 1,024, 1,047,290 bytes | Unchanged |

Raising a bound of the batch (operations, value bytes, keys per get-many) later
is compatible; lowering one is not. The shipped clients check these bounds
before they send, so a client keeps refusing a larger batch until it is
updated, and a release that raises a bound must ship the client change with it.
The bounds of one namespace, key or value are not the batch's to change: the
clients also check the entries that a response carries against them, so raising
one needs a client release first. Every surface uses the same bounds (the 16 MiB shell line and
the 128 MiB MCP line sit far above them). The live-record quota is judged once,
under the same guard as the commit, on the count that the whole batch leaves
(the net change of all its slot changes: the last write to a slot wins), and
only when the batch writes at least one record. An empty batch or a batch of
deletes is never refused for quota, like a single delete. A batch refused for
slots is one refused request: `kv_slots_exhausted_total` counts it once. An
empty batch is a successful no-op.

### 2.6 Compatibility and rollback

The frames a batch writes are the ordinary batch frames that every build since
format 2 reads. A batch entry is an ordinary record, so 0.7.1 reads, lists,
updates and deletes batch-written entries. A server without the two new routes
(0.7.1 and earlier) answers 404 `not_found` to the admin token, which the
clients report as an unsupported operation. It answers every other credential
403 (`read_only` for the read-only token, `forbidden_scope` for a scoped
credential), because its permission check runs before it matches a route, and
the clients report that as an ordinary permission error. To find out
beforehand, read `GET /v1/capabilities`, which every credential may call, and
look for the two route paths in its `routes`. Do not test `features.kv_batch`
for this: it is true only for files at format 5 or later, while a format 3 or 4
file serves both routes with the flag false. Over gRPC such a server answers
`UNIMPLEMENTED` to every caller, as for any method of the service that it does
not have, and gRPC has no capabilities call, so a gRPC client asks REST. Nothing
about a batch is one-way.

### 2.7 Growing the batch later

These rules keep a later change to the batch compatible. Nothing in this
subsection is a decision to build any of it.

- A new kind of batch operation, or a new field on an existing operation, ships
  with its own capability flag. An older server refuses it as an unknown field,
  so a client that needs it checks the flag first.
- Over gRPC an older server silently ignores a field of a message that it does
  not know. A per-operation extension therefore needs a new `oneof` case (an
  older server refuses an operation whose `kind` it cannot read) or a
  capability gate that the client checks first.
- A batch across collections and a delete by prefix are new surfaces, never
  per-operation fields. Delete by prefix stays decision (p) of the
  [serving contract](serving-kv-ttl-contract.md#12-delivery-slices-ledger-and-owner-decisions).
- A batch deadline (decision D1) gets its own capability flag, and the flag must
  not be tied to the lease ledger, even where the two share format work: a
  server can offer one without the other.
- **What `features.kv_batch` means.** D2 approved the name, but the flag follows
  the format of the file, not the batch: it is true on a file at format 5 or
  later, like `features.kv`, and false says only that the file is at format 3 or
  4. It does not tell whether the server has the routes, which a format 3 or 4
  file serves with the flag false; the two paths in `routes` tell that.

## 3. Surfaces

The names and shapes below are approved (D2). Any addition is a new decision.

**Rust.**

```rust
pub enum KvOp<'a> {                       // #[non_exhaustive]
    Put { namespace: &'a str, key: &'a str, value: &'a [u8], options: KvPut },
    Delete { namespace: &'a str, key: &'a str, expected: Expected },
}
pub enum KvOutcome { Put { version: u64 }, Deleted(bool) }   // #[non_exhaustive]
impl KvStore<'_> {
    pub fn write_batch(&mut self, operations: &[KvOp<'_>]) -> Result<Vec<KvOutcome>>;
    pub fn write_batch_at(&mut self, operations: &[KvOp<'_>], now_ms: u64) -> Result<Vec<KvOutcome>>;
}
impl KvView<'_> {
    pub fn get_many(&self, keys: &[(&str, &str)]) -> Result<Vec<Option<KvEntry>>>;
    pub fn get_many_at(&self, keys: &[(&str, &str)], now_ms: u64) -> Result<Vec<Option<KvEntry>>>;
    pub fn visit_many_at<F>(&self, keys: &[(&str, &str)], now_ms: u64, visitor: F) -> Result<()>
    where F: FnMut(usize, Option<KvRef<'_>>) -> ControlFlow<()>;
}
pub const MAX_KV_BATCH_OPERATIONS: usize = 1_000;
pub const MAX_KV_BATCH_VALUE_BYTES: usize = 4_194_304;
pub const MAX_KV_GET_MANY_KEYS: usize = 1_000;
```

A put in a batch whose `options.expires_at_ms` is set is refused with
`InvalidKvKey` in this release (D1). `visit_many_at` lends each entry like
`visit_at`; its `has_next` says that a later requested key follows.

**REST** (new paths; section 2.6 says what a server without them answers):

- `POST /v1/collections/{collection}/kv/batch`, scope `write`:
  `{"operations":[{"op":"put","namespace":"app","key":"a","value_base64":"AAEC","expected":"absent"},{"op":"delete","namespace":"app","key":"b","expected":{"revision":"42"}}]}`
  answers 200 `{"results":[{"version":"43"},{"deleted":true}]}`, one result per
  operation, in order. Unknown or duplicate fields are 422 `invalid_json`.
- `POST /v1/collections/{collection}/kv/entries/get`, scope `read` (the
  read-only token may call it): `{"keys":[{"namespace":"app","key":"a"}]}`
  answers 200 `{"items":[<entry>|null],"next_index":null}`. The items are
  positional (`null` for an absent or due key). The largest prefix of the keys
  whose results fit the response cap is returned with `next_index` set to the
  first key not read; the first key always fits. One read guard and one clock
  serve the whole call. The graph routes use the same `POST …/get` shape.
- `GET /v1/capabilities` gains `features.kv_batch` (true on format 5 and later
  files, like `features.kv`) and the two routes in `routes`, which it lists on
  every format of file. A caller that needs to know whether a server has the
  routes looks for the two paths in `routes`: the flag is not that test,
  because a format 3 or 4 file serves both routes with the flag false.

Wire details that every client and tool follows:

- **Batch request.** `operations` is required and holds 0 to 1,000 objects.
  A `put` takes `namespace` (1 to 255 UTF-8 bytes), `key` (1 to 1,024 bytes),
  `value_base64` (canonical padded standard base64 of at most 1,047,290
  bytes) and an optional `expected`: `"any"` (the default), `"absent"` or
  `{"revision":"<u64 decimal>"}`. A `delete` takes `namespace`, `key` and the
  same `expected`. A put has no `expires_at_ms`: the field is unknown, so 422
  `invalid_json`. The decoded values of all puts total at most 4,194,304
  bytes, and no two operations name the same `(namespace, key)`; either
  violation is 422 `invalid_input`.
- **Order of the checks.** A body that does not parse (an unknown or duplicate
  field and a wrong type included) is refused first as `invalid_json`. Then the
  operation count and the stateless checks of section 2.2 run in order, and the
  first fault is the answer, whatever the state of the collection. Over gRPC the
  adapter converts every operation of the message to JSON before those checks,
  after it has checked the operation count, so an operation whose `kind` is
  unset is refused ahead of them, in the way of a malformed operation object
  over REST: `INVALID_ARGUMENT` with `invalid_input` and a message that starts
  `operations[<index>]: ` and goes on `KV batch operation is required`.
- **Batch result.** `results[i]` answers `operations[i]`: a put gives
  `{"version":"<nonzero u64 decimal>"}` (every put of one batch gives the same
  version) and a delete gives `{"deleted":<bool>}`. An empty batch answers
  `{"results":[]}`.
- **Get-many request.** `keys` is required and holds 0 to 1,000
  `{"namespace","key"}` objects with the bounds above. The same key may
  appear more than once; each position is answered.
- **Get-many result.** `items` has one element for each of the first
  `len(items)` keys, in order: the entry object that `GET …/kv/entry` returns
  (`key`, `value_base64`, `version`, `expires_at_ms`), or `null` for a key that
  is absent or due. `next_index` is `null` when every key was read; otherwise
  it is the index of the first key not read, equal to `len(items)` and at
  least 1. Calls share no snapshot: a later call for the remaining keys sees
  the state at its own time.
- **Errors** are the single-operation errors (`precondition_failed` 412,
  `kv_slots_exhausted`, `not_kv_collection`, `upgrade_required`,
  `graph_attached_record` 409, `invalid_input` and `invalid_json` 422,
  `collection_not_found` 404, 403, 429 and 503 `commit_indeterminate`). A
  server without the routes answers 404 `not_found` to the admin token and 403
  to every other credential (section 2.6). A client reports the 404 as an
  unsupported operation and the 403 as an ordinary permission error, and never
  falls back to single calls.

**gRPC** (`vlite.v1.Kv`, new RPCs and messages, no tag reused):
`rpc GetKvEntries(KvGetManyRequest) returns (KvGetManyResponse)` and
`rpc WriteKvBatch(KvBatchRequest) returns (KvBatchResponse)`, with
`KvKeyRef { namespace, key }`, `KvGetManyItem { KvEntry entry }` (a message
field, unset for a key that is absent or due),
`KvGetManyResponse { repeated KvGetManyItem items; optional uint32 next_index }`,
`KvBatchPut`, `KvBatchDelete`, a `KvBatchOperation` oneof, and a
`KvBatchResult` oneof (`version` or `deleted`).

**MCP and shell.** `vlite_kv_get_many` and `vlite_kv_batch` (listed only with
`--writable`), and the commands `kv-get-many` and `kv-batch`, with the REST
fields plus `collection`.

**Clients.** Python `kv_get_many` and `kv_batch`, JavaScript `kvGetMany` and
`kvBatch`, .NET `KvGetManyAsync` and `KvBatchAsync`, Go `KvGetMany` and
`KvBatch`. Each client follows its existing key-value style: it validates the
bounds before sending, returns `bytes`, `Uint8Array`, `byte[]` or `[]byte`
values and 64-bit versions as `int`, `bigint`, `ulong` or `uint64`, hands the
caller the server's result unchanged (a partial get-many result carries
`next_index`; the client does not continue it for the caller), never retries
and never falls back to single calls. The Go client has no key-value methods
today, so it gains the whole set (get, put, delete, list, get-many and batch).

**Audit and metrics.** A `kv_batch` audit event carries the principal, actor,
collection, outcome and the counts of puts and deletes, never a namespace, key
or value. The metrics are unchanged.

## 4. Decisions

| # | Decision | Options | Recorded choice (2026-11-05) |
|---|---|---|---|
| D1 | A deadline on a batch put | Defer: no `expires_at_ms` in a batch / add a batch entry kind behind a new header flag (a format change: every decoder of batch frames, the change feed, replicas, PITR, maintenance and compaction learn it, and builds before it refuse the file) | Defer. A batch put clears the deadline like a put without `expires_at_ms`; there is no format change. A deadline can arrive additively with the format work that the lease ledger also needs. |
| D2 | The names and shapes in section 3 | As proposed / other | As proposed. |
| D3 | Refuse duplicate keys; bounds of 1,000 operations and 4 MiB of values | Refuse or apply in order / other bounds | Refuse; these bounds. |
| D4 | A get-many that reaches the page cap | Partial result with `next_index` / refuse the request / a smaller key count that always fits | Partial result with `next_index` (the DynamoDB `UnprocessedKeys` precedent); no snapshot across calls. |
| D5 | The Go client | Add get, put, delete, list and the two new calls / the two new calls only | Add the whole set; there is no base to extend. |

## 5. Evidence to be produced

A property test that compares `write_batch` with the same operations run one
at a time on a copy (state, slot choice and first error), using keys that
share one window; frame-level checks that a batch is one frame and that
failures write none; crash tests that cut the frame at every byte; a REST
differential against 0.7.1 for every existing route; and hand mutants for the
overlay, the duplicate and bound checks, the commit and the no-op rule.
