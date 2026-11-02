# Gap 1 C1: private consolidation contract (frozen)

Status: frozen private contract for the independent C1 oracle and a later,
separately authorized implementation. Not implemented, public or activated; no
format, wire or API change. The Gap 1 owner proposed it against `93f45d90`; the
coordinator froze it on 2026-11-02 after checking it against source at `0cfcaa4f`.
Sections 1-8 are the delivered text unchanged (original artifact SHA-256
`8DEAFEA8D8E7FF0E0D457CEDBB08CF6CD626CF3911A403C8789C98E350C0DC24`).
Private error names are semantic categories; their spelling follows the accepted
P3b mapping (P3b slices 0 and 1a are integrated). Changes need a new freeze.

Coordinator verification at `0cfcaa4f`:

- Existing helpers in `src/memory.rs` match the text: `MemoryScope::encode` and
  its key, `memory_key`, `operation_receipt_key`, `receipt_outcome_key`,
  `content_key`, `content_edge_key`, `content_digest`/`json_string`,
  `encode_sources`, `encode_provenance`, `content_chunks` and the
  empty-provenance expansion.
- Entry sizes match the codec (`payload_len`, `labels_len`, `properties_len`,
  `properties::validate`): body header 4, entry header 8, Attach 28,
  SetNodeExpiry(Some) 24, reclaim 16. Reclaims are uncounted but byte-charged and
  only open the body or follow an expiry entry due at the batch clock.
- Recomputed: outcome maximum 602 bytes; frame envelope 11,853,316 bytes. With
  this schema a full content node is 4,337 bytes, HAS_CONTENT 139 and DERIVED_FROM
  212; summary metadata stays far below 65,536 (content and provenance are each
  at most 4,096 bytes). Erratum: 4,337 corrects a misprinted 4,336; the section
  7 formulas govern, and no bound changes.
- The engine has no separate past-expiry refusal. A SetNodeExpiry already due at
  the captured commit clock makes the planner reclaim the summary inside the
  batch, so its later edges fail endpoint validation before append. Report that
  through the accepted engine/P3b error mapping, not as MissingMemory.

## 1. Boundary and explicit deltas

C1 creates one caller-produced Episodic summary from retained Entity/Episodic
sources in one provisioned exact scope. It does not complete whole slice C.
No correction, contradiction inference, deletion disposition, stale-status
maintenance, erasure job/barrier, temporal segment, new kind or provider call.
No caller-supplied graph properties or extra REFERENCES are accepted.
Observation and optional TTL are the existing episodic time model, not an
invented validity interval. Summary evidence is at most one existing attachable
record in the binding's existing collection; no vector mutation.

Inputs default to 64, explicitly raisable to the approved ceiling 256. The
private request input budget is execution control, not a persisted setting.
Provenance retains the existing separate 64-entry/4096-encoded-byte limits.

C1-only decisions, not statements about A1/A2:

- Input IDs form a canonical set: duplicates reject, then sort by UTF-8 bytes.
- Provenance order AND multiplicity are preserved. Empty provenance expands to
  one entry (summary source, None, None, None), as verified in memory.rs:1208-1218.
  An explicit equivalent single entry has the same normalized representation.
- Transformation is required and nonempty. Summary content is nonempty for C1.
- New private C1 digest encoding below is length-delimited binary, NOT A1/A2's
  canonical JSON. This supersedes the earlier underspecified JSON-convention
  suggestion. It avoids float-printing and escaping ambiguities for fixtures.
  It does not change A1/A2 request bytes or their receipt-window hashing.
- Deadline and future-observation policy checks follow successful live replay.
- C1 has no legacy receipts. Every present C1 receipt, even expired, must decode
  strictly; no missing/invalid-expiry-as-live fallback.

## 2. Request and normalization

Required: scope, idempotency_key, inputs, summary.id, summary.content,
summary.source, summary.observed_at_ms, transformation, execution C,
receipt_window_ms. Optional: input_budget (64), deadline, importance (0.5),
allow_future_observations (false), provenance (empty), expiry_ms (None),
evidence (None). Each input has id, expected_memory_revision and explicit retain
disposition; no default deletion and no disposition enum widening in C1.

