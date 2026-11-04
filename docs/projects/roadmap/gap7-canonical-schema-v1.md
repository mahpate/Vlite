# Gap 7 canonical schema v1

Status: **PROPOSED for coordinator ratification**. This appendix specifies bytes,
not an implementation acceptance, public API, wire, activation or release decision.
It supplements sections 3-6 of `gap7-graphrag-ingestion.md` without changing their
operation order, state derivation, admission limits or thirteen approved decisions.
The independent implementation is `tests/ingestion_schema_reference.py`; its
schema-v1 fixtures are not outputs of Rust or of the preparation oracle.

## 1. Primitives and framing

Concatenation below is literal byte concatenation. No padding, platform alignment,
terminator, newline conversion, Unicode normalization, BOM insertion or implicit
coercion is permitted. Integers reject booleans and overflow. Decoders reject
unknown tags, duplicate/unsorted map keys, invalid UTF-8, trailing bytes, nonfinite
floats and noncanonical negative zero. Encoders canonicalize negative zero.

| Notation | Exact representation |
|---|---|
| U8 / U16 / U32 / U64 | Unsigned fixed-width little-endian 1 / 2 / 4 / 8 bytes |
| I64 | Signed two's-complement little-endian 8 bytes |
| F64 / F32 | Finite IEEE-754 binary64 / binary32, little-endian; zero has positive sign |
| Bool | U8: false=0, true=1; all other values invalid |
| D | Exactly 32 raw digest bytes, not length-prefixed or hexadecimal |
| B(x) | U32(byte length of x), then x |
| S(x) | B(exact UTF-8 bytes of x) |
| Opt(T) | U8(0) if absent; otherwise U8(1), then T; absence is not an empty string |
| List(T) | U32(element count), then each T in the specified order |
| Map(T) | U32(entry count), then S(key), T(value), sorted unsigned UTF-8 key bytes |
| Attr | The already frozen scalar encoding in the next paragraph |
| Attrs | B(the already frozen `attributes_key` bytes), NOT a new counted map |

Preserved verbatim scalar tags: **0 Null; 1 Bool + U8(0/1); 2 Integer + I64;
3 Unsigned + U64; 4 Float + F64; 5 String + U32-LE UTF-8 byte length + bytes**.
Preserved `attributes_key`: concatenate S(key), Attr(value) in unsigned UTF-8 key
order, with **no map count**. Attrs adds only an outer byte-array frame so a
following field is unambiguous. Empty Attrs is `00000000`. Null and Unsigned
remain distinct from missing, signed, and string values. This is not the engine's
metadata encoding. No existing scalar tag, preparation fixture or function changes.

All named records have a leading U8(1); nested record values use B(the complete
record including its version). A binding and an Entry inside Shard are inline
structs, not nested named records. The standalone diagnostic `entry` fixture
uses a version byte before that struct, not an extra byte in each shard entry.
Identity records add their registry U8 tag after version. Other record schemas
are distinguished by their use site and domain, not by an extra inferred tag.

**Exact framed hash**:

```text
H(domain, payload) = SHA256( 01 || S(domain) || B(payload) )
```

Domains are the literal ASCII strings below, with no trailing NUL. The payload
is the complete named record. Domain bytes and payload bytes both have U32
lengths. Digest text is exactly 64 lowercase hexadecimal ASCII characters; reject
uppercase, prefixes, whitespace and odd lengths. Chunk/content digests remain
SHA-256 of exact raw source/chunk bytes, **without this domain framing**.

## 2. Registry

No string is implicitly derived at decoding time. The following literal registry
is complete for v1. Domains are distinct; tokens are distinct. Identity tag values
are U8, reserved even for later slices. `edge` here means a keyed ordinary Multi
edge, never EnsureEdge or UniqueTriple.

| Tag | Identity / role | Domain literal | Graph-key token |
|---|---|---|---|
| 1 | source head | `vlite.g7.v1.source` | `src` |
| 2 | revision | `vlite.g7.v1.revision` | `rev` |
| 3 | chunk | `vlite.g7.v1.chunk` | `chk` |
| 4 | entity | `vlite.g7.v1.entity` | `ent` |
| 5 | HAS_REVISION edge | `vlite.g7.v1.has-revision` | `hr` |
| 6 | HAS_CHUNK edge | `vlite.g7.v1.has-chunk` | `hc` |
| 7 | MENTIONS edge | `vlite.g7.v1.mention` | `men` |
| 8 | extracted relation edge | `vlite.g7.v1.relation` | `rel` |
| 9 | ingestion root receipt | `vlite.g7.v1.receipt` | `rec` |
| 10 | batch receipt | `vlite.g7.v1.batch-receipt` | `bat` |
| 11 | result-ID shard | `vlite.g7.v1.result-shard` | `ids` |
| 12 | ownership shard | `vlite.g7.v1.ownership-shard` | `own` |
| 13 | delete root receipt | `vlite.g7.v1.delete-receipt` | `del` |
| 14 | abort root receipt | `vlite.g7.v1.abort-receipt` | `abt` |
| 15 | vector allocation identity, not a graph node | `vlite.g7.v1.vector` | none |

