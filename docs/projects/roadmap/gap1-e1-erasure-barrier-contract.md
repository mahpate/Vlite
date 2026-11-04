# Gap 1 E1: exact-scope erasure barrier and job (ratified, provisional)

Status: **RATIFIED by the coordinator on 2026-11-04 (PROVISIONAL: logged in
the decision ledger and revisited at the release go/no-go).** Documentation
and independent Python reference only; not implemented, activated or public.
The Rust sub-slice E1 is authorized against this contract and its oracle. The
job and receipt schemas of sections 3-4 are persisted formats and freeze at
the 0.6.2 release; they are flagged for the go/no-go.
Base `136d72c5a94e1c0d1878f99c461a555a91b7c3f6`.
Authority: [C2 remainder](gap1-c2-remainder-contract.md) section 6 E1,
decisions 4-12 and section 9, subordinate to
[Gap 1 section 5](gap1-agent-memory.md#5-forget-retention-and-erasure).
The word "exact" below specifies this proposed private semantic version, not
a new engine encoding, public API or serving schema. Ratification freezes
sections 1-8 together; changes before implementation need a new ratification.

## 1. Boundary, identity and isolation

E1 begins logical erasure of precisely one already provisioned scope. It creates
a persistent barrier and pending job; **it deletes no live memory or evidence**.
The successful graph frame can still perform the engine's opening reclamation
of unrelated TTL-expired graph nodes; it is not E1 erasure progress.
No automatic transition to complete, no implicit job from an oversized forget.
The scope node is neither replaced nor given a TTL. Job-node presence is the
barrier, including after eventual logical completion. No automatic reactivation.
An entity target, subtree target, wildcard and array of scopes are not E1 inputs.

A scope is `(tenant,agent,user?,session?)`. Tenant/agent are required; session
requires user. Each present component is 1..256 UTF-8 bytes. Missing optional
and explicit None are identical. There is no Unicode normalization, case folding,
string-prefix ancestry or alias resolution. A session is this complete tuple,
not an independent globally named session identifier.

Define `child(P,Q)` strictly: equal tenant/agent, every optional component present
in P is equal in Q, and Q has at least one optional component absent in P.
Consequently a tenant/agent scope can have user and session descendants; a user
scope can have session descendants; a session scope has no children. Siblings
are distinct scopes with the same immediate parent (remove session, else user).
This definition describes tests only: **the barrier never walks this relation**.
Exact tuple equality alone matches. Distinct tuples have distinct framed bytes
(presence and lengths make decoding unique); SHA collisions do not authorize
aliasing: stored tuple mismatch fails DigestCollision. Thus no child, ancestor,
sibling, other agent or tenant is barred by a job for P.
Copies in another scope need separate authorization, as Gap 1 section 5 requires.

## 2. Private types and validation

Proposed new private types:

```text
EraseScopeRequest {
  scope: MemoryScope, idempotency_key: String,
  expected_scope_revision: u64, now_ms: u64,
  receipt_window_ms: u64, deadline_ms: Option<u64>
}
ErasureStatusRequest { scope: MemoryScope }
ErasureStatus {
  job_key: String, revision: u64, generation: u64,
  state: Pending, logical_visibility: Hidden,
  deleted_count: u64, remaining: Option<u64>, resume_token: Option<String>
}
```

In E1 every status has generation=1, state=pending, logical_visibility=hidden,
deleted_count=0, remaining=None, resume_token=None. Unknown remainder is not zero.
No scanner runs to compute it. No usable continuation token is issued until E2
is frozen and implemented. An E1 pending status does not promise a working resume.
Revision is an actual graph-node revision, never a caller memory revision.
`erasure_status` returns the current job revision; replay returns the original
revision from its receipt, whose initial frame also created the job.

`expected_scope_revision` is an identity guard: the provisioned scope node's
revision is the provisioning-frame revision and is immutable in E1. Because
`provision_scope` returns `()`, add crate-private read-only
`MemoryRef::scope_revision(&MemoryScope) -> MemoryResult<u64>` in erasure.rs.
It reads the scope node, verifies its exact tuple, and returns its graph revision.
It is barrier-exempt so a caller can inspect the stable identity guard after a
job begins. It never mutates the scope node or proves job progress.

New crate-private MemoryError variants: DeletionPending and JobConflict.
The latter means a different/expired begin cannot start a second job for a scope.
Reuse InvalidTarget for zero/overflow expected_scope_revision, RevisionConflict
for a fresh mismatched scope revision, MissingMemory for status with no job,
and existing InvalidScope/InvalidKey/InvalidClock/InvalidReceiptWindow/
DeadlineExceeded/InvalidBundle/DigestCollision/IdempotencyConflict/
OperationLimit/BodyLimit and Storage mappings. No public Error changes.
JobConflict anticipates E2 fencing but E1 does not implement a resume operation.

Begin structural order: scope; opaque key (1..256 UTF-8 bytes); now_ms in
0..i64::MAX; positive receipt window with checked u64 now+window;
nonzero u64 expected_scope_revision; optional u64 deadline; binding validation.
E1 requires a present collection name to be 1..63 UTF-8 bytes, matching the
engine collection-name bound (`src/format.rs:125,570`); None is legal.
Deadline, now and numeric booleans are not interchangeable. No silently accepted
extra request fields, mode/target discriminants or caller graph properties.
Status request validates only scope, not clock/window/key.

Then read/validate the existing provisioned scope node and tuple. Missing scope
is InvalidScope; tuple mismatch is DigestCollision. Do not mask an existing
tuple collision with a barrier. Scope lookup may validate more existing schema
than tuple equality; E1 does not relax its existing decoder.

Begin then strictly decodes any job and its same-key receipt/outcome. Malformed
reserved state is InvalidBundle; stored tuple mismatch is DigestCollision.
On a live receipt (now < expiry), different digest is IdempotencyConflict;
matching digest and canonical outcome replay before deadline/revision checks.
Replay requires the corresponding valid job to exist; missing job is
InvalidBundle, not a reconstruction or a new barrier.
At now=expiry the receipt is expired. With a valid job, a fresh begin returns
JobConflict, including the same key; it never replaces the permanent job.
JobConflict also covers any begin attempted while a job already exists; E2
uses the same category for its job-fence conflicts (C2 section 6).
Without a job, any existing begin receipt is inconsistent (InvalidBundle).
Otherwise check deadline (now>=deadline refuses), scope expected revision,
then physical limits and complete lower-layer validation before append.

## 3. Exact bytes and keys

H is SHA-256, hex is lowercase, no whitespace. U32/U64 are unsigned big-endian.
S(s)=U32(UTF-8 length)||UTF-8 bytes. SCOPE(b)=U32(b.len)||b.
O(None)=0; O(Some(s))=1||S(s). No trailing bytes except domain NUL below.

Existing ScopeBytes is ASCII `scope:v1` followed by O(tenant), O(agent),
O(user), O(session). ScopeKey=`scope:v1:`+hex(H(ScopeBytes)).
Graph identity/lineage comes from the bound graph; E1 does not invent a
persistent global database UUID or a portable cross-file job identity.

```text
BeginBytes = ASCII "vlite.memory.erasure.begin.v1\0"
          || SCOPE(ScopeBytes)
          || O(bound_collection)
          || U64(expected_scope_revision) || U64(receipt_window_ms)
BeginDigest = "v1:" + hex(H(BeginBytes))
```

The implicit target is exact scope, implicit mode is logical deletion; neither
is caller-variable. Scope revision, window and collection ARE hashed.
now_ms, deadline_ms and idempotency_key are execution/retry controls excluded
from BeginBytes; the latter still determines receipt identity.
No reuse or change to accepted remember/forget/consolidate digests.

Using EXISTING operation_receipt_key(op,ScopeKey,key):

```text
R(op,key) = "receipt:v1:" || ScopeKey || ":"
         || hex(H(ASCII op || ASCII ":v1\0" || UTF-8(key)))
JobKey = R("erase-scope-job", "singleton")
ReceiptKey = R("begin-erasure", idempotency_key)
OutcomeKey = "receipt-outcome:v1:" + hex(H(UTF-8(ReceiptKey))) + ":0000"
```

Operation names are deliberately distinct even though JobKey uses the receipt
key namespace helper. No ID allocation or claim that the job is itself a receipt.

## 4. Exact persisted schema and strict decoders

All three nodes have exactly one label, exactly the listed properties, no graph
TTL, temporal segment, edge, attachment, vector mutation or inline receipt outcome.
All String properties <=4096 UTF-8 bytes; Integer values use the engine i64 type.
Counters that need u64 are canonical decimal Strings (`0` or nonzero without
leading zero), bounded to u64; here generation is exactly `"1"`.
Scope tuple is canonical lowercase hex of ScopeBytes. Binding String is empty
iff no collection; otherwise its exact name. No sensitive memory text is copied.

| Node / label | String properties (exact) | Integer properties (exact) |
|---|---|---|
| JobKey / VliteMemoryErasureJob | e1_schema=`erase-scope:v1`, scope=ScopeKey, e1_scope_tuple=hex(ScopeBytes), e1_collection=binding String, begin_digest=BeginDigest, generation=`1`, state=`pending`, logical_visibility=`hidden`, deleted_count=`0`, remaining=`unknown` | created_at_ms=initial now_ms |
| ReceiptKey / VliteMemoryReceipt | e1_schema=`erase-scope:v1`, scope=ScopeKey, e1_scope_tuple=hex(ScopeBytes), e1_collection=binding String, digest=BeginDigest, expires_at_ms=decimal(initial now+window), job_key=JobKey | outcome_chunks=1 |
| OutcomeKey / VliteMemoryReceiptOutcome | receipt_key=ReceiptKey, outcome=ASCII below | index=0 |

Outcome is precisely five LF-terminated lines including final LF:

```text
erase-scope:v1
<JobKey>
1
pending
unknown
```

No spaces, CR, optional fields, padding, duplicate chunks or trailing data.
Outcome length is 15 + (len(JobKey)+1) + 2 + 8 + 8 = 183 bytes,
since JobKey is 149 ASCII bytes. Require <=4096 independently.
Original revision is read from the actual receipt node after commit/reopen,
not anticipated in outcome bytes. Existing graph PutNode revisions in one
ordinary commit are that frame's sequence; the initial job and receipt share it.
No current job revision is substituted when replaying an original receipt.

Decode label, key, exact property names/types, scope and tuple, binding, schema,
canonical digest shape `v1:`+64 lowercase hex, canonical nonzero u64 receipt
expiry, and outcome shape even for expired receipts. A receipt must refer to the
correct job; its digest must equal the job's begin_digest, and its initial
revision must equal the job's initial revision in E1. Wrong binding is
InvalidBundle; tuple collision is DigestCollision. Other malformed state is
InvalidBundle; no legacy E1 receipts or forgiving fallbacks exist.
The singleton job node carries a schema version. E1 strict decoding recognizes
only `erase-scope:v1` and fails closed on unknown versions. E2 extends the job
under a new schema version and advances the job revision; generation changes
only under C2 decision 10 (E4). The E1 receipt-revision-below-job check is
defensive for E1's atomic frame and must be revisited in E4 if maintenance
renumbers revisions.
No checksum authenticates coordinated malicious changes to valid stored fields.

## 5. Barrier and replay matrix

After existing request validation, read scope node, verify tuple, immediately
probe JobKey by exact key. Do not do memory/evidence discovery between the scope
read and job-presence probe. Provisioning also probes for a job before either
returning success for an existing scope or creating a missing scope.
If a missing scope has an orphan job, provisioning fails InvalidBundle (no
reactivation); other operations retain InvalidScope for a missing scope.

The apparent "barrier before receipt lookup" exception is narrow: after observing
the barrier, only the requested operation's exact receipt key may be read to
recognize a valid **live, matching** pre-barrier commit. Its actual graph receipt
revision must be strictly less than the job revision. No scans, other-operation
lookups or fresh planning are allowed. Its existing decoder and retention
semantics remain unchanged, including operation-specific expiry decoding:
remember and forget parse a String `expires_at_ms` as u64 and treat missing,
wrong-typed or unparsable expiry as **not expired**; their usual scope/digest/
outcome decoder then decides replay or its existing error. Consolidate strictly
decodes its receipt before expiry classification, including on expired rows.
Malformed receipt therefore means that operation's existing decoder result,
not a blanket InvalidBundle.
Expired, absent, nonmatching or non-pre-barrier receipts cannot authorize work:
DeletionPending. Even a mismatching digest yields DeletionPending here; before
a barrier it keeps its operation's usual IdempotencyConflict behavior.
No receipt refresh, TTL extension, reconstruction, resurrection or job mutation.
This exception protects retries, not reads of hidden memory content.

| Operation | Exact barred scope | Unbarred scope |
|---|---|---|
| remember / forget / consolidate | matching live pre-barrier receipt replays, otherwise DeletionPending | unchanged |
| provision_scope | DeletionPending; no receipt exception | unchanged |
| recall (including broad multi-scope) | omit that scope entirely; one crate-private deletion_barriers scope entry per barred scope in request order; never produce hits/stale labels from it | existing discovery/rank/limits |
| summary_status (S1) | DeletionPending after its existing scope-node read and tuple check, before summary lookup/TTL/edge work; no receipt exception | S1 contract |
| scope_revision | strict provisioned-scope tuple/revision read; exempt and read-only | same identity revision |
| S2 recall label | no hit, hence no label or S1 call from barred scope | S2 contract |
| erasure_status | strict read-only current job status; allowed through barrier | MissingMemory if no job |
| begin_erasure | own strict live replay/digest conflict; otherwise JobConflict | begin fresh |

Recall's existing structural validation rejects duplicate scopes and seeds
whose scope is absent from `request.scopes` as `RecallError::InvalidScopes`
before any barrier work. The probe is per requested scope in the existing loop,
after `recall_collection()` and the
`memory_scan_bound * scopes + collection.len()` source-record preflight; barred
scopes remain charged by that preflight. At the scope-node read, verify tuple,
then probe job. Process every requested scope before reporting a barred seed, so
a later missing scope still returns InvalidScope and a later tuple collision
still returns DigestCollision. If a valid entity seed belongs to a barred
scope, return DeletionPending after the scope loop rather than today's
MissingMemory; it must not become a hit, S2 label, or leak. RecallOutput has no
diagnostics field today: E1 adds exactly one crate-private
`deletion_barriers: Vec<MemoryScope>`, distinct barred scopes in request order.
Existing private `complete` and allow_partial semantics do not change. Serving's
`meta.truncation += deletion_barrier` and `complete=false` mapping is owned by
gap1d, not this contract.

The S1 probe belongs immediately after S1's existing scope-node read and tuple
check. The `stale.rs` and S2 recall-label hook is added by whichever slice lands
second; E1 implementation adds it if S1 lands first. `erasure_status` also
requires the live handle's evidence-collection binding to exactly match the
job's recorded binding (None only matches None); mismatch is InvalidBundle.
Status remains strict, read-only and barrier-exempt.
Metadata status is the only new readable job information, never forgotten text.

## 6. Atomicity, budgets, clocks and reopen

Batch order: job CreateNode(Expected::Absent), outcome CreateNode(Absent),
receipt CreateNode(Absent). Exactly three user operations, no source guards,
scope update, SetNodeExpiry or cleanup chunks. Fresh scope revision is checked
under exclusive &mut Database; no callback, release or intervening write.
Future chunks will guard the job's actual graph revision AND generation in
their single progress/delete frame. E1 offers no KV claim, lease or worker;
no dependency on the Gap 2 ledger.

At most 4096 user operations and 16,777,216 encoded body bytes. Compute actual
field lengths using C1 section 7: M=4+sum(2+name_bytes+1+payload), String payload
4+value_bytes, Integer payload 8; PutNode=8+8+2+key_bytes+1+
sum(1+label_bytes)+4+M. Body=4+sum(entries). Optional opening reclaim adds
16 bytes and zero user ops. Schema-bounded E1 cannot attain either cap;
do not manufacture invalid large E1 properties just to reach them.
Check actual batch and lower-layer frame limits; do not silently truncate.
The exact schema maximum is 6,094 body bytes: start from the tiny vector's
1,833, add 2*(2,104-44) for maximum tuple hex, 2*(63-1) for collection,
and 20-3 for receipt expiry decimal. Keys/digests/outcome remain fixed-width.
One opening reclaim raises this to 6,110. Thus changing <= to < at either
4,096 ops or 16 MiB is E1-schema-equivalent, not a counted mutation kill.
Commit clock=max(now,wall,graph high-water), captured once. No object here has
TTL; created_at_ms is request now, not a promise about append wall time.

Validation/planning/preappend refusal leaves memory, job, receipt, outcome and
cleanup unchanged: in particular, no barrier from a rejected frame.
CommitIndeterminate after possible append is preserved; no rollback guarantee
and no blind retry on a poisoned handle. Reopen/reconcile same semantic begin
under engine rules. No expiry/clock movement can remove a permanent barrier.

Implementation acceptance MUST round-trip all three exact nodes, scope tuple,
job revision/generation, status, begin replay and barrier matrix on plain file,
encrypted file, directory and encrypted directory. Reopen does not equal
revision-changing compaction/upgrade; those need E4. Python snapshot roundtrip
is a logical test, **not** evidence that these four storage backends passed.

## 7. Ruling table (choices ratified provisionally on 2026-11-04)

| Open point / options | Choice | Rationale |
|---|---|---|
| exact / descendants | exact tuple, children defined in section 1 | C2 decision and copied-scope authorization |
| target discriminant / implicit | exact-scope logical begin only | no surprise TTL/entity/subtree semantics |
| scope mutation / separate job | singleton job derived through operation_receipt_key | decision 4; no format or scope-schema change |
| reusable / permanent barrier | permanent, no graph TTL | decision 5; status never reactivates |
| process / KV / ledger | single-process job revision+generation | decision 6; no cross-namespace atomicity fiction |
| initial revision guard / unguarded | required expected_scope_revision; fresh compare only | Gap 1 section 5 expected revisions; safe replay |
| generation start | 1, persisted decimal String | checked nonzero u64; E2 may advance with a new freeze |
| expose resume token / None | None while E1 pending, remaining unknown | E2 token bytes are not frozen here |
| job key per key / per scope | per-scope fixed singleton operation key | different keys cannot create multiple barriers |
| begin repeat with new/expired key | JobConflict, query status | permanent job outlives receipt window |
| before-barrier receipt exception | only live/matching/exact op and older revision | decision 5 without authorizing fresh writes |
| mismatching old operation retry | DeletionPending behind barrier | fail closed; no different-operation lookup |
| begin replay status revision | original receipt revision; status read uses current job | same-frame actual revision, no anticipatory number |
| no job status error | existing MissingMemory | avoid an unnecessary new private variant |
| malformed barrier | writes stay barred; status/begin strict decode fail | presence cannot be bypassed by malformed properties |
| begin-erasure receipt expiry decode | strict even if expired | no E1 legacy or silent fallback; old remember/forget/consolidate retain their own decoders |
| binding absent/long name | None allowed; Some 1..63 bytes for E1 | engine collection-name limit; bind digest to collection |
| S1/S2 barrier behavior | S1 DeletionPending; S2 omission, no label work | read privacy before status source inspection |
| recall barrier output | `deletion_barriers: Vec<MemoryScope>` in request order; no partial flag change | scope omission is deliberate, not budget exhaustion |
| orphan job during provision | InvalidBundle, never recreate scope | fail closed on inconsistent stored state |
| begin while job exists | JobConflict, including same-key expired begin | permanent job remains authoritative; C2 E2 uses same fence error |
| E1/E2 persisted job schema | fail-closed E1 version; E2 uses new version and advances job revision; generation changes only under C2 decision 10 (E4) | never reinterpret unknown state under old schema |
| receipt revision equality | require initial same-frame equality in E1; revisit in E4 | maintenance may renumber revisions |
| deletion count/status | pending, hidden, 0 deleted, unknown remaining | barrier is not erasure completion |
| additions to private errors | DeletionPending, JobConflict only | existing categories suffice elsewhere |
| outcomes JSON/binary/LF | exact bounded LF ASCII; digest binary | easy strict decode, no floating/escaping ambiguity |

No conflict with adopted C2 decisions or Gap 1 section 5 is known.
The replay exception and E1 pending-without-resume choice particularly require
ratification before code; serving's future resumable job object is not changed.

## 8. Independent acceptance and not frozen

New files only: `tests/memory_erasure_oracle.py` and
`tests/fixtures/agent_memory/erasure_golden.json`; groups are named E1 so E2-E4
can append groups without rewriting accepted E1 inputs/expectations.
Goldens pin byte/digest/key vectors, schema, status, every barrier hook,
pre-barrier replay versus expiry/conflict, exact child/sibling isolation,
preappend-no-barrier, read-only status and logical reopen. Every rejected golden
declares one repairable defect; precedence probes are explicitly separate.
This hook-file list amends C2 section 6 E1's allowed files by adding
`src/memory/stale.rs` for one S1 probe call; forget is implemented in
`src/memory.rs`. Other E1 hooks are in `memory.rs`, `consolidate.rs`,
`recall.rs`, and `erasure.rs`; the S1/S2 hook lands in E1 if that slice lands
first.
Run normal and optimized Python; mutation kills must name the intended case.
Fixtures label harness-only faults/receipt seeds/backend snapshots; no Rust
logic copying or claim that a Python filter proves B/S1 implementation parity.

Coordinator-owned CI proposal, next to the existing memory oracle commands:

```text
python tests/memory_erasure_oracle.py --self-test
python -O tests/memory_erasure_oracle.py --self-test
```

**Not frozen:** E2 deletion chunks, resume request/token bytes and fencing
execution; E3 entity barriers/incident-edge erasure; E4 manifests, retention and
maintenance/restore reconciliation; X1 delete disposition; every F-track slot,
fact/correction/query/contradiction behavior; serving/auth/wire schemas; ledger
or KV worker claims; cross-process/multi-worker scheduling; secure erase,
cryptographic per-memory deletion, remote purge, backup/history completeness
or protection against unsupported coordinated tampering. E1 is not whole C.
