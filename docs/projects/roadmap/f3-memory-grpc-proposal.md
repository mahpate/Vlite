# F3 proposal: gRPC Memory service and gRPC reservation parity

Status: **approved on 2026-11-05 (D1 to D4 as recommended, through the coordinator); S1 is built and reported (section 5.4); S2 is paused until the owner re-evaluates the plans after 0.7.2.** Lane F3 (surface parity), branch `mahpate-f3-surface-parity`, base `b4673670` (v0.7.1 plus three test-only commits). It follows `serving-memory-contract.md` section 8 (the approved nine-operation inventory, decision 67, and the exact schemas) and `serving-wire-conventions.md` sections 1, 2 and 5. Where it departs from either, section 9 says so. It adds no file format, no stored state and no REST change, so rolling back to 0.7.1 is unaffected; S1 changes no behavior of a running `vlite-server`, and S2 will lift the `--grpc-bind` refusal while memory bindings exist (section 5).

**Approval record.** D1: all nine RPCs are declared now; the four that wait for lane F1 refuse like REST, and the coordinator asks F1 to confirm it needs no other shape. D2: two slices, S1 (reservation parity: bindings published to the gRPC router, the gRPC leg of the generated differential over every existing RPC, the structural scanner leg; no proto change; the startup refusal stays), then S2 (the Memory service, lifting both `--grpc-bind` refusals, docs). D3: the slice lands out of the contract order with only the `Memory` block and the two add-only mapper arms; **this supersedes `serving-memory-contract.md` sections 6 ("gRPC refusal unchanged") and 10 ("gRPC wiring waits for Gap 12") for this lane only.** D4: `Memory` is registered only when a bindings file names a binding. A real REST/gRPC divergence found by S1 is reported at once.

## 1. Decisions asked

| # | Question | Recommendation |
|---|---|---|
| D1 | Declare all nine RPCs now (the four that wait for lane F1 refuse exactly like REST), or only the five that work? | **All nine.** REST already answers the four with 409 `memory_capability_unavailable`; gRPC then answers FAILED_PRECONDITION with the same trailer, and F1 later fills in handler bodies with no proto change. The alternative (five now, F1 appends four) makes an old server answer UNIMPLEMENTED, which a client cannot tell from "not configured". Cost of D1: the four request and response shapes of section 8 are frozen now, so F1 should confirm that it needs no other shape. |
| D2 | Two slices on my branch: **S1** proves gRPC honors reservations (no proto change, startup refusal stays), **S2** adds the Memory service and lifts the refusal? | **Yes.** The lift may only land with the differential and scanner legs, so S1 is their home. S1 can find real divergences between gRPC and REST for the 39 existing RPCs; any fix is reported at once. |
| D3 | Land memory gRPC out of the contract order? The order puts Gap 12 Graph gRPC (step 3), TTL (5) and temporal (6) before memory (7), and none of them is on this base. | **Proceed.** This slice becomes the first consumer of the two new mapper arms (`deadline_exceeded`, `response_too_large`) that the order gives to step 3, and it adds the `Memory` block only; it creates no `Graph` service and touches no shared message. The owner's instruction to pick up lane F3 in parallel already implies the out-of-order landing. |
| D4 | Register `Memory` only when the bindings file names a binding, as REST mounts its routes only then? | **Yes.** Without bindings every `vlite.v1.Memory` call is UNIMPLEMENTED (REST: plain 404) and the `vlite.v1.Memory` health name is not reported. Always registering would add a reachable service to every gRPC server that has no memory. |

Everything else below follows the contract and needs no decision.

## 2. Proto

One new `service Memory` in `package vlite.v1` with the nine RPCs and the messages of contract section 8, copied without change: names, tags, `optional` presence, closed string enums as `string`, `reserved 100 to 199;` in every message. No existing service, message, field or enum changes.