Opaque IDs, source, transformation and each scope component are 1..256 UTF-8
bytes. Optional provenance kind/URI/transformation allow empty strings but at
most 256 bytes; provenance source is opaque. Scope is the accepted tenant/agent,
optional user/session tuple, with session requiring user. Observation is i64;
C is u64 constrained to i64::MAX. Revisions are nonzero u64.
Importance is finite [0,1], with negative zero normalized to positive zero.
No Unicode normalization, case folding, timestamp clamping or silent truncation.
Content is nonempty UTF-8, at most 16,777,216 raw bytes before request encoding.
Expiry is optional u64, using the accepted A2 representation; it need not fit
i64. Receipt window is positive and C+window must not overflow u64.

The source set is exact request-scope IDs only, not caller aliases. Different
scopes cannot be traversed to resolve a missing ID. Output is always Episodic.
Future-observation policy compares explicit observation to C on fresh execution.

## 3. Exact canonical bytes and hashes

All definitions here are C1-only unless expressly identified as existing.
H(x) = SHA-256(x), hex = lowercase hexadecimal without whitespace.
U32/U64 = unsigned big-endian fixed-width integers.
I64 = signed two's-complement big-endian eight bytes.
S(s) = U32(UTF-8 byte length) followed by UTF-8 bytes.
O(None) = byte 0; O(Some(s)) = byte 1 followed by S(s).
OU(None) = byte 0; OU(Some(u64)) = byte 1 followed by U64(value).
B(false/true) = one byte 0/1.
F(f) = eight IEEE-754 binary64 bytes, big-endian, after zero normalization.
Arrays have a U32 count and the ordered encodings of their elements.
No terminators, padding or trailing bytes except domain literals specified below.

ScopeBytes is the EXISTING MemoryScope::encode:
ASCII "scope:v1", then O(tenant), O(agent), O(user), O(session), in that order.
The first two are always Some. ScopeKey is "scope:v1:" + hex(H(ScopeBytes)).

SourceBytes =
ASCII "vlite.memory.consolidate.sources.v1\0"
|| SCOPE(ScopeBytes) || U32(N)
|| for each canonical input: S(id) || U64(expected_memory_revision) || byte 0.
Here SCOPE(b) means U32(b.len) || b, and disposition byte 0 means retain.
SourceDigest = "v1:" + hex(H(SourceBytes)).

Provenance entry bytes = S(source) || O(kind) || O(uri) || O(transformation).
Evidence bytes = byte 0 if absent; otherwise byte 1 || S(evidence.id)
|| SCOPE(evidence.ScopeBytes). Collection is fixed by the binding, not request.
The receipt additionally binds its collection below.

RequestBytes =
ASCII "vlite.memory.consolidate.request.v1\0"
|| SCOPE(ScopeBytes)
|| U32(N) || each canonical input's S(id), U64(revision), byte 0
|| S(summary.id) || S(summary.content) || S(summary.source)
|| I64(observed_at_ms) || F(importance) || OU(expiry_ms)
|| U32(normalized_provenance.len) || each ordered provenance entry
|| S(transformation) || EvidenceBytes
|| B(allow_future_observations) || U64(receipt_window_ms).

RequestDigest = "v1:" + hex(H(RequestBytes)).
Scope/output kind are fixed or encoded; no unrepresented variable kind.
Execution C/deadline, input_budget and idempotency_key are excluded.
Receipt window REMAINS HASHED. Collection identity is checked by receipt binding,
so changing a binding cannot silently replay another collection's receipt.
RequestBytes >16,777,216 fails BodyLimit before receipt access.
This replaces the earlier JSON-escaping request-boundary claim: binary encoding
has no escaping expansion. Its exact byte boundary can still be constructed
with a valid long raw content field, independently of later operation refusal.

## 4. Exact graph schema and keys

Existing summary key: "mem:v1:" + ScopeKey + ":" + hex(H(summary.id UTF-8)).
Existing content digest: "v1:" + hex(H(existing json_string(content) UTF-8)).
This is the A1 content identity, NOT the C1 request digest.
Summary label: VliteMemory. Existing memory_properties names/types:

String: scope, id, kind ("episodic"), content_digest, sources, provenance,
memory_revision ("1"), reference_count ("0"), evidence.
Float: importance.
Integer: observed_at_ms, created_at_ms, updated_at_ms.
String content is present iff raw content <=4096 bytes.
created_at_ms and updated_at_ms are execution C on first commit.
sources is existing encode_sources([summary.source]); provenance is existing
encode_provenance(normalized ordered entries). Evidence is empty or lowercase
hex of evidence ID UTF-8 (existing single-element evidence encoding).
Concretely, sources is hex(summary.source UTF-8), without a semicolon for this
single value. Provenance concatenates four fields per entry, in source/kind/uri/
transformation order, preserving entries and duplicates: None is "-;"; Some(s)
is canonical decimal UTF-8 length + ":" + s + ";". No outer delimiter/count.
This textual persistence encoding differs from the binary digest encoding.

Additional summary String properties, exactly:
c1_schema = "consolidate:v1"
c1_transformation = normalized transformation
c1_source_digest = SourceDigest
Additional Integer property: c1_source_count = N.
No source-ID array or source content/provenance copy on the summary.
No assertion that old generic memory readers validate these additional fields.

For each source, one edge summary -> source:
relationship DERIVED_FROM; no edge expiry or temporal segment.
Key = "derived:v1:" + hex(H(
ASCII "vlite.memory.consolidate.edge.v1\0"
|| S(summary graph key) || S(source graph key))).
String properties exactly:
c1_schema = "consolidate:v1"
c1_source_memory_revision = canonical decimal expected u64 revision.
No source mutation, no source expected-graph-revision guard operation.

Large content uses existing UTF-8 scalar-safe content_chunks (actual chunk
count, not blindly ceil(bytes/4096)). Keys and labels:
"content:v1:" + hex(H(summary graph key UTF-8)) + ":" + four-digit lowercase
hex chunk index; VliteMemoryContent; properties scope String, index Integer,
content String. Existing content edge key uses "content-edge:v1:" with the
same hashed summary key/index; relationship HAS_CONTENT, empty properties.
Indices in valid C1 are below 0x1000, so four digits are exact.

Receipt key uses EXISTING operation_receipt_key("consolidate", ScopeKey, key):
"receipt:v1:" + ScopeKey + ":" +
hex(H(ASCII "consolidate:v1\0" || idempotency_key UTF-8)).
Receipt label: VliteMemoryReceipt. Properties exactly:
String scope = ScopeKey
String c1_scope_tuple = hex(ScopeBytes)
String c1_schema = "consolidate:v1"
String c1_collection = bound collection name, or empty string if no binding
String digest = RequestDigest
String expires_at_ms = canonical decimal C+receipt_window_ms
Integer outcome_chunks = 1.
Receipt expiry is logical; no SetNodeExpiry on receipt.

One outcome node uses existing receipt_outcome_key(receipt_key,0):
"receipt-outcome:v1:" + hex(H(receipt_key UTF-8)) + ":0000".
Label VliteMemoryReceiptOutcome; properties exactly:
String receipt_key; Integer index=0; String outcome = bytes below as ASCII.
No extra outcome nodes, links, legacy inline outcome or duplicate input list.

## 5. Exact one-chunk outcome and strict decoding

ASCII outcome, with LF after EVERY line including final line:
consolidate:v1
<lowercase hex of summary.id UTF-8>
1
<canonical decimal N>
<SourceDigest>

Thus outcome length = 15 + 2*id_bytes + 1 + 2 + digits(N)+1 + 67+1,
at most 602 bytes (id_bytes=256,N=256); validate <=4096 regardless.
Line 1 includes LF (15 bytes), line 3 is "1\n"; no CR, spaces, optional fields
or trailing data. Persisted summary memory revision is the original 1.
Returned receipt revision/commit sequence comes from the actual receipt node,
not an anticipatory number embedded in the outcome. Replay returns that same
original outcome even if source/summary state later changes.

Strict C1 receipt decoding validates before live/expired classification:
exact label/schema/property names and types; scope equals ScopeKey and stored
scope tuple matches (tuple mismatch -> DigestCollision); collection binding
matches (mismatch -> InvalidBundle); digest is "v1:" plus 64 lowercase hex;
expires_at_ms is nonzero canonical decimal u64, no sign/leading zero/whitespace;
outcome_chunks is Integer 1, not String/Float; exactly the expected index-0
node exists with its exact label/properties, receipt_key and Integer index 0.
Missing/extra properties in these reserved receipt/outcome nodes reject.
Outcome must have precisely five LF-terminated lines and canonical values;
decoded ID must be valid UTF-8/opaque, revision exactly 1, N in 1..256,
SourceDigest correctly shaped, hex canonical and <=4096 bytes overall.
No reconstruction of a digest from currently retained source objects.