Additional non-key domains: `vlite.g7.v1.pipeline`, `vlite.g7.v1.request`,
`vlite.g7.v1.plan`, `vlite.g7.v1.batch`, `vlite.g7.v1.delete-action`,
`vlite.g7.v1.abort-action`, `vlite.g7.v1.policy`, `vlite.g7.v1.properties`.
Policy digest = H(policy domain, complete Policy record). Captured property
digest = H(properties domain, canonical C9 metadata bytes specified below);
metadata is the one payload here that uses its existing engine framing rather
than a named-record version prefix. No new tombstone node is allocated: the source head is
tombstoned, as the approved contract requires. A tombstone therefore retains the
source key/domain, not an independently keyed object.

For each keyed role, key = ASCII `g7_` + token + `_` + hex(H(domain, identity)).
Length is 3 + token length + 1 + 64 = 68 + token length: 70 for `hr`/`hc`, 71 for
every other token. All are below the 256-byte C9 key cap. Vector record IDs are
planner-chosen U64s, not a hash-derived allocation method.

## 3. Binding and identities

Binding = S(graph_name), S(collection_name), S(tenant), D(generation).
Names are the exact stored, authorized names, including tenant qualification,
not transient numeric engine IDs; each name is 1..63 bytes, tenant is 1..128
bytes. Generation is an opaque 32-byte provisioned value, immutable for this
binding; it is not generated by inference or by hashing a runtime lineage.
Reprovisioning must supply a different generation and pipeline version.
The explicit tenant in an identity must equal binding.tenant.

This name-based form survives maintenance renumbering. Numeric graph/collection
IDs are captured authorization/execution bindings, never identity fields.
No database lineage or graph revision occurs in a source/entity/content identity.
Delete's explicit expected source revision remains in its approved action identity.

Every identity is `01, U8(tag), fields in table order`. `I(role)` below means
B(the full identity of exactly that role), not a digest or a graph key. Full
nested fields prevent a colliding endpoint hash from substituting a different
endpoint identity. Root means an ingestion/delete/abort receipt identity.

| Role | Ordered identity fields after version/tag |
|---|---|
| source | Binding, S(tenant), S(source_id) |
| revision | I(source), D(content_digest), D(pipeline_version) |
| chunk | I(revision), U32(ordinal), U32(start_byte), U32(end_byte) |
| entity | Binding, S(tenant), S(kind), S(normalized_key) |
| has_revision | I(source), I(revision) |
| has_chunk | I(revision), I(chunk) |
| mention | I(chunk), I(entity), U32(source_start), U32(source_end) |
| relation | I(revision), I(chunk), I(entity subject), S(predicate), I(entity object), U32(source_start), U32(source_end), Attrs |
| receipt | Binding, S(tenant), S(source_id), D(content_digest), D(pipeline_version) |
| batch_receipt | I(root), U32(batch_ordinal) |
| result_shard / ownership_shard | I(root), U32(batch_ordinal), U32(dependency_group), U32(shard_index) |
| delete_receipt | Binding, S(tenant), S(source_id), U64(expected_source_revision) |
| abort_receipt | Binding, S(tenant), S(source_id), I(aborted root) |
| vector | Binding, U64(planned_record_id) |

Chunk ordinals and dependency-group ordinals start at zero. Batch ordinals start
at one. Shard indexes start at zero **within (root, batch, group, shard role)**.
Reject missing, skipped or duplicate indexes during replay. Spans are source
offsets, checked as in preparation; no change to chunk-relative extraction.
Entity keys are already ascii-key-v1 normalized, 1..256 bytes. Kind/predicate are
case-sensitive provisioned identifiers, 1..63 bytes, as required by the engine's
label/type rules; no arbitrary extractor strings become executable schema.

Lookup recomputes the role key, decodes the stored full identity and compares
**all fields, recursively, byte-for-byte** after canonical validation. Equal
digest/key and different identity is `ingestion identity conflict`, never reuse.
Identical identity with differing evidence properties is the existing evidence
conflict, not a hash collision and not last-writer-wins.

## 4. Stored objects and C9 representation

Logical stored records below are version byte 1 followed by the ordered fields
shown. Keys are S(graph key), counts U32, flags Bool, digests D. Optional fields
always use Opt. Evidence attributes are Attrs, not native untyped engine values.
Absent expiry/TTL is mandatory, including on vector records.

