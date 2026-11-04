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
| GetReceipt | Yes: remember, forget, consolidate | receipt lookup accessor (section 3) | `correct_facts`: `facts`; `resume_erasure`: `erasure_jobs` |
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

Unsupported A1-unhashed request fields (`vector`, `metadata`, `evidence`
payloads, `confidence`, `expires_at_ms`, `assertion`) are refused the same way
with `details.feature` equal to the field name, never silently dropped, because
accepting them would need a new digest version (contract section 3).
**PROVISIONAL.**

## 2. Error mapping (core to wire)

Mapping is exhaustive over the crate-private `MemoryError`, `RecallError` and
`RankingError`; a new variant fails to compile until mapped. `Storage(e)` is
delegated unchanged to the existing server-private mapper. The adapter must
validate request shape (scope tuple shape, empty or duplicate bundles, empty
input lists, non-retain dispositions) before calling the core, so these core
variants reaching the adapter mean stored-state or configuration faults.

| Core variant(s) | REST / code |
|---|---|
| `InvalidKey`, `InvalidExpiry`, `InvalidEvidence`, `DuplicateItem`, `OperationLimit`, `BodyLimit`, `InvalidIdentity`, `InvalidContent`, `InvalidImportance`, `InvalidObservation`, `FutureObservation`, `InvalidProvenance`, `ProvenanceLimit`, `InvalidReference`, `CrossScope`, `ExpectedRevisionRequired`, `InvalidTarget`, `DuplicateTarget` | 422 `invalid_input` |
| `InvalidScope` (scope not provisioned in the bound graph) | 409 `memory_not_configured` **PROVISIONAL** |
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

## 3. Crate-private accessors (decision 76)

- **Receipt ID.** `receipt_id(operation, scope, idempotency_key,
  receipt_revision)` returns `rcpt:v1:` plus 64 lowercase hex of SHA-256 over
  the domain `vlite.memory.receipt.id.v1\0`, the length-prefixed receipt node
  key and the 8-byte big-endian receipt revision. It is stable on replay and
  reopen (the revision is the stored receipt's), differs after receipt expiry
  and re-execution (the receipt node is rewritten at a new revision), and does
  not expose the idempotency key. The adapter never computes a receipt key.
  **PROVISIONAL** encoding.
- **Receipt lookup** for GetReceipt (by scope, operation, key, captured clock)
  returning the stored outcome without the original request is not yet in the
  core: remember replay decoding currently needs the normalized request. It is
  the next accessor after binding configuration.
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
 "grants":[{"principal":"key-id","scopes":[{"agent":"a"}]}]}]}
```

- Key `(tenant, binding)` is unique; `tenant` is `@root` or a 32-hex `Tenant.id`.
  Lineage is the server's own and is not repeated per entry. `binding` matches
  `^[a-z][a-z0-9_-]{0,62}$`; `graph` and `evidence_collection` are stored names,
  each owned by at most one entry (active or retired); A1 allows 0 or 1
  evidence collection.
- `receipt_window_ms` is required, 1..2592000000 (decision 70).
  `schema_version`, `ranking_version` and `digest_version` accept only the
  values above. `generation` is 1..2^63-1.
- Grants: each `principal` is a credential ID, at most once per entry; each
  scope is an exact `MemoryScope` (session requires user), unique within the
  grant. Absent principals have no scopes, including admins.
- `state` is `active` or `retired`; `retired` requires `retired_at_ms`, which
  `active` forbids.
- **PROVISIONAL** where the field rules above are silent: `tenant` hex is
  lowercase; `graph` and `evidence_collection` follow the stored graph and
  collection name rule `^[a-z][a-z0-9_-]{0,62}$`, graph names and collection
  names being separate namespaces; `principal` is 1..256 bytes; a grant lists
  at least one scope and every scope is in the entry's own tenant (the JSON
  scope has no `tenant` key).

**Generation rotation rule.** A new set is validated against the running one:

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
   **PROVISIONAL** consequences of rule 5: `retired_at_ms` is not earlier than
   the validation clock of the change that retires the entry, and never
   changes afterwards, so removal can never precede a receipt's retry window.
4. A retired binding serves GetReceipt (and later GetErasure) only; every
   other operation returns 409 `memory_capability_unavailable`
   (`details.feature` `binding_retired`), so retained receipts stay readable
   and no mutation executes under changed policy. **PROVISIONAL.**
5. An entry may be removed only when it is retired and the validation clock is
   at least `retired_at_ms + receipt_window_ms`, the end of every receipt
   retry window it could have issued. Removal earlier rejects the whole set.
6. A graph or evidence collection of a removed entry may not be reused by a
   different `(tenant, binding)` in the same set change.

Receipt lookup is keyed by the bound graph, scope, operation and idempotency
key, none of which rotation can change, so a receipt stays readable until its
own `retry_until_ms`.

## 5. Prerequisite status

| Prerequisite | Status | Remaining |
|---|---|---|
| 1. Final type/error names | Sections 1-3 | Receipt lookup accessor |
| 2. Binding configuration and rotation | Section 4; crate-private `src/memory/binding.rs` (strict parser behind `json`, rotation validator), unwired | Startup wiring with the REST adapter |
| 3. Digest byte goldens | A1/A2/C1 core goldens exist | Loader/verifier for the independent wire fixtures under `tests/fixtures/agent_memory/wire/` once published; existing goldens unchanged |
| 4. Manifest cleanup and restore reconciliation | E4, after E2 | Deferred; not blocking the first slice, whose Forget is small logical deletion |
