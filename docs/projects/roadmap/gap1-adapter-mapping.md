# Gap 1 slice D: adapter-to-core mapping and binding configuration (provisional)

Status: coordinator-directed freeze for the first REST slice, 2026-11-04,
under the owner's standing direction that Gap 1 become consumable. Nothing here
ships a route, OpenAPI/proto change, public Rust API or public `Error` variant
(decision 76). Every choice the [serving contract](serving-memory-contract.md)
leaves open is marked **PROVISIONAL** and may be changed by the owner.
Authority, in order: [serving contract](serving-memory-contract.md), its
[wire conventions](serving-wire-conventions.md), [Gap 1](gap1-agent-memory.md),
the [C1](gap1-c1-consolidation-contract.md) and
[C2 remainder](gap1-c2-remainder-contract.md) contracts. This file overrides none.

## 1. First REST slice inventory

Core on this branch line: A1/A2 remember/forget, B recall, C1 consolidate,
S1/S2 staleness and the E1 erasure barrier. The first slice starts no
erasure job, but a barrier committed by E1 still applies: writes to a barred
scope fail 409 `memory_deletion_pending` and Recall omits the scope with the
`deletion_barrier` truncation reason.

| Wire operation | First slice | Core entry point | Refusal (409 `memory_capability_unavailable`, `details.feature`) |
|---|---|---|---|
| Remember | Yes: `entity`, `episodic` | `MemoryMut::remember` | `fact`/`relation` kinds: `fact_kinds` |
| Recall | Yes | `MemoryRef::recall` | `embedding_provider`: `embedding_provider`; `as_of_ms`: `temporal_read` |
| FindFacts | No | none | always: `facts` |
| CorrectFacts | No | none | always: `facts` |
| Consolidate | Yes: C1, retain only, commit only | `MemoryMut::consolidate` | `dry_run:true`: `consolidate_plan`; `logical_delete` disposition: `consolidate_delete` (X1) |
| Forget | Yes: `ids` target, `logical_delete` | `MemoryMut::forget` | `series`/`entity`: `forget_target`; `session`/`scope`/`allow_job:true`: `erasure_jobs`; `expire`: `forget_expire` |
| GetReceipt | Yes: forget, consolidate | `MemoryRef::receipt` (section 3) | `remember`: `remember_receipt_lookup`; `correct_facts`: `facts`; `resume_erasure`: `erasure_jobs` |
| GetErasure | No | none | always: `erasure_jobs` |
| ResumeErasure | No | none | always: `erasure_jobs` |

Refusals happen after authentication, binding resolution and exact-scope
authorization (contract section 6) and before any lookup, provider or write.
`memory_capability_unavailable` is the contract's code for "core activation
missing" (section 6 table); the feature strings are **PROVISIONAL** values of
the shared `details.feature` context field (wire decision 21).

The core writes no deletion manifest yet (E4). First-slice Forget responses
therefore carry the full `erasure_notice` with `manifest_id` omitted, which the
schema permits but section 5 of the contract expects for logical deletion;
this gap is disclosed, not hidden, until E4. **PROVISIONAL.**

Request fields with no first-slice mapping (`vector` on Remember, `metadata`,
`confidence`, `assertion`) are refused the same way with `details.feature`
equal to the field name, never silently dropped: the A2 digest does not hash
them, so accepting them needs a new digest version (contract section 3).
Correction: the core **does** hash item expiry and evidence references
(`encode_request` writes `expiry` and each evidence `id`/`scope`), so
`expires_at_ms` and evidence references map to the core unchanged and are not
refused for digest reasons. **PROVISIONAL.**

## 2. Error mapping (core to wire)

Mapping is exhaustive over the crate-private `MemoryError`, `RecallError` and
`RankingError`; a new variant fails to compile until mapped. The adapter
validates every contract bound (section 6) before calling the core, so the
request-input variants below are a defensive second check that keeps the
contract's 422 code; stored-state and configuration faults are 500. `Storage(e)`
is delegated to the existing server-private mapper, except
`Storage(InvalidQuery)`, which the memory core raises only for a missing or
mistyped stored property (an integrity fault) and which maps to 500
`internal_error`, not the mapper's 422. A bound graph missing at request time
is 409 `graph_not_found`; a missing evidence collection is 404
`collection_not_found` (existing codes). **PROVISIONAL.**