| Object / record name | Ordered data fields |
|---|---|
| head_data | Opt(S revision_key), Opt(D revision_digest), Opt(D pipeline), U8(status), Opt(S job_receipt_key), U32(last_batch) |
| revision_data | S(source_key), Opt(S uri), U32(text_length), U32(chunk_count), S(receipt_key) |
| chunk_data | S(source_key), S(revision_key), U32(ordinal), U32(start), U32(end), D(chunk_digest), D(pipeline) |
| entity_data | empty; immutable kind/normalized key live in full identity |
| link_data | empty; directed endpoints live in full identity |
| mention_data | S(source_key), S(revision_key), S(chunk_key), S(surface), Opt(F64 confidence), D(extractor_version), Attrs |
| relation_data | S(source_key), S(revision_key), S(chunk_key), Opt(F64 confidence), D(extractor_version) |
| receipt_data | D(request_digest), DigestSlot(plan), U32(batch_count), S(revision_key), U8(outcome), U32(chunks), U32(entities), U32(relations), U32(mentions) |
| batch_data | DigestSlot(batch), Bool(final), U32(created_anchors), U32(reused_anchors), U32(result_shards), U32(ownership_shards) |
| vector_data | S(tenant), S(source_key), S(revision_key), S(chunk_key), B(exact text), List(F32 vector bits) |

Head status tags: 0 Partial, 1 Complete, 2 Deleted, 3 Aborted. These describe the
**head's** current job, not stored receipt states. Superseded/receipt state and
receipt_revision remain derived exactly as contract section 3 says. Outcome tags:
0 Created, 1 Replaced, 2 Deleted, 3 Aborted. Delete/abort roots use receipt_data;
for no resulting revision, revision_key is the empty string, counts are planned
cleanup counts, and the outcome disambiguates it. No change to public result shape.

Extractor version is proposed as an opaque, provisioned **32-byte immutable
version token**, not an arbitrarily long display label and not an inferred hash
of an unspecified registry encoding. The pipeline and evidence carry the identical
token. Model-version/alias strings occur in the bounded prepared pipeline, not in
stored evidence. This choice needs explicit ratification: an unbounded textual
extractor version cannot always fit a C9 evidence object. Changing the version
token requires a new canonical pipeline/version; never truncate a version string.

Exactly one node label is used: source=`G7Source`, revision=`G7Revision`,
chunk=`G7Chunk`, entity=`G7Entity`, receipt=`G7Receipt`,
batch_receipt=`G7BatchReceipt`, result_shard=`G7ResultShard`,
ownership_shard=`G7OwnershipShard`, delete_receipt=`G7DeleteReceipt`,
abort_receipt=`G7AbortReceipt`. HAS_REVISION/HAS_CHUNK/MENTIONS use those literal
relationship types; extracted relations use their provisioned predicate. Each
edge is keyed by its complete identity above, never keyless. Every physical
property, endpoint and relationship type participates in bit-identical replay
comparison. Labels/type tokens satisfy the existing 63-byte syntax limit.

Physical C9 properties for every non-shard graph object:

* `g7_v`: Integer 1, `g7_kind`: String(registry token).
* `i00`, `i01`, ...: canonical padded RFC 4648 base64 fragments of full identity.
* `d00`, `d01`, ...: canonical padded RFC 4648 base64 fragments of stored data.
* `source_key`, `revision_key`, `chunk_key`: String, **only** when present and
  non-absent in the data schema. Copy exactly and check against decoded data.

Split each binary value at 3,072 raw bytes; independently base64-encode each
fragment to at most 4,096 ASCII bytes. Always emit at least `i00` and `d00`, even
for an empty logical payload. Require contiguous two-digit decimal suffixes,
maximal nonfinal fragments, canonical padding, no whitespace and exact reencoding.
Reject >99 fragments or any C9 field/byte limit; never silently drop a field.
Concatenating decoded fragments recovers the complete original bytes, so a
16,384-byte surface does not violate the 4,096-byte single-string cap.

This representation deliberately stores Null and Unsigned inside tagged Attrs:
C9 PropertyValue has no Null or unsigned-u64 variant. No conversion to signed,
lossy float, string sentinel or new engine hook is authorized. All physical
properties are existing String/Integer scalars. Confidence remains a finite F64
in the data record. The immutable entity has no mutable descriptive data.

Within the approved preparation limits, identities/data fit the C9 envelope.
Exact preflight must check the actual physical maps: at most 64 fields, field
names 1..256 bytes, strings <=4,096 bytes, aggregate metadata <=65,536 bytes.
Do not infer acceptance merely from each string fitting. If a valid input cannot
fit, report it instead of truncating; this appendix is not permission to lower
the approved bounds. `src/property_graph/engine/model.rs` and `limits.rs` were
read read-only; no schema/codec/export change is proposed there.