```proto
service Memory {
  rpc Remember(MemoryRememberRequest) returns (MemoryMutationResponse);
  rpc Recall(MemoryRecallRequest) returns (MemoryRecallResponse);
  rpc FindFacts(MemoryFactsRequest) returns (MemoryFactsResponse);
  rpc CorrectFacts(MemoryCorrectionRequest) returns (MemoryMutationResponse);
  rpc Consolidate(MemoryConsolidateRequest) returns (MemoryConsolidateResponse);
  rpc Forget(MemoryForgetRequest) returns (MemoryMutationResponse);
  rpc GetReceipt(MemoryReceiptRequest) returns (MemoryReceiptResponse);
  rpc GetErasure(MemoryErasureRequest) returns (MemoryErasureResponse);
  rpc ResumeErasure(MemoryResumeRequest) returns (MemoryMutationResponse);
}
```

- **Placement.** The block goes between `InfoResponse` and `service Kv`, not at the end of the file. File order has no wire meaning; this keeps lane F2's edits to the Kv block (the end of the file) and mine from touching adjacent lines.
- **Field-number review** (the contract's open item for this slice): checked against the shipped core and the shipped REST JSON, every message maps one to one onto the REST body, with the adapter conversions the contract lists (`MemoryScores` relevance, decay, importance, score to R, D, I, S; `MemoryWeights` to the JSON `weights` triple; `MemoryProvenance.source` flattened; payload bytes to `payload_base64`; `uint64` and `int64` to canonical decimal strings; absent optional to absent, or explicit null where REST renders null). No tag needs to be added, dropped or moved. The implementation proves it with paired literals (section 7); if a mismatch shows up I stop and ask rather than change a tag.
- **RPC short names** are unique across all services (`api_reference.py` keys on them): none of the nine collides with an existing RPC.
- **Reference rows.** Nine `data-api="grpc:<Name>"` rows in `site/api.html`.
- **Descriptor test.** `tests/grpc_smoke.py` `check_additive_wire` removes the reviewed additions (EmbedText, the rerank fields, the Kv messages) before comparing the frozen hash of the approved v1 schema. The Memory messages and service are added to that list with an explicit name list, the nine method signatures and an assertion that every Memory message declares `reserved 100 to 199`. The frozen hash itself does not change.

## 3. Semantics

### 3.1 One pipeline, the REST handlers' own logic

Every RPC runs through `Adapter::call`, the same pipeline as the other 39 RPCs: admission permit, authenticate, per-credential rate limit, `authorize_need`, then the core inside `guard::track_hidden`, then `record_request`. It adds `x-request-id` to success and failure.

| RPC | Route label | `Need` | Read-only token passes the pipeline | Core |
|---|---|---|---|---|
| Remember | `grpc:/vlite.v1.Memory/Remember` | `Op(GraphWrite, Filtered)` | no | `remember_in` |
| Recall | `.../Recall` | `Op(GraphRead, Filtered)` | yes, then the handler answers the single 403 | `recall_in` |
| Consolidate | `.../Consolidate` | `Op(GraphWrite, Filtered)` | no | `consolidate_in` |
| Forget | `.../Forget` | `Op(GraphWrite, Filtered)` | no | `forget_in` |
| GetReceipt | `.../GetReceipt` | `Op(GraphRead, Filtered)` | yes, same | `lookup` |
| FindFacts, GetErasure | `.../FindFacts`, `.../GetErasure` | `Op(GraphRead, Filtered)` | yes, same | refusal |
| CorrectFacts, ResumeErasure | `.../CorrectFacts`, `.../ResumeErasure` | `Op(GraphWrite, Filtered)` | no | refusal |

These are the classifications `server.rs` `permission()` and `memory::read_route` and `write_route` give the nine REST routes today.

The decisions that matter (binding resolution, exact-scope authorization, writable precheck, binding-generation recheck, refusal priority, item and evidence validation, the receipt digest, the receipt window) are not re-implemented. The REST handlers already split into a transport part (axum extractors, `Caller`, `Bytes`) and transport-neutral bodies: `remember_in`, `consolidate_in`, `forget_in`, `recall_in`, `lookup`, `resolve`, `Bound::authorize`, `mutation`, `refuse`. S2 splits the five REST wrappers once more, so that each is "extract, then call a flow taking `(Call, binding name, decoded body)`", and the gRPC adapter calls the same flow. REST behavior is unchanged and is proven so by the existing REST memory tests (`tests/server_memory*.rs` and the `pins_*` unit pins). The alternative, a second copy of the sequencing beside the REST one, touches no REST code but can drift; I do not recommend it. The flows live in a child module `routes/rpc.rs`, so they see `routes.rs`' private items; the gRPC side is `src/server/grpc/memory.rs`, like `grpc/kv.rs`.

Order of failure is the REST order: binding resolution (a single 403, before the request is converted), then conversion (422), then exact scope (403), then the handler's own steps.

### 3.2 Request and response conversion

The adapter converts the decoded proto message to the canonical request JSON the REST parser reads, calls the shared flow, and converts the response JSON back. Rules:

- **Presence.** The converter emits a JSON key only for a set field. A message field that is unset is an absent key, so REST's own required-field checks answer it (422 `invalid_input` with the field in `details`). A non-optional `string` cannot be told from unset: an empty required string is rejected, as the contract says ("Missing required nonoptional strings reject empty"), even for `content`, where REST accepts an explicit `""`. A zero `revision` or `generation` that must be nonzero is rejected, except `MemoryRef.revision` 0 for an ID created in the same bundle.
- **Empty lists and maps.** Protobuf cannot say "present but empty". `scopes` empty means `[scope]`, `kinds` empty means all kinds, an empty `metadata` map means absent; JSON rejects or refuses the explicit empty forms. This is the contract's published difference, listed in `docs/GRPC.md`; it never widens authorization (an empty `scopes` still authorizes `scope`).
- **Integers.** Where REST accepts a decimal string or an exact integer (`MemoryU64`, `MemoryI64`) the converter emits the canonical string. Where REST accepts only a JSON integer (`deadline_ms`, `max_references`, `limit`, the recall limits) it emits a number. A per-field table test pins the choice against the REST parser.
- **Floats.** A non-finite `float` or `double` is 422 `invalid_input` naming the field, never coerced.
- **Unknown request fields** are dropped by prost (contract: decision 72), so any later field that changes scope, filter or policy meaning needs a new service version, not an added field.
- **Response.** The REST JSON is built by our own handlers; a response key the converter does not know, or a missing required key, is an internal bug and answers INTERNAL `internal_error` (logged, never silently dropped).

### 3.3 Status mapping

All rows use the existing mapper (`grpc.rs` `status_code`) except the two marked new. Existing arms stay and are not reordered; the new arms are added before the generic `(400..=499, _)` arm.

| REST | gRPC | Notes |
|---|---|---|
| 401 | UNAUTHENTICATED | |
| 403 `forbidden_scope`, `read_only`, `tenant_*` | PERMISSION_DENIED | one answer for unknown binding, binding of another tenant, ungranted binding, non-scoped role, scope outside the grant |
| 404 `memory_not_found`, `receipt_not_found`, `erasure_not_found` | NOT_FOUND | only after binding and scope checks pass |
| 409 `memory_*`, `idempotency_conflict`, `index_not_ready`, `binding_*` | FAILED_PRECONDITION | includes `memory_capability_unavailable` for facts and erasure |
| 412 `precondition_failed` | ABORTED | `details.field` names revision, generation or job |
| 422 `invalid_input`, `memory_unsupported_ranking`, `memory_bounded_plan_unsupported` | INVALID_ARGUMENT | |
| 429 | RESOURCE_EXHAUSTED | `retry-after: 1` metadata |
| 503 `busy`, `unavailable` | UNAVAILABLE | |
| 503 `commit_indeterminate` | UNKNOWN | reconcile by receipt |
| 503 `poisoned`, 500 | INTERNAL | |
| **new** 504 `deadline_exceeded` | DEADLINE_EXCEEDED | before the commit point only; today this falls through to INTERNAL |
| **new** 413 `response_too_large` | RESOURCE_EXHAUSTED | adapter-built, see 3.5; gRPC only, REST has no response cap |

Trailers: `vlite-error-code` always; `vlite-error-details-base64` is the base64 of the same JSON the REST body carries under `error.details` (`{"feature":...,"field":...}`, static names only, never caller data) and is absent when REST has no `details`; `x-request-id`. A request over 8 MiB is tonic's OUT_OF_RANGE with no trailer, and an expired `grpc-timeout` is CANCELLED with no trailer; both are accepted (wire conventions 2.2).

### 3.4 Idempotency and receipts

The key is in the body, one per logical request. The receipt digest is computed over the typed core request after conversion, so a REST and a gRPC client sending the same logical request replay each other's receipts (`replayed: true`, same receipt ID). A receipt written over one transport is readable by `GetReceipt` over the other. Both directions are tested.

### 3.5 Limits

- **Request:** tonic decode cap 8 MiB (equal to the REST body cap); item, content, provenance, reference and deadline bounds live in the shared parser.
- **Response:** the `Memory` handlers check the encoded length of the response before returning it. Above 16 MiB the answer is RESOURCE_EXHAUSTED `response_too_large`, never a truncated success. Only Recall can approach the cap; the largest legal mutation, receipt and refusal responses are far below it, and a test pins that. I do not put the check in `Adapter::call`: that would change the failure of the existing 39 RPCs from OUT_OF_RANGE to RESOURCE_EXHAUSTED, which is outside this lane.
- **Rate and admission:** the existing per-credential bucket, one charge per call including replay and receipt lookup, and the shared in-flight permit (503 `busy`). No new default or exemption.
- **Quota:** memory writes go through `run_mutation` as the REST routes do; no new accounting.
- **Deadline:** `MemoryMutation.deadline_ms` and `MemoryLimits.deadline_ms` keep the REST meaning (default 250 ms, ceiling 5,000 ms, excluded from the digest). The gRPC call deadline is separate; a dropped handler never rolls back a commit because the pipeline runs in its own task.

### 3.6 Audit and observability

Each call writes one `memory_request` audit entry exactly as REST does: operation, binding, principal, `actor` (`audit::actor(role, None)`; gRPC has no dashboard session), outcome, receipt ID and sequence, never memory text, keys or identifiers. Metrics and logs carry the route label `grpc:/vlite.v1.Memory/<Rpc>`, status and code only.

## 4. How F1 adds facts and erasure

The four RPCs exist from S2 and refuse like REST: binding resolution, exact scope, the writable precheck for the two writes, then the binding's refusal (`facts` or `erasure_jobs`, or `binding_retired`) as FAILED_PRECONDITION with `memory_capability_unavailable` and `details.feature`. For them the converter reads only `binding` and `scope`. When F1 turns a capability on it (1) replaces the REST handler, (2) extends the converter for that request and the matching response (`MemoryFactsResponse`, `MemoryErasureResponse`, the `erasure` blocks of the mutation response), (3) removes the refusal for that RPC. No proto change, no new decision. A new field on a facts or erasure message uses the next free low tag and, if it changes scope, filter or policy meaning, a new service version (decision 72). The paired-literal fixtures for those shapes already exist in `tests/fixtures/agent_memory/wire/` (`facts.json`, `correct_facts.json`, `erasure.json`, `resume_erasure.json`).

**F1 confirmation (2026-11-05).** Lane F1 confirmed that these four RPCs need no other message shape than contract section 8, and that slot mode stays off the wire (bindings file only). The erasure adapters are coded against the crate-private core signatures `MemoryRef::erasure_job(&ErasureJobRequest) -> MemoryResult<ErasureJob>` and `MemoryMut::resume_erasure(&ResumeErasureRequest) -> MemoryResult<ResumeErasureResult>` (F1 branch `mahpate-f1-temporal-memory`, `gap1-e2-resume-contract.md` section 2). GetErasure requires `job_id` to equal the returned `job_key`, otherwise the existing `erasure_not_found`. ResumeErasure maps `MemoryResumeRequest` 1:1 to the core request. The window and the clock are binding-level and server-side, and `deadline_ms` is converted by the adapter as for forget. `MemoryErasure` carries `job_key` as `id`, plus `revision`, `generation`, `state`, `logical_visibility`, `deleted_count` and `remaining` as `remaining_count` (null when unknown), and `resume_token` only while the job is pending; the chunk counter stays internal. The REST 200 (logical) versus 202 (pending) split is adapter-level. The facts core entry points (`MemoryRef::facts`, `MemoryMut::correct_facts`; names provisional) land with F1's E2; until then the four RPCs keep the refusal.

## 5. Lifting the `--grpc-bind` refusal (reservation parity)

### 5.1 Why the refusal exists

`configured_admin_router` builds its REST router over its own copy of the `Service`: `with_bindings(...)` replaces `memory_bindings` on that copy. The gRPC router is built afterwards from the caller's copy, which never receives the bindings. Only the admin state is shared (`published_admin`, set in `configured_routes`, adopted by `grpc_router`). So today a gRPC router has empty reservations: the per-request guard (`RequestScope::new` reads `service.memory_bindings`) and every shared `*_core` check of the form `if !service.memory_bindings.is_empty()` are inert on gRPC. `configured_admin_router` therefore refuses `--grpc-bind` while bindings exist (`admin.rs` 235-239) and while the database holds reservations that no file names (`admin.rs` 250-253).

### 5.2 Design

- `configured_routes` publishes the final bindings (after `with_bindings` and `with_orphans`) next to the admin state, and `grpc_router` adopts both or neither. A router that protects less (no admin token, or no reserved resources where the held publication has them) never replaces the publication of one that protects more, so a plain router built later cannot leave a listener without the tokens and reservations of the strictest router of its engine. A `grpc_router` over a service whose REST router was never configured keeps its documented development-only behavior (no admin token means every caller is the administrator, and nothing is adopted); that is unchanged and is not a production path, because `vlite-server` always configures the REST router first. A `grpc_router` built before any REST router keeps that behavior too, but remembers it: `configured_admin_router` then refuses to configure the same service. Every `grpc_router` is remembered as well, and from then on `configured_admin_router` refuses a router that reserves memory resources (the bindings of its file, or records found in the database): once before startup writes anything, and again under the publication locks when it publishes, the same locks under which an adoption records itself, so a `grpc_router` built at any moment either sees the reservations or is seen by the check. `router` and `authenticated_router` cannot refuse and log a warning for the first order. The listener adopts once, when it is built; a router built after it changes what REST applies and not what the listener applies, except for the refusal above. Unit tests (`src/server/published.rs`) pin: both adopted together, neither adopted by an unconfigured service, a poisoned lock recovered rather than skipped, a later router replacing the publication for listeners built after it, a router that reserves nothing (`router`, `authenticated_router`, a configured router with no bindings and no reserved records) never replacing the publication of one that does, a router without an admin token never replacing one that has one, a router that holds reserved records without a file still reserving, the REST-before-gRPC refusal, and a reserving router refused once a gRPC router exists (with a file, with reserved records, and at publication).
- Both refusals in `admin.rs` are removed in S2, not S1. Until then the gRPC differential builds its own world (`configured_admin_router` then `grpc_router`, which the tests already do) and proves the adopted reservations work.
- `Memory` is registered when the adopted bindings are configured (D4). `GrpcHealth` reports the names it registered.
- Unchanged: automatic index maintenance is still refused while bindings exist; imports, replication export, metrics and webhook list stay 403 for every credential, on gRPC through the same shared cores.

### 5.3 The two proofs that must land with it

1. **A gRPC leg of the generated differential** (`tests/server_guard_differential.rs`, under `#[cfg(feature = "grpc")]`, reusing its world, 12 principals, seeds and protected-resource digest). The inventory is read from the `rpc` lines of `vlite.proto`: every RPC of `VLite`, `Kv` and `Memory` has a row or an `EXEMPT` entry with a reason, so a new RPC cannot ship unchecked. The three properties of the REST differential are asserted per RPC and principal: a reserved name answers like a missing one (status code, `vlite-error-code`, message; a create answers like an occupied name); ordinary names answer like a database without bindings; after every non-read request every protected resource reads back unchanged. It includes the three streaming RPCs (`WatchChanges`, `ExportVectors`, `StreamImport`) and every RPC whose REST twin is refused or filtered while bindings exist (imports, export, change feeds, info). It needs a mutant: a `grpc_router` that does not adopt the bindings must fail it.
2. **A structural leg of the guard scanner** (`tests/server_name_lookup_scan.rs`). The text scan already reads `src/server/**`, so it covers `grpc.rs`, `grpc/*.rs` and the new `routes/rpc.rs`; a test asserts that those files are in the scan set. A new structural test asserts, per service impl (`VLite`, `Kv`, `Memory`), that the methods equal the proto's RPC list, that each method makes exactly one `self.call(` and nothing else reaches `service.run*` or the database directly, and that each route label is `grpc:/vlite.v1.<Service>/<Rpc>` for its own RPC. The 0.7.1 handover lists making the scanner structural as a 0.7.2 item; this is the gRPC part of it.

### 5.4 Result of S1

S1 changed no proto, no REST behavior and no `vlite-server` startup behavior: `--grpc-bind` still refuses while bindings or reserved records exist. A program that builds the routers itself gets the refusals described in section 5.2.

- **Adoption.** `grpc_router` adopts the tokens and the reservations of the REST router it is built after, both or neither (`Service::publish` and `adopt_published`, `src/server.rs`). A router that protects less never replaces the publication of one that protects more. A `grpc_router` built before any REST router is remembered, and `configured_admin_router` then refuses the same service; once any `grpc_router` exists, `configured_admin_router` refuses a router that reserves memory resources.
- **The differential leg** (`tests/server_guard_differential/grpc_leg.rs`, `grpc_checks.rs`) runs every RPC the proto declares (39: 35 `VLite`, 4 `Kv`) for the eight token principals, because gRPC authenticates bearer tokens only, with and without bindings, and runs the three checks of the REST differential on each (it never compares a REST answer with a gRPC answer). The inventory is read from the proto: a new RPC needs a row, or an `EXEMPT` entry for a binding-addressed RPC that `tests/grpc_memory.rs` will cover in S2 (nothing checks that claim yet: S2 must make each entry require that file to call the RPC); a request field that looks like a name but is not called `collection` must be varied by a row or be listed in `NOT_A_NAME`.
- **The scanner leg** (`tests/server_name_lookup_scan/grpc_leg.rs`) reads every `impl` of `Adapter` and every function of the gRPC files. A handler makes one pipeline call under its own route, read from code, and takes nothing of the pipeline or the engine outside it. An engine helper (`apply_chunk`) is reachable from a pipeline call only, and the two stream continuations must take their steps as calls whose results they use.
- **Mutants:** `tests/f3_mutants.json` (`k71_mutants.py --list`), 47 in all: adoption and publication (`f3-s1/P1`..`P12`), the guard in the handlers (`G1`..`G4`), the scanner (`S1`..`S4`) and the C ABI key-value operations (`f3-kv-abi/K1`..`K27`).

What S1 found, none of it changed here:

| # | Finding | Status |
|---|---|---|
| 1 | The administrator's `SearchDiskAnn` on a named path that does not exist answers `internal_error`; REST and every other gRPC caller answer 409 `disk_ann_not_built`. | Pre-existing, admin only. Left as is (a changed answer is a wire change); the differential row builds the sidecar first. |
| 2 | The administrator names the disk ANN `path`, and nothing ties the file to the collection the request addressed: the header `collection_id` is not compared with the guarded collection. A path that holds the sidecar of a collection that is reserved by a binding would be searched under an ordinary name. | Pre-existing, admin only, latent: unreachable on a running server while the startup refusal stands. **Must be fixed before S2 lifts the refusal**: compare `index.source().collection_id` with the addressed collection (else `disk_ann_not_built`) and refuse administrator paths inside `VLITE_ADMIN_DIR`. |
| 3 | `export_page` authenticates again for each page but takes no rate limit and does not check the operation again. | Pre-existing, left as is (a throttled stream is a behavior decision). |
| 4 | Before S1 a library embedder that built the REST router with bindings (`grpc_enabled: false`) and then `grpc_router` got a gRPC listener that applied none of the reservations. `vlite-server` was never exposed: it refuses `--grpc-bind` with bindings. | Fixed by the adoption above. The release notes should say it is a security fix for library embedders. |
| 5 | Lane F2 adds two RPCs to `Kv` (`GetKvEntries`, `WriteKvBatch`, both with a `collection` field). The inventory test fails on the integration branch until each has a row in `rows()`; the scanner reads their handlers as it reads the rest. | Integration step; the coordinator added the two rows on the staging branch (`5e280681`). |

## 6. Security review (dedicated pass, one of the three reviewers)

The reviewer is given: unknown, ungranted, other-tenant and non-scoped answers are byte-identical apart from `x-request-id` (code, message, trailers, no `details`); resolution happens before conversion, so an invalid body never reveals whether a binding exists; the read-only token and the admin token never reach memory data; exact-scope authorization has no parent, child or sibling fallback; `x-request-id`, audit and metrics never carry names, keys or text; `details` carries static names only; rate limit, admission and size handling are the REST ones; adopted reservations equal the REST router's (S1 mutant); no REST response, audit or error body changes; `response_too_large` never follows a partial or committed-but-unreported write.

## 7. Test plan

| Test | Proves |
|---|---|
| `tests/grpc_memory.rs` (feature `grpc`) | the five live RPCs and four refusals end to end on a real listener: success and every error class through its gRPC code and trailers; presence and empty-list rules; read-only and write prechecks; the oversize and 429 paths; health; `UNIMPLEMENTED` with no bindings |
| paired literals | per RPC about thirty cases written from the contract, as the REST request JSON and the equivalent proto message, with the expected REST response; the converter is checked against both, so a field forgotten by the converter fails the case |
| cross-transport equivalence | the same logical request over REST and gRPC on twin databases gives equal normalized responses; receipts replay in both directions |
| authorization matrix | principals by operations: only the allowed succeed, every denial identical |
| descriptor and tag golden | `grpc_smoke.py`: names, tags, signatures, `reserved 100 to 199`, frozen hash unchanged |
| gRPC differential and scanner legs | section 5.3 |
| updated startup tests | `server_memory_binding_record.rs` (393, 446) and `server_memory_guards.rs` (1810) change from "refuses" to "starts and serves"; mutant entries that target the removed refusals in `memory_followup_mutants.json` are retired, with the new behavior covered by new mutants |
| hand mutants | one per build for the conversion, order of failure, the two mapper arms, the preflight and the adoption; named killing tests; ledger `tests/f3_mutants.json` in the `k71_mutants` convention; replayed on a second machine through the worker pool |

## 8. Files and slices

- **S1** (no proto change): `src/server.rs` (publication, about ten lines), `src/server/grpc.rs` (adoption), `tests/server_guard_differential.rs`, `tests/server_name_lookup_scan.rs`, and a fix with a named test for any divergence the leg finds.
- **S2**: `proto/vlite/v1/vlite.proto` (Memory block), `src/server/grpc.rs` (registration, health names, the two arms), `src/server/grpc/memory.rs`, `src/server/memory/routes.rs` and `routes/{recall,consolidate,forget,receipt,refused}.rs` (the extract-then-flow split) plus `routes/rpc.rs`, `src/server/admin.rs` (both refusals removed), `tests/grpc_memory.rs`, `tests/grpc_smoke.py`, `docs/GRPC.md` (Memory section, presence differences, status table), `docs/MEMORY.md`, `docs/API.md`, `docs/SECURITY.md`, `site/api.html` (nine rows). Hot files get add-only blocks. The coordinator publishes.

## 9. Departures and open points

- Contract section 6 says the gRPC refusal is "unchanged" and section 10 says gRPC wiring waits for Gap 12. The owner's instruction for lane F3 supersedes both; section 6 and `docs/MEMORY.md`, `docs/API.md` and `docs/SECURITY.md` are updated with the lift.
- The contract names `src/server/memory_grpc.rs`; the adapter goes to `src/server/grpc/memory.rs`, where `grpc/kv.rs` already lives.
- Contract section 8 describes `client.memory(binding)` facades; wire conventions decision 10 chose flat client methods. That is client work (d), not this slice.
- S1 may reveal that an existing gRPC RPC answers differently from its REST twin while bound. That would be a security finding in shipped 0.7.1 code (reachable only with `--grpc-bind` and bindings, which startup refuses today). I report it at once and fix it in S1 with a named test.
- Effort, relative: S1 is medium (the leg is the bulk), S2 is large (about forty converter messages and the test matrix).