| Core variant(s) | REST / code |
|---|---|
| `InvalidKey`, `InvalidExpiry`, `InvalidEvidence`, `DuplicateItem`, `OperationLimit`, `BodyLimit`, `InvalidIdentity`, `InvalidContent`, `InvalidImportance`, `InvalidObservation`, `FutureObservation`, `InvalidProvenance`, `ProvenanceLimit`, `InvalidReference`, `CrossScope`, `ExpectedRevisionRequired`, `InvalidTarget`, `DuplicateTarget` | 422 `invalid_input` |
| `InvalidScope` (an authorized scope not provisioned in the bound graph) | 409 `memory_not_configured` **PROVISIONAL** |
| `UnsupportedKind` | 409 `memory_capability_unavailable` |
| `MissingMemory`, `MissingReference` | 404 `memory_not_found` |
| `IdempotencyConflict` | 409 `idempotency_conflict` |
| `RevisionConflict`, `IdentityConflict`, `MergeIdentityConflict` | 412 `precondition_failed` **PROVISIONAL** for the two identity conflicts |
| `RecordAlreadyAttached`, `ForeignAttachment` | 409 `memory_attachment_conflict` |
| `DeletionPending` | 409 `memory_deletion_pending` |
| `JobConflict` (unreachable in the first slice) | 412 `precondition_failed` **PROVISIONAL** |
| `DeadlineExceeded`; recall `Deadline` | 504 `deadline_exceeded` |
| `InvalidClock`, `InvalidReceiptWindow`, `InvalidBundle`, `DigestCollision` | 500 `internal_error` (engine clock, binding configuration or integrity fault) |
| recall `InvalidLimits`, `InvalidScopes`, `InvalidQuery`, `Budget(_)` | 422 `invalid_input` (`Budget` names the bound in `details.field`) |
| recall `UnsupportedPlan` | 422 `memory_bounded_plan_unsupported` |
| ranking `Weights`, `HalfLifeRequired`, `HalfLife`, `Limit`, `AgeOverflow`, `FutureObservation` | 422 `invalid_input`; `memory_unsupported_ranking` is produced by the adapter for refused selection blends |
| ranking `CandidateCount`, `Scope`, `Importance`, `Rank`, `DuplicateRank`, `DuplicateIdentity`, `DenseRanks`, `SegmentBounds`, `NonFiniteScore` | 500 `internal_error` (recall built an invalid ranking input) |
| recall `Cancelled` | no wire code (wire conventions 2.2): never a success, never partial |
| `Storage(InvalidQuery)` from the memory core (missing or mistyped stored property) | 500 `internal_error` (integrity fault) **PROVISIONAL** |
| `Storage(GraphNotFound)` at request bind; evidence collection missing at request bind | 409 `graph_not_found`; 404 `collection_not_found` **PROVISIONAL** |

**Check order (PROVISIONAL).** Every route evaluates, in this order, and stops
at the first failure: (1) authentication and the route permission
(`graph_read` for Recall and GetReceipt, `graph_write` otherwise; 401/403 as
existing routes); (2) binding resolution and exact-scope authorization, where
an unknown binding, a binding of another tenant and a binding with no grant
for the caller's credential on the exact requested scope(s) all return the
same 403 `forbidden_scope` with the same body, so existence is not disclosed;
(3) writable precheck for write operations on a read-only node or replica
(403 `read_only`); (4) capability refusals (409, priority below); (5) request
validation against contract bounds (422 `invalid_input`); (6) the core call.
`memory_not_configured` is returned only for an authorized scope that is not
provisioned in the bound graph.

Raw adapters are distinct from memory binding authorization: after ordinary
grant checks, a reserved resource without owning-binding raw access answers
like a missing resource (`404 collection_not_found`, `409 graph_not_found`),
not like an ungranted binding. Creating under its name answers like an occupied
resource (`409 collection_exists`, `409 graph_exists`); future renames must
preserve that collision rule. Webhook filters accept hidden names, with event
delivery still suppressed. The `delete_node` attachment check remains unchanged.

**Multi-refusal priority (PROVISIONAL).** When several refusals apply, the
first in this list wins: `binding_generation` (the per-write recheck of
section 4, on the mutation routes only); `binding_retired`; the route refusal
of section 1
(`facts`, `erasure_jobs`); `memory_feed_unsupported`
(`require_complete_feed: true`); then field refusals in request field order
(`details.feature` naming the first refused field). `UnsupportedPlan` and the
section 6 partial-plan refusals are 422 `memory_bounded_plan_unsupported` and
come after every 409.

## 3. Crate-private accessors (decision 76)