For the String/Integer properties used here, canonical C9 metadata is U32(field
count), then fields sorted by unsigned UTF-8 name bytes: U16(name byte length),
name bytes, U8(type), value. Type 1=String with S(value); type 2=Integer with I64.
No native Null, unsigned, float, boolean or geographic field is introduced by this
schema. This is the existing metadata encoding, not the frozen Attr encoding.
The metadata length includes its field count but not an outer engine length.

Capacity witnesses use the largest 63/63-byte names, tenant128, source256,
kind63/key256, surface16,384, Attrs inner8,192, confidence present and fixed32
extractor version. Source/revision/chunk/entity identities are respectively
692/762/780/759 bytes. The mention identity is 1,557 and mention data 24,851;
its complete physical map is **35,613 bytes / 15 fields**. A relation identity is
11,349 bytes; its smaller data/map also fits. Other nonrecursive stored roles
are smaller. These witnesses do not authorize a new source/surface limit.
Abort targets must be the existing ingestion/delete job root, not recursively
nested newly invented abort requests; ongoing abort resumes its existing root.

## 5. Result-ID and ownership shards

Each shard has **one property**, `data`: String(canonical padded RFC 4648 base64
of the entire binary Shard record). Receipt-key/index back-references are inside
that string; there is no second unbounded provenance string. No split fragment
is stored on another node. Each shard is one ordinary CreateNode operation.

```text
Shard = 01, U8(channel: result=0 / ownership=1),
        S(root_receipt_key), U32(batch), U32(group), U32(index),
        List(Entry)
Entry = U32(producing_op), U32(result_ordinal), U8(action), U8(object_kind),
        S(object_key), IdRef, RevisionRef, Opt(U16 collection),
        Opt(U64 record), Opt(S attached_chunk_key), Bool(retained)
IdRef = U8(0), U64(actual id)
      | U8(1), U32(producing operation ordinal), U32(result ordinal)
RevisionRef = U8(0)                         # Absent
            | U8(1), U64(captured revision)
            | U8(2), U32(previous batch), U32(result ordinal)
```

Action tags: 0 created, 1 reused, 2 removed. Object-kind tags:
0 node, 1 edge, 2 vector. Result channel records all returned mappings; ownership
channel records created/removed source-owned objects and retained/reused anchors.
`retained=true` is mandatory for shared entity anchors, false for source-owned
objects. Removed entries are manifest accounting, not instructions to delete a
second time; live ownership is reconstructed from created/removed entries.
Refs and kinds must agree with the named operation/object. Duplicate entries
are invalid, not silently coalesced.

Entry order is ascending `(producing op ordinal, result ordinal, action,
object_kind, object_key UTF-8 bytes)` from the frozen ordered manifest. For objects
that already exist, use their captured manifest ordinal as the producing ordinal;
IdRef's literal id is the checked captured value. Never sort by newly allocated
numeric IDs or completion order. Removal/cascade semantics remain section 4's.
Producing-operation ordinals are zero-based across the complete plan, traversing
batches, groups and operations in their frozen order, not reset per batch.
Each v1 object-producing operation returns one object, result ordinal zero;
guards have no result. Removed/captured rows use the corresponding removal
operation's ordinal and captured literal id. RevisionRef(kind=2) names the
previous batch receipt's graph revision; its result ordinal must be zero.

For a 71-byte receipt key, the binary header through count is:
1+1+(4+71)+4+4+4+4 = **93 bytes**. Base64 capacity 4,096 ASCII bytes means
exactly **3,072 raw bytes**, leaving **2,979 raw entry bytes** in this case.
General raw entry capacity = 3,072 - (22 + receipt_key UTF-8 byte length).
There is no second hidden allowance for back-references.

For each dependency group and channel independently, greedily take complete
entries while total raw bytes <=3,072; emit maximal nonempty shards, then advance
index. Never span dependency groups; never split an entry. If a single entry
does not fit, fail preparation and report the schema conflict. Do not emit empty
shards. On decode, reject wrong channel/root key/batch/group/index, padding,
trailing bytes, out-of-order entries and non-maximal intermediate shards.

Committed result IdRefs are literal U64s; a prepared view uses the symbolic
producing operation reference. Since both are 9 bytes, runtime allocation does
not change shard capacity. A literal IdRef in a prepared record refers only to
an existing captured object; an allocated id may not be smuggled into this view.

Entries carry bounded graph keys, **not recursive full identities**. A full
relation identity can exceed one shard's capacity; duplicating it in each shard
would be unnecessary and incompatible with C9. Before following a key, read the
target's stored full identity and compare it to the prepared manifest, including
captured revision. On cleanup without the old bundle, validate the target's
identity, key, source/revision ownership and shard root transitively. A key alone
is never proof of ownership. Shared anchors are retained. For a removed target,
the removal receipt/manifest records the historical key and captured ID; absence
does not authorize deletion of an unrelated replacement.