On a live matching request digest, additionally compare decoded summary ID,
N and SourceDigest to normalized request. On an expired receipt, validate
intrinsic encoding only; a different new request need not match old outcome.
Valid shape but maliciously self-consistent changes are not cryptographically
authenticated by a memory checksum; do not claim detection of arbitrary
coordinated storage forgery. Normal frame integrity remains the engine's job.

## 6. Error precedence and born-expired summary

The following is C1-specific ordered behavior. Existing expectations unchanged.

1. Structural checks: scope InvalidScope; key InvalidKey; C InvalidClock;
   receipt window InvalidReceiptWindow; input budget/count InvalidBundle for
   empty/zero or OperationLimit for excess; caller-order source ID/revision
   InvalidTarget, duplicates DuplicateTarget, any non-retain disposition
   InvalidBundle. Then summary ID/source
   InvalidIdentity, empty content InvalidContent, raw oversize BodyLimit,
   importance InvalidImportance, observation InvalidObservation, expiry
   representation InvalidExpiry. Provenance: count then encoded bytes
   ProvenanceLimit, then field validity InvalidProvenance. Transformation
   invalid -> InvalidProvenance. Evidence ID InvalidEvidence; evidence scope
   shape InvalidScope, explicit different scope CrossScope. Finally canonical
   RequestBytes cap -> BodyLimit. All before receipt lookup.
2. Verify provisioned scope and tuple: InvalidScope / DigestCollision.
3. Decode present C1 receipt strictly as above: malformed -> InvalidBundle;
   tuple collision -> DigestCollision. At C<expiry: mismatching digest ->
   IdempotencyConflict; matching digest/outcome -> replay immediately.
   At C>=expiry: valid receipt is expired, continue with fresh execution.
4. Fresh execution: elapsed deadline (C>=deadline) -> DeadlineExceeded;
   disallowed future observation -> FutureObservation;
   summary expiry Some(E) with E<=C -> MissingMemory.
5. In canonical input order: absent/expired source -> MissingMemory;
   malformed existing source -> InvalidBundle; unsupported kind ->
   UnsupportedKind; expected MEMORY revision mismatch -> RevisionConflict.
   Fresh summary ID OR existing Episodic exact content/source dedup identity
   collision -> IdentityConflict. Optional evidence ownership/attachability
   checks preserve existing CrossScope/RecordAlreadyAttached categories.
6. Actual user operation budget -> OperationLimit; lower layer completes all
   batch/frame validation before append; frame size -> BodyLimit. Other
   lower-layer errors preserve accepted P3b/private storage mapping.

Born-expired resolution is explicit: every C1 summary has >=1 DERIVED_FROM
edge, so a fresh request with E<=C rejects MissingMemory, WITH OR WITHOUT
evidence, and changes nothing. It never writes an immediately reclaimed summary
or drops the derivation edges to make the request succeed. This is a C1 rule
motivated by A2's expired-with-reference/evidence handling, not blanket parity.
A valid live receipt replays before this check, even when output TTL is now due.

Batch order: summary CreateNode; optional SetNodeExpiry; content nodes/edges;
DERIVED_FROM edges in canonical input order; optional Attach; outcome node;
receipt node. A future E>C can still be <= the engine's captured commit clock:
then summary reclaim makes endpoint/attachment resolution refuse preappend.
Do not remap that lower-layer refusal to a guaranteed MissingMemory without
checking accepted P3b. No artificial clock subtraction or TTL clamping.

Commit clock is max(request C, wall, graph high-water), captured ONCE by
commit_planned before reclamation. No promise of detecting every later real
wall crossing during encoding/append. No new C1 callback/release/intervening
write between validation and commit. Existing timestamp clock hook may run.

## 7. Physical budget arithmetic and failure semantics

Let N=1..256, K=0 for inline content or actual UTF-8 content chunk count,
A=0/1 Attach, X=0/1 SetNodeExpiry. Physical user GraphOps:
P = N + 2K + A + X + 3.
Three = summary + outcome + receipt. No per-source guard or revision bump.
Fresh receipt/outcome CreateNode enforces absence. A valid expired receipt and
its outcome use actual expected graph revisions for replacement. If the private
logical write budget charges each such guard separately, add exactly those
0..2 charges, not fictitious N charges; oracle reports physical and logical
counts separately. Fresh maximum witnesses avoid replacement ambiguity.