- **Receipt ID.** `receipt_id(operation, scope, idempotency_key,
  receipt_revision)` returns `rcpt:v1:` plus 64 lowercase hex of SHA-256 over
  the domain `vlite.memory.receipt.id.v1\0`, the length-prefixed receipt node
  key and the 8-byte big-endian receipt revision. It is stable on replay and
  reopen (the revision is the stored receipt's), differs after receipt expiry
  and re-execution (the receipt node is rewritten at a new revision), and does
  not expose the idempotency key. The adapter never computes a receipt key.
  **PROVISIONAL** encoding.
- **Receipt lookup.** `MemoryRef::receipt(operation, scope, key, now_ms)`
  reads the receipt node of the bound graph at the captured clock and returns
  the stored outcome, receipt revision and `retry_until_ms` (the stored
  expiry) without the original request. `now_ms` is the clock seam: the
  adapter captures the server clock once per request and passes it in, so
  tests drive expiry without sleeping. Order: scope shape (`InvalidScope`),
  key (`InvalidKey`), clock at most `i64::MAX` (`InvalidClock`); the scope node
  must exist (`InvalidScope`) with the requested tuple (`DigestCollision`).
  An absent receipt, or `now_ms >= retry_until_ms`, is `None` (404
  `receipt_not_found`). A present receipt, live or expired, decodes strictly
  first: exactly the
  receipt label and properties of its operation, the receipt's own scope key
  (`DigestCollision` otherwise), a canonical nonzero decimal expiry and a
  well-formed outcome; anything else is `InvalidBundle` (500), never `None`.
  Forget yields its sorted `deleted` IDs, each reported at the receipt
  revision (contract section 8 example); Consolidate yields the summary ID at
  revision 1, its C1 receipt also pinning the binding's evidence collection.
  Lookup is a read: an erasure barrier on the scope does not hide receipts.
  **PROVISIONAL.**
- **Remember receipt lookup: refused, persisted-format blocker.** An A1/A2
  remember receipt stores only a 2-bit action code for `created` and `merged`
  items; their IDs and revisions are recovered from the replayed request.
  Answering without the request needs a new receipt encoding, which this slice
  does not invent. GetReceipt with operation `remember` therefore returns 409
  `memory_capability_unavailable`, `details.feature` `remember_receipt_lookup`,
  after authorization and before any lookup. Remember replay with the original
  request is unaffected. **PROVISIONAL** until an accepted encoding change.
- **Reference revision pin: not kept in the first slice.** A1 references are
  bare IDs and the `memory-request-v1` digest hashes them as such; an
  enforceable pin needs a new digest version with byte goldens first. The
  adapter accepts a `MemoryRef` reference only with revision `0` naming an ID
  created in the same bundle (checked from the request alone); any nonzero
  revision is refused with `details.feature` `reference_revision_pin`.
  **PROVISIONAL**: no guard is silently ignored.

## 4. Binding configuration (prerequisite 2)

A binding set is a strict, versioned private JSON document read at startup.
Duplicate or unknown keys, nulls for required fields and fallbacks reject the
whole set; no default is invented.

```json
{"version":"memory-bindings-v1","bindings":[{"binding":"notes","tenant":"@root",
 "generation":1,"state":"active","graph":"notes_graph","evidence_collection":null,
 "schema_version":"memory-schema-v1","ranking_version":"memory-ranking-v1",
 "digest_version":"memory-request-v1","receipt_window_ms":86400000,
 "grants":[{"principal":"abcdefghij27","scopes":[{"agent":"a"}]}]}]}
```

- Key `(tenant, binding)` is unique; `tenant` is `@root` or a 32-hex `Tenant.id`.
  Lineage is the server's own and is not repeated per entry. `binding` matches
  `^[a-z][a-z0-9_-]{0,62}$`; `graph` and `evidence_collection` are stored names,
  each owned by at most one entry (active or retired); A1 allows 0 or 1
  evidence collection.
- `receipt_window_ms` is required, 1..2592000000 (decision 70).
  `schema_version`, `ranking_version` and `digest_version` accept only the
  values above. `generation` is 1..2^63-1.
- Grants: each `principal` is a credential ID matching `^[a-z2-7]{12}$`
  (never a `vlk_` token or secret), at most once per entry; each scope is an
  exact `MemoryScope` (session requires user), unique within the grant.
  Absent principals have no scopes, including admins. `raw_access` is an
  optional strict boolean, default `false` (section 6).
- `state` is `active` or `retired`; `retired` requires `retired_at_ms`
  (at least 1), which `active` forbids.
- Caps (**PROVISIONAL**): the file is at most 1 MiB; at most 256 bindings, 1024
  grants per binding and 256 scopes per grant.
- **PROVISIONAL** where the field rules above are silent: `tenant` hex is
  lowercase; `graph` and `evidence_collection` follow the stored graph and
  collection name rule `^[a-z][a-z0-9_-]{0,62}$`, graph names and collection
  names being separate namespaces; a grant lists at least one scope and every
  scope is in the entry's own tenant (the JSON scope has no `tenant` key).
  `graph` and `evidence_collection` are the caller-visible names; for a
  non-root tenant the stored names are tenant-qualified `t{tenant}__{name}`
  exactly as the existing routes qualify them, and every uniqueness and
  reservation rule applies to the stored names.

**Generation rotation rule.** A set is validated at startup against the
*persisted* previous state, never against a remembered running set
(**PROVISIONAL** implementation): each bound graph holds one crate-private
binding record (tenant, binding, generation, state, `retired_at_ms` and the
rule 1 identity fields) written by startup provisioning on a writable primary
under the graph's write guard. The previous set for validation is the set of
binding records found in the stored graphs named by the new file plus every
stored graph carrying a binding record not named by it (a removed entry). A
graph with no record contributes nothing (first deployment of that entry).
Read-only nodes and replicas compare but never write records. Before every
memory write the handler rechecks, under the same write guard, that the
graph's record still has the generation it serves; otherwise 409
`memory_capability_unavailable`, `details.feature` `binding_generation`.

1. Identity fields of an existing `(tenant, binding)` (`graph`,
   `evidence_collection`, `schema_version`, `ranking_version`,
   `digest_version`, `receipt_window_ms`) never change. The window is hashed
   into every request digest, so changing it would turn a retained retry into
   `idempotency_conflict`; a new window needs a new binding name.
   **PROVISIONAL.**
2. Any change to an entry (grants or state) increases `generation`; an
   unchanged entry keeps it. Generations never decrease. Grants compare as
   written, so reordering them is a change (**PROVISIONAL**). A new entry may
   start at any valid generation.
3. `active` may become `retired`; `retired` never becomes `active`.
   **PROVISIONAL** consequences of rule 5 (anti-backdating): `retired_at_ms`
   is at least 1 and not earlier than the validation clock of the change that
   retires the entry, including an entry first seen already retired (no
   persisted record), and never changes afterwards, so removal can never
   precede a receipt's retry window.
4. A retired binding serves GetReceipt (and later GetErasure) only, and MUST
   keep serving GetReceipt for every receipt until its own `retry_until_ms`
   (retry contract); every
   other operation returns 409 `memory_capability_unavailable`
   (`details.feature` `binding_retired`), so retained receipts stay readable
   and no mutation executes under changed policy. **PROVISIONAL.**
5. An entry may be removed only when it is retired and the validation clock is
   at least `retired_at_ms + receipt_window_ms`, the end of every receipt
   retry window it could have issued. Removal earlier rejects the whole set.
6. A graph or evidence collection of a removed entry may not be reused by a
   different `(tenant, binding)` in the same set change. Rule 6 is scoped to
   that change only; because a removed entry's graph keeps its binding record,
   any later startup naming it under another key sees the same removal again
   and is refused, so reuse needs the graph deleted through the existing admin
   API first (**PROVISIONAL**, conservative).

Receipt lookup is keyed by the bound graph, scope, operation and idempotency
key, none of which rotation can change, so a receipt stays readable until its
own `retry_until_ms`.

## 5. Prerequisite status

| Prerequisite | Status | Remaining |
|---|---|---|
| 1. Final type/error names | Sections 1-3; receipt lookup for forget and consolidate | Remember receipt lookup needs a new receipt encoding (section 3) |
| 2. Binding configuration and rotation | Section 4; crate-private `src/memory/binding.rs` (strict parser behind `json`, rotation validator) | Wired in 0.7.1: stored binding records, the rotation rules at startup and the per-write recheck ([0.7.1 follow-ups](m71-memory-followups.md#1-binding-record-bindingv1-f-01-f5-f6-slice-s2)) |
| 3. Digest byte goldens | A1/A2/C1 core goldens exist | Loader/verifier for the independent wire fixtures under `tests/fixtures/agent_memory/wire/` once published; existing goldens unchanged |
| 4. Manifest cleanup and restore reconciliation | E4, after E2 | Deferred with E2-E4. The first slice writes no manifest, so `restore_requires_manifest` cannot be honoured for its Forget: a DISCLOSED **PROVISIONAL** gap (section 1), not a waiver of the prerequisite |

## 6. Serving rulings (review ACCEPT-WITH-FIXES, all PROVISIONAL)

- **Default off (B1a).** With no `--memory-bindings-file` /
  `VLITE_MEMORY_BINDINGS_FILE` (mirroring `--providers-file`), memory routes
  are not mounted (plain 404) and no reservation guard runs unless the database
  holds binding records (0.7.1), whose graphs and collections then stay
  reserved with no grants; existing routes,
  OpenAPI bytes and `permission()` defaults are unchanged. A configured file
  that is unreadable, over the cap or invalid fails startup with a nonzero
  exit and a redacted message (error class only, never file contents).
- **Reservation (B1b).** Each binding reserves its stored graph and evidence
  collection, per tenant. A crate-private registry (`src/server/reserved.rs`)
  answers whether a stored graph or collection name is reserved and whether a
  credential has `raw_access` for it; raw graph, record, search, export, feed
  and attachment routes refuse reserved resources and filter them from lists
  unless the caller's grant has `raw_access: true`. `raw_access` still needs
  the ordinary underlying grants, is never a memory scope grant, and memory
  grants never imply raw access. An empty registry is a no-op. Raw-route
  guards are implemented on the separate guards lane against this interface.
- **Adoption.** Startup refuses to bind an in-use ordinary resource. A graph is
  adopted when (a) it has no live node and its evidence collection holds no
  record, (b) it already carries a binding record for the same
  `(tenant, binding)`, or (c) once, for a graph a 0.7.0 server adopted before
  records existed: it has no record and holds only memory nodes (each with one
  memory label and the key prefix that label's nodes carry; the evidence
  collection is not inspected). One node with a memory label is not enough
  ([0.7.1 design note](m71-memory-followups.md), section 1.3).
- **Provisioning (B2).** On a writable primary, startup provisions every
  ACTIVE entry's granted scopes idempotently via the crate-private
  `provision_scope`, after the adoption check. Read-only nodes and replicas
  skip provisioning; their writes are refused (403 `read_only`). A missing
  graph or evidence collection, or a graph without graph support enabled, is a
  startup refusal; resources are created through the existing admin API.
- **gRPC.** The gRPC listener has no central coverage of the reservation
  guards, so startup refuses to start it whenever bindings are configured
  (fail closed); zero bindings change nothing.
- **Authorization.** Memory routes use `Reach::Filtered` (never the
  `Reach::Collection` default-grant path). Access is the INTERSECTION of the
  credential's underlying grants on the stored graph (and evidence collection
  when used) and the binding's exact scope grants; `collections: ["*"]` alone
  never confers memory access.
- **Recall.** Text or vector recall on a binding without an evidence
  collection is 409 `memory_capability_unavailable`, `details.feature`
  `evidence_recall`. `require_complete_feed: true` on any mutation is 409
  `memory_feed_unsupported` before writes. `max_references` caps each item's
  references and is the C1 `input_budget` for Consolidate. `allow_partial:
  true` still returns 422 `memory_bounded_plan_unsupported` for candidate,
  vector, evidence and MMR budget exhaustion. A non-summary hit is reported
  `stale: false` (contract example); a summary hit whose stale label is
  unknown is 500 `internal_error`, never guessed.
- **Lengths and characters.** Contract bounds are validated before the core:
  content at most 4096 bytes (A1 chunks it, so the full bound is supported);
  query text above 16384 bytes is 422 `invalid_input`, and 1025..16384 bytes
  (beyond the core hybrid cap of 1024) is 409 `memory_capability_unavailable`,
  `details.feature` `query_text_length`. Every opaque value (IDs, keys,
  binding, source fields) rejects all control characters (U+0000-U+001F,
  U+007F-U+009F) with 422 `invalid_input`.
- **Deletion barrier (E1).** A recall over a barred scope returns 200 with
  `complete: false` and the barrier in diagnostics; GetReceipt for a receipt
  written before the barrier returns 200; a barred recall seed or write is 409
  `memory_deletion_pending`.
- **Forget and Consolidate output.** Forget outcomes are sorted by ID
  ascending, each `deleted` at the receipt revision (replay identical).
  Consolidate returns one `created` outcome for the summary at revision 1.
  Wire `source` maps field-for-field to the core `Provenance` (`source_id`
  to `source`, `source_kind` to `kind`, `uri`, `transformation_id` to
  `transformation`); the wire requires `source_kind`, so the core `kind` is
  always present and no default kind is invented. Consolidate's output
  `transformation_id` is the C1 `transformation`.
- **Remember receipt lookup** stays refused (section 3) and is reported as a
  persisted-format blocker.

### Confirmed at the 0.7.0 freeze (PROVISIONAL)

A live conformance review of the release candidate raised the points below.
Each is a first-slice restriction or a wording choice, recorded here so the
code, goldens, OpenAPI text and user documentation agree.

- **Recall ranking.** `exact` and `mmr` selections are vector-only and refuse
  every blend, weights other than vector-only and a half-life, as 422
  `memory_unsupported_ranking`. Contract section 4 lets `exact` keep text-only
  candidates; serving that needs core work and is a later slice.
- **Approximate mode.** `mode: approximate` is 422
  `memory_bounded_plan_unsupported` (golden `recall-unsupported-ann`): no
  binding owns an approximate index and none is built. `index_not_ready` stays
  for a text index that is not ready.
- **Item evidence.** An `evidence` value that is not an empty list (on the
  request, an item or the consolidate output) is refused as `evidence`; an empty
  list is the same as none. Nonempty references need alias validation,
  ownership, replay hashing and attachment checks together, and are a later
  slice.
- **Body errors.** JSON syntax, duplicate keys, size and media type are
  400/413/415 `invalid_json`. A wrong type or shape, an unknown or missing
  field and an out-of-range value are 422 `invalid_input` with `details.field`
  (contract section 6 and the golden fixtures). The wire conventions' rule that
  a shape failure is `invalid_json` describes the legacy routes, whose
  extractor parses the body.
- **Binding identity.** Wired in 0.7.1 (the 0.7.0 freeze left it unwired):
  each bound graph holds a binding record, every start runs the rotation rules
  against the stored records, and a write rechecks the record's generation. The
  rules and the record are in
  [the 0.7.1 follow-ups](m71-memory-followups.md#1-binding-record-bindingv1-f-01-f5-f6-slice-s2).
  Under 0.7.0 a restart with a changed window made a retry inside its original
  window 409 `idempotency_conflict`.
- **Guard and dedup.** Exact deduplication is decided before the merge
  revision check: a merge that would change nothing answers 200 `noop` at the
  current revision even when its `expected` revision is stale.
- **Expected absent.** `expected` without `merge: merge_provenance`, including
  `{"absent": true}`, is refused as `expected`.
- **Entity seeds.** `entity_seeds` must name entities. A retained episodic
  summary is recalled through text, a vector or a reference from an entity.
- **Default off.** With no bindings file the nine routes are unmounted: the
  administrator sees 404, and a scoped credential sees 403 `forbidden_scope`
  because unmapped routes keep their admin-only `permission()` default.
- **Integers.** An integer input is a canonical decimal string or an exact JSON
  integer. `90.0`, `1e3`, padded strings and strings with leading zeros are
  refused as 422 `invalid_input`.
- **Default provenance.** An explicit empty `provenance` list is the same as an
  absent one and defaults to `[source]`: the core cannot present a memory
  whose source has no kind. Consolidate's output shares this parser. A source
  `uri` follows the opaque-key rules (nonempty, no control character) as well
  as the schema's 256-byte bound.
- **Forget reporting.** Forget reports the deleted memory ids and writes no
  manifest. An evidence record that another memory still uses is retained and
  not reported; contract section 5 expects a report, which is a later slice. A
  missing or null `mode` and a bad `target` entry name `mode` and `target` in
  `details.field`.
- **Recall distance.** Vector recall measures squared Euclidean distance (the
  core's query metric) for `weighted`, `exact` and `mmr`. `exact` orders by it
  and every selection reports it as `scores.distance`; `mmr` selects by cosine
  but reports the squared value. Contract section 4 says `mmr` retains cosine
  distance, and the unreplayed goldens `recall-vector-exact`,
  `recall-vector-mmr` and `recall-mmr-diversity-not-weighted-score-order`
  expect cosine values (1 for orthogonal vectors, where the server reports 2).
  For vectors that are not unit length the order differs from a cosine
  ranking: against the query [1,0], `exact` returns [0.6,0.6] before [10,0].
  Choosing the metric (and replaying those three goldens) is a later slice.