Root/batch receipts and result shards are retained metadata, not source-content
cleanup targets. Ownership shards may be removed by the explicit ING-012 abort
cleanup together with the objects they list; this does not authorize deleting
retained receipts/result shards. All metadata creates still have deterministic
manifest entries, with retention flags reflecting that distinction. Include these
entries when packing, including a shard's own symbolic result. The bounded
metadata packing closure is not a hash fixed point: begin with non-shard entries,
pack each group/channel, add the predicted shard-create entries, regenerate
ordinals and greedily repack until shard counts per group/channel are unchanged.
Do not append during this calculation. Every added shard introduces at most one
107-byte graph entry per channel versus 2,979 available entry bytes, so this
closure is monotone and bounded; reject operation/frame/memory overflow rather
than truncate. Root/batch receipt entries are included from the first iteration.
For example, 28 maximum-size base graph entries in each channel need two shards
per channel; adding the four shard entries leaves 32 entries, still two shards
per channel. Freeze only the final complete entries/ordinals. `split_shards`
qualifies greedy packing of that complete list; it is not a plan constructor.

Node/edge entries have collection/record/chunk fields absent; vector entries
have collection, record and attached_chunk_key present and object_key empty.
All ingestion keys are <=71 bytes. The largest entry is 8+1+1+4+9+9+3+9+76+1 =
121 bytes (vector), or 8+1+1+75+9+9+1+1+1+1 = 107 bytes (graph).
At least one valid entry always fits; exact split remains length-based.
Shard identities themselves retain I(root), but the compact stored header holds
root_key; replay reconstructs I(root) from the root's **validated full identity**
before comparing/recomputing the shard key. This is a transitive stored identity,
not trusting a digest collision as identity.

## 6. Digest dependency and complete record schemas

Proposed circularity resolution:
DigestSlot is U8(slot kind), D(value): kind 0 literal, 1 current plan, 2 current
batch. Canonical masked views replace only kind-1/kind-2 value bytes with 32 zero
bytes and retain the kind tag. Literal zero digests are never placeholders.
Persisted slots contain the computed digest; compare them to the recomputed
masked digest. A caller cannot turn an ordinary attribute into a placeholder.

Request: source identity + exact content digest + URI presence/value + full pinned
pipeline + expected-source tag/value + mode + exact original text. Execution
deadline is excluded. Pipeline includes every semantic limit, strategy, overlap,
rule, provider/model identifier, dimensions, schema version and binding generation.
A request is not an identity: conflicting URI/mode/policy under a receipt key
still fails idempotency checks.

Batch: root identity, ordinal, group boundaries, ordered operations and planned
symbolic refs in a masked view; never hash runtime guard substitutions, allocated
IDs, completion order or created-vs-reused anchor outcomes.
Plan: request digest, captured affected-object state and full accepted extraction/
vector payloads, then ordered planned batches (including their complete bodies,
not a self-reference to the plan). Both body and batch mask
plan/batch slots before hashing, breaking the root/first-batch/plan cycle.
Compare runtime lineage separately in the retained envelope before any execution;
no lineage is placed in a key or identity. Frozen symbolic previous-batch revision
references, not predicted numeric revisions, belong in the plan.

The following named records all begin with 01. Field order is normative:

| Record | Ordered fields |
|---|---|
| policy | U32(source_bytes), U32(chunks), U32(entities), U32(relations), U32(mentions), U32(chunk_entities), U32(chunk_relations), U32(decoded_bytes), U32(attribute_bytes), U32(attribute_fields), U32(attribute_string_bytes), U32(frames), U32(operations), U64(prepared_bytes) |
| entity_rule | S(rule_id), S(literal), S(kind), S(key) |
| relation_rule | S(rule_id), S(predicate), S(subject_kind), S(object_kind) |
| pipeline | U8(chunker: FixedUtf8V1=0, ParagraphV1=1), U32(chunk_bytes), U32(overlap), S(extractor_alias), D(extractor_version), List(entity_rule), List(relation_rule), S(normalization), S(embedder_alias), S(model_version), U32(dimensions), D(binding_generation), U32(schema_version), policy |
| request | I(source), D(content_digest), Opt(S uri), pipeline, RevisionRef(expected_source), U8(mode: AtomicDocument=0, Incremental=1), B(original_text) |
| entity_observation | U32(local_id), S(kind), S(key), S(surface), U32(start), U32(end), Attrs, Opt(F64 confidence) |
| relation_observation | U32(subject_local_id), S(predicate), U32(object_local_id), U32(start), U32(end), Attrs, Opt(F64 confidence) |
| accepted_chunk | I(chunk), B(exact_text), List(entity_observation), List(relation_observation), List(F32 accepted_vector) |
| captured | I(any object), RevisionRef(expected), D(canonical stored-property digest) |
| group | U32(ordinal), List(Operation) |
| batch | I(root), U32(ordinal), List(group) |
| plan | D(request_digest), List(captured), List(accepted_chunk), List(batch) |
| delete_action | I(delete_receipt), D(policy_digest), U8(mode) |
| abort_action | I(abort_receipt), D(policy_digest), U8(mode) |