N=1,K=2046,A=X=0 -> P=4096, body 8,380,416 ASCII bytes is valid.
N=2 with identical valid body -> P=4097, reject OperationLimit.
Maximum accepted raw content is 8,380,416 bytes with fully packed chunks;
UTF-8 can reduce that maximum for a given scalar pattern.

Exact byte arithmetic uses existing codec:
metadata M = 4-byte count + sum(2+field_name_bytes+1+value_payload).
String payload = 4+UTF8 bytes, Integer/Float payload=8.
PutNode entry = 8 envelope +8 ID+2 key_length+key_bytes
+1 label_count+sum(1+label_bytes)+4 metadata_length+M.
PutEdge entry =8+24+1+relationship_bytes+2+key_bytes+4+M.
Attach entry=28; SetNodeExpiry(Some)=24; reclaim=16; body header=4.
Reclaim entries do not consume user-op budget; their bytes do count.

With the specified fixed reserved fields, content entry <=5120 and each
HAS_CONTENT/DERIVED_FROM entry <=512. Summary/receipt/outcome each <=66,048
using the engine's generous 65,536 metadata ceiling. At most an opening reclaim
and one due-after-summary-expiry reclaim; no receipt graph expiry. Therefore
even independently overestimating all maxima:
4 + 2046*(5120+512) +256*512 +3*66048 +1024
=11,853,316 bytes <12MiB<16MiB.
This corrects the former four-byte omission in the envelope, not a tight
attained maximum. Independent fixtures must use actual schema field lengths,
not this loose bound, for exact expected frame lengths.

Physical 16MiB is unreachable for this C1 schema. Preserve lower-layer proof:
memory::tests::oversized_physical_frame_is_rejected_without_append;
property_graph::engine::codec::tests::encoded_length_is_checked_before_any_payload_is_copied;
property_graph::engine::codec::tests::batch_entry_and_byte_limits.
No invalid manufactured C1 fixture to reach that nominal byte boundary.
Raw/request 16MiB bounds are separate validation tests; long requests can pass
those checks and still fail operation budget. Do not claim accepted16MiB C1.

Unchanged-state/file/no-append guarantees apply to validation, planning and
preappend rejection ONLY, including failed engine endpoint/frame validation.
No blanket rollback for arbitrary I/O error: commit_frame_with_sync can poison
the handle and return CommitIndeterminate after possible append/sync effects.
Preserve that error; do not reuse poisoned handle or promise safe blind retry.
Reopen/recover and reconcile same semantic request/key under engine rules.

Exclusive &mut Database and retained file/directory writer lock cover the
normal-API source race. CreateEdge/PutEdge/insert_edge change adjacency, edge
indexes and sequence, NOT source graph-node or memory revisions/properties.
Source TTL and attachments remain unchanged for selected live sources.
Unrelated expired objects may be reclaimed on successful commit. Failure
before append commits no cleanup. No unsupported external tampering guarantee.

## 8. Required independent acceptance coverage

Additive C1 fixtures only. Do not edit accepted write/ranking expectations.
Input64/65 default and256/257 override; canonical source permutation; provenance
order/multiplicity conflict and empty expansion equality; semantic digest
changes including receipt window; live replay before deadline/future/TTL/source
checks; expiry equality; strict malformed receipt/outcome cases even expired;
output identity/dedup collision; source revision/scope/TTL checks; optional
evidence constraints; actual chunk packing and fresh4096/4097 operations;
request-byte limit (distinct from physical frame); preappend failure atomicity;
source-node/property/TTL/attachment preservation and all-four-backend reopen;
forget interactions and B's REFERENCES-only traversal isolation.

One-frame creation + receipt and repeat outcome equality must be independently
observed. Born-expired WITH and WITHOUT evidence both reject; live replay of
now-expired summary succeeds without resurrection. Pin commit-clock tests to
captured clock semantics, not timing races. Corruption fixtures do not pretend
to detect arbitrary coordinated forgery. Real fail-first mutations required
for each behavior group after implementation is separately authorized.

No whole-C completion, production activation or public contract is claimed.