Nested named records are B(record bytes), including policy inside pipeline.
Policy values are the contract ceilings or operator-lowered values, never widened.
Pipeline rule lists sort by unsigned rule-ID UTF-8 bytes with duplicates rejected.
Normalization must be literal `ascii-key-v1`, schema_version=1. Dimensions are
positive; the embedder is explicit. `pipeline_version=H(pipeline domain, pipeline)`.
Content digest must equal SHA256(original_text). Request expected_source permits
Absent or captured Revision, not a previous-batch symbolic value.

Captured rows sort by full identity bytes, with duplicates rejected. Accepted
chunks ascend chunk ordinal; observations use preparation order, not regenerated
model output. Batches ascend from one, groups preserve frozen dependency order
and ordinal, operations preserve section 4's order. No sorting rewrites operation
semantics. All sequences have U32 counts; guards use captured U64 revisions.
Lineage, authorization handles, deadlines, cancellation, allocated IDs and
created/reused execution outcomes are retained in a **noncanonical execution
envelope**, rejected if unexpected at execution; they are not fields accepted by
these canonical records. Unknown fields in a canonical record are errors.

The per-observation 8 KiB measurement is the frozen attributes_key stream length,
excluding the new four-byte outer Attrs length. The 1 MiB decoded-output
measurement is 01 || List(entity_observation) || List(relation_observation),
with nested records framed as above, before deduplication except the contract's
post-dedup relation-output cap. This newly selects a measurement representation;
the unchanged preparation oracle still does not claim either ceiling.
The byte codec checks representation, not every preparation admission invariant:
for example, its empty Request vector exercises syntax, not an accepted empty
source. Cross-field evidence, bounds and operation-order admission remain the
preparation/core validator's responsibility, not a success-shaped fallback.

Operation is a U8 tag followed by exactly these fields (no separate version):

| Tag | Operation | Ordered fields |
|---|---|---|
| 0 | guard source/job | I(source), RevisionRef |
| 1 | remove edge | I(edge), RevisionRef |
| 2 | detach vector | I(chunk), U16(collection), U64(record), RevisionRef |
| 3 | delete vector | U16(collection), U64(record) |
| 4 | remove node | I(source/revision/chunk), RevisionRef |
| 5 | create revision/entity/chunk | Object |
| 6 | vector put | U16(collection), U64(planned_record), vector_data |
| 7 | attach | I(chunk), U16(collection), U64(record) |
| 8 | create provenance/mention/relation edge | Object |
| 9 | create shard | Shard |
| 10 | create batch receipt | Object |
| 11 | head progress | I(source), RevisionRef, head_data |
| 12 | create root receipt, first frame last | Object |

Object = I(role), B(data record for that role). Kinds select the data schema
from section 4, not a caller-controlled opaque blob. Result/ownership shard
operations use Shard's channel to choose the keyed identity.
No expiry, EnsureEdge, unguarded overwrite or raw provider operation exists.
Source creation is represented by tag 11 with Absent; execution uses
UpsertNode(Expected::Absent) or equivalent guarded creation, never a live-key
CreateNode retry. Existing heads alone use guarded PatchNode.
Shared entity tag 5 is a **possible** create, preflight charged as such; reuse
omits only its executed form, not its canonical planned operation.

Only DigestSlot values are masked. Literal request/content/vector/attribute bits
are always included. IdRef(kind=1) and RevisionRef(kind=2) retain symbolic
ordinals; the execution envelope has their resolved values and is excluded
entirely, not by stripping arbitrary keys from payload objects. Changed literal
captured revisions, candidate record IDs or source bytes change the plan.
Compute each batch digest from its masked record, compute plan digest from its
masked record, then fill typed slots. Validate every filled slot against the
appropriate computed digest before commit. No slot participates as an ordinary
literal dependency, so no fixed-point hash is needed.
Only receipt_data.plan_digest uses slot kind 1; only batch_data.batch_digest uses
kind 2. Other digest fields are raw literal D, never inferred placeholders.
Request/Plan/Batch use H with their registry domains. Cleanup request_digest is
H(delete-action, DeleteAction) or H(abort-action, AbortAction), respectively.
For the explicit "delete"/"abort" member of the contract identity tuple, the
identity role tag 13/14 encodes that action enum; no additional string is implied.

Hashes always consume the **frozen prepared record**, never reverse-engineered
executed operations. Executing a batch constructs new physical records from this
view: resolves IdRefs/guards, substitutes actual created/reused action outcomes
in shards and actual anchor counts in its batch receipt, and fills slots. These
permitted runtime projections do not mutate the retained canonical record or its
planned pessimistic counts. Replay validates the stored planned digest against
that retained record, then validates actual IDs/counts against committed shards.
It must not hash runtime-filled shards or receipt counts as a new planned batch.
Fixed-width outcomes/counts and refs keep projected capacity unchanged.

## 7. Frame-size consequences

Canonical identity/plan bytes are not engine op-12 bytes. Every actual shard
counts as one user operation, as do vectors, attachments, evidence, receipts,
head updates and guarded cleanup. Keep the existing section-4 order verbatim.
Exact preflight uses the physical maps, actual engine keys and fixed-width guard
substitutions, reserves the opening reclaim and validates the real engine encoder.

For K engine entries, body bytes = 4 + sum(8 + entry_payload_bytes).
An opening reclaim contributes 8+8=16 bytes. Metadata has an additional U32
length prefix inside each node/edge payload. Exact payload consequences:

| Engine entry | Payload byte count |
|---|---|
| PutNode | 8 + 2 + key bytes + labels bytes + 4 + metadata bytes |
| PatchNode | 8 + added labels bytes + removed labels bytes + 4 + set metadata bytes + unset names bytes |
| PutEdge | 24 + 1 + relationship bytes + 2 + key bytes + 4 + metadata bytes |
| DeleteNode / DeleteEdge / Reclaim | 8 |
| Attach / Detach | 20 / 12 |
| VectorMutation | 4 + length of the existing vector batch entry |

Labels bytes = 1 + sum(1+UTF-8 length); unset names bytes = 1 +
sum(2+UTF-8 length). Use the existing vector encoder for its inner entry,
not the canonical vector_data length. A head absent-create emits PutNode;
a guarded live patch emits PatchNode. Guards themselves need not emit an entry.
Every emitted shard/receipt/evidence/create/vector mutation counts in exact
admission; anchor reuse can reduce the actual list only. Preserve C7's explicit
distinction for internal reclaim versus user-operation counting; reserve its bytes.
Do not treat schema-operation tag count as an engine-entry count.

Outer plaintext/encrypted envelopes add 52/80 bytes, preserving **66/94** for
body14 and **16,777,268/16,777,296** for body16,777,216. Body+1 rejects.
The reference qualifies this arithmetic against hand numbers; it neither runs nor
replaces the engine's exact encoder. No Rust/compiler/frame-reference change.

## 8. Ruling table

All choices are proposed; none alters an approved Gap7-* decision.

| Ambiguity | Options | Proposed choice / rationale | Compatibility / gate |
|---|---|---|---|
| Hash framing | delimiter; fixed domain tag; framed ASCII | version + U32 domain + U32 payload, no concatenation ambiguity | New private v1 bytes only |
| Internal digest | hex64; raw32 | raw32, compact and unambiguous | Text remains lowercase hex64 |
| Binding | ephemeral ids; names+generation | authorized stored names, tenant, opaque32 generation | No lineage/renumbered IDs in keys |
| Nested identities | digest-only; complete | length-framed complete versioned identities | Collision mismatch cannot masquerade as an endpoint |
| Attributes | native C9 values; tagged bytes | retain frozen tags and attributes_key, wrap B | Null/Unsigned need no new engine variants |
| Identity/data properties | one overlong string; fragments | maximal 3,072-byte binary fragments as base64 | Existing String/Integer C9 only |
| Shard backrefs | separate props; header | one `data` string with key/batch/group/index header | Exact 93-byte header for key length 71 |
| Shard entry identity | full; key+lookup | compact key plus captured ID/revision; validate target's full identity on lookup | Full recursive identities would exceed one shard |
| Recursive digest fields | hash filled values; typed mask | tagged 32-zero placeholders only in typed slots | Breaks root/first-batch/plan circularity without a fixed point |
| Result IDs and guards | runtime numeric; symbolic | IdRef 0/1 and RevisionRef 0/1/2 | No predicted sequences; actual record candidates remain frozen |
| Later delete/abort | reuse ingest domain; reserve domains | separate literals/tokens, root shapes retained | No implementation authorization |
| Stored U64 record IDs/revisions | signed narrowing; text | tagged U64 bytes inside base64 data/shard strings | Full range preserved; no unsigned PropertyValue needed |
| Stored spans/counts | bounded i64; text | U32 bytes inside the same data strings | Checked U32; engine Integer used only for g7_v=1 |
| Stored generation | numeric; UUID text; bytes | opaque D, base64 inside full identity | No lossy integer interpretation |
| Stored extractor version | arbitrary text; provisioned fixed token | opaque D shared with pipeline/evidence | Needs ratification; no unbounded evidence property or invented string truncation |
| Encoded attribute / decoded-output measure | JSON; canonical bytes | frozen inner attributes_key / versioned observation lists | New explicit accounting, not retroactive preparation-oracle coverage |
| Captured properties/policy hashes | unframed; shared domain | distinct properties/policy domains with exact metadata/Policy bytes | No undefined hash inputs remain |
| Runtime anchor outcomes | mutate the plan; execution projection | planned pessimistic counts immutable, committed counters/shards projected | Planned digest must never be recomputed from actual outcomes |
| Stored optional URI/job/revision/pipeline/confidence | omitted property; sentinel; presence tag | Opt with explicit 0/1 inside canonical data strings | Absent differs from empty; no C9 Null |
| Cleanup receipt with no resulting revision | Opt; sentinel | empty revision_key only for Deleted/Aborted outcome | Explicit sentinel cannot be a valid graph key; head fields still use Opt |
| Deadlines | persist; execution-only | no stored deadline/TTL; execution envelope only | Existing explicit-delete policy unchanged |
| Edge creation/comparison | keyless; keyed | all four edge roles keyed; every physical property participates | Bit-identical equality, normalize all float zero bits first |
| Node retries | CreateNode retry; guarded create | Absent guard for new keyed nodes; only head can patch | GraphKeyExists is not successful reuse |

## 9. Deliberately not frozen and resumption rule

Not frozen: purge, serving, external provider request/response transport,
on-disk bundle custody, activation, performance, later-slice executable cleanup
algorithms and their exact public API. The domains/receipt identity/action-record
bytes above are reserved for later delete/abort slices; they do not authorize
those slices to run or widen their gates.
Future additions must use new domains/versioned records and reject unknown
versions. Never reinterpret a ratified tag, token, field width or hash input under
the same v1 domain. Keep preparation goldens, raw chunk hashing and frame-size
goldens unchanged. No implementation may cherry-pick this draft as authorization.

## 10. Hand-checkable examples

* U16(258) = `0201`; U32(1) = `01000000`; U64(1) = `0100000000000000`.
* I64(-1) = `ffffffffffffffff`; Bool(true) = `01`.
* S("g") = `0100000067`; S("") = `00000000`; Opt(S("")) = `0100000000`.
* F64(+0), after canonicalizing -0, = `0000000000000000`.
* Attr Null = `00`; Attr Unsigned(1) = `030100000000000000`.
* attributes_key({"a": Null}) = `010000006100`; Attrs of that map =
  `06000000010000006100`, preserving the existing no-count inner representation.
* H input for domain `"vlite.g7.v1.plan"` (16 ASCII bytes) and empty payload =
  `01 10000000 766c6974652e67372e76312e706c616e 00000000`.
  This frames a proposed domain; it is not a valid complete Plan record.
* A 71-byte receipt-key shard header occupies 93 bytes; 93+2,979=3,072 bytes
  gives 4,096 base64 characters. 3,073 raw bytes need 4,100 characters and fail.
* A relation's 8,192-byte Attrs alone requires 8,196 bytes including B length,
  before any endpoint identity/header. This is why shards reference keys and
  validate the separately stored full identity rather than duplicating it.
* Metadata {"a":"x","z":1} =
  `02000000 0100 61 01 01000000 78 0100 7a 02 0100000000000000`.
  This pins U16 name lengths separately from U32 string lengths.
* Tiny binding graph="g", collection="c", tenant="t", zero generation =
  `0100000067 0100000063 0100000074` then 32 zero bytes: 47 bytes.
  Source with tenant="t", source_id="s" is `0101`, that binding,
  `0100000074 0100000073`: 59 bytes. Its key is
  `g7_src_353371371de324dabc2e1a9f912cc12810c0606a5d23142253bb24fef8c9129e`.
* Revision is `0102 3b000000` + those 59 bytes + two raw32 digests: 129.
  Chunk is `0103 81000000` + revision + three U32s: 147. Every other
  identity's full hand-composed atoms are in the fixture, using these same nests.
* Entry(0,0,created,node,"k",actual id1,Absent,no vector fields,false) =
  eight zero bytes, `0000 010000006b 000100000000000000 00 000000 00`.
  Result shard is `0100 0100000072 01000000 00000000 00000000 01000000`
  plus that entry (root key "r"); ownership changes only its channel to `01`.
  These short keys are codec examples, not admissible ingestion graph keys.
* Base64: raw `61` -> `YQ==`; raw `6162` -> `YWI=`; raw `616263` -> `YWJj`.
  A source's empty head data is `01 00 00 00 00 00 00000000`, stored as
  `d00="AQAAAAAAAAAAAA=="`, not native Null properties.
* Guard operation is `00` + framed Source identity + Absent `00`;
  delete-vector is `03 0100 0100000000000000`. Every operation tag has a
  positive byte vector. Plan-empty is `01` + raw32 request digest + three
  zero U32 counts (45 bytes); pipeline minimal codec example is 186 bytes.
  The fixture pins complete Request, populated Plan and Batch nested preimages
  and independently hashed lowercase digest constants. These are byte examples,
  not permission to bypass source/captured-state semantic admission.
