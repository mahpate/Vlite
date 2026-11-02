# Serving wire conventions

Status: shared design conventions owner-approved on 2026-11-02; section 6 records all 23 choices. Docs only: no implementation acceptance, gate activation, release approval or new public `Error` variant; additive v1 only (BASE:209). This note settles the cross-document conflicts found when the ten serving drafts were read side by side: proto tags and `HybridHit` (C1, C16), error codes (C2-C9), the `GraphBatch` op union (C10), the change feed (C11), encodings (C12), client and MCP shapes (C13), shared files, health and caps (C14) and operationIds (C15). Remaining feature-specific endpoint, signature, field and error-code approvals are not granted by these shared choices. Conflicting proposal text in sibling drafts must follow this record; section 7 tracks the remaining conformance work, not completed implementation.

| ID | Document in `docs/projects/roadmap/` | Scope |
|---|---|---|
| BASE | `serving-contract.md` (approved) | Gap 8 policy, Gap 12 section, delivery order (BASE:253-255) |
| G8 | Gap 8 candidate for BASE (ae30f932, about 144 added lines) | provider registry, embeddings, custom ranking |
| GR12 | `serving-graph-grpc-contract.md` | Gap 12 `vlite.v1.Graph` |
| KV | `serving-kv-ttl-contract.md` | Gap 2 key-value and Gap 3 graph TTL |
| TMP | `serving-temporal-contract.md` | Gap 4E temporal |
| MEM | `serving-memory-contract.md` | Gap 1D memory |
| ALG | `serving-graph-algorithms-contract.md` | Gap 5 S5 algorithms |
| G6, G7 | `gap6-cypher-subset.md`, `gap7-graphrag-ingestion.md` | Cypher subset, GraphRAG ingestion |
| P5 | `serving-binary-results-contract.md` | binary results |
| PRC | `serving-precision-contract.md` | P3 precision API |

Citations: `file:line` is source at e88ca853; `BASE:n` is a line of `serving-contract.md` at e88ca853; `tonic` is tonic 0.14.6 source (Cargo.lock); `ID §n ~line` is a section and approximate line of a working draft. These historical citations explain the proposal; the owner walkthrough used the shared note at 1d531a26 and the preceding P3b/P3c, G6 and G7 approval records. Drafts continue to move by lines per edit, so section names and decision IDs are the stable anchors; re-pin source and exact schemas at each implementation review.

## 1. Proto manifest

### 1.1 Services (all `package vlite.v1`, appended to `proto/vlite/v1/vlite.proto`)

| Service | RPCs in landing order | Health name | Owner |
|---|---|---|---|
| `VLite` | 34 baseline (`vlite.proto:16-60`), `EmbedText` (BASE:199), `CreateCollectionWithPrecision` (PRC §4 ~165) | `""` and `vlite.v1.VLite` (`grpc.rs:27,66-68`) | existing; G8 and PRC append |
| `Graph` | 17 (BASE:137-153); then appended in landing order: `GetNodeExpiry`, `GetEdgeExpiry` (KV §7 ~551-552), `WriteExpiryBatch` (KV ~553; only a wrapper of the band-400 message, 1.3), `EdgeTimeline` (TMP §7 ~766), five ALG RPCs (ALG ~839-843); no absolute total is promised, because each slice appends to whatever it finds | `vlite.v1.Graph` | GR12; later slices append |
| `Kv` | 7: `GetKvEntry`, `PutKvEntry`, `DeleteKvEntry`, `ListKvEntries`, `AcquireLease`, `RenewLease`, `RevokeLease` (KV §7 ~507-515; the four bare verbs renamed, 1.4) | `vlite.v1.Kv` | KV |
| `Memory` | 9: `Remember`, `Recall`, `FindFacts`, `CorrectFacts`, `Consolidate`, `Forget`, `GetReceipt`, `GetErasure`, `ResumeErasure` (MEM §8 ~691-700) | `vlite.v1.Memory` | MEM |
| `GraphTtl` | not created; its RPCs fold into `Graph` (decision 2) | none | KV |
| none (package `vlite.serving.v1`) | messages only, no service (P5 ~151-203, 341) | none | P5 |

### 1.2 Per-service obligations (C14)

| Item | Rule | Evidence |
|---|---|---|
| Registration | one `add_service` per service on the shared `Routes`; adapters in their own modules; `grpc.rs` keeps router, health, caps and the shared error mapper | `grpc.rs:85-105` (`Routes::new` at 100) |
| Message caps | every service sets 8 MiB decode and 16 MiB encode itself; tonic defaults are 4 MiB receive and unlimited send | `grpc.rs:21-25,98-99`; tonic `codec/mod.rs:101-102`; BASE:177 |
| Health | the service name joins `GrpcHealth::set`, the `set_not_serving` doc comment and the `grpc_smoke` health loop | `grpc.rs:66-73`; `grpc_smoke.py:184-186` |
| Descriptor test | one test lists every service, every RPC short name (unique) and the tag manifest of 1.3; each slice adds named entries | BASE:209; GR12 ~931 |

### 1.3 Field-number allocation (C1)

Gap 12 shared messages are `GraphSelector`, `GraphLimits`, `GraphProjection`, `GraphReadMeta`, `GraphSchema` and `GraphWriteOperation` (BASE:203). The bands apply to every one of them and to any other Gap 12 message a sibling extends (the read requests, `GraphEdge`).

| Tags | Owner | Use |
|---|---|---|
| 1-99 | GR12 | core fields; `GraphWriteOperation` arms 1-13 (BASE:204); retain its `reserved 14 to 31` for future core atomic graph-batch operation arms (decision 20) |
| 100-199 | ALG | unused unless a shared field is needed; ALG keeps its own `GraphAlgorithm*` messages |
| 200-299 | TMP | at most one field or arm per shared message, first tag 200 |
| 300-399 | none | held for MEM if it ever needs a `WriteGraphBatch` arm; it has none today (own `Memory` service) |
| 400-499 | KV (TTL) | at most one field or arm per shared message, first tag 400 (GR12 labels the band "KV/TTL"; KV is a separate service) |
| 500 and up | nobody | needs another exact-schema review (GR12 §3 ~76-77) |

| Mechanic | Rule |
|---|---|
| Fences | GR12 declares the four `reserved` fences in every shared message, as it does in `GraphWriteOperation` (GR12 §3 ~239-243). The band owner's slice carves exactly one tag out of its band (`reserved 200 to 299` becomes `reserved 201 to 299`) and adds the field |
| Core operation hold | GR12 / the Gap 12 core graph protocol owner owns `GraphWriteOperation` tags 14-31 for future core atomic graph-batch operation arms. Keep them reserved until a separate exact-schema review names an operation and explicitly releases its tag. This is not permission to allocate one now, nor a reservation of 14-31 in every other message |
| Arms wrap domain messages | the field or arm holds a domain-owned message with its own oneof and `reserved 100 to 199`; no domain field is added directly to a shared message |
| Proof | the descriptor test asserts that every tag of 100 or more in a shared message is its band's first tag; each landing adds a named entry (GR12 ~931) |
| TMP | `TemporalWriteOperation temporal = 100` cannot compile against `reserved 100 to 199` (GR12 §3 ~239-243). It and the other tag-100 additions on shared messages (`TemporalSelector` on the nine read requests, `TemporalContext` on `GraphReadMeta`, `TemporalBounds validity` on `GraphEdge`; TMP §7 ~771-777) become tag 200 |
| KV | the TTL arm is `GraphExpiryOperation expiry = 400` in `GraphWriteOperation` (message at KV ~572); `WriteExpiryBatch` only wraps the same message, so a mixed create-and-TTL batch is one `WriteGraphBatch` call (KV ~589-590) |
| Reversal | GR12 recommends permanent fences with sibling messages and RPCs only (GR12 ~71-77, 1026, 1029). This note makes the fences sibling-owned so mixed batches stay atomic (BASE:152; KV ~589-590; TMP ~777): decision 1 |

Baseline-owned messages carry no fence; they take the next free low tag in landing order. These numbers are proposals that the descriptor test fixes at landing (BASE:209: never reuse a tag).

| Message.field | Tag | Step | Replaces |
|---|---|---|---|
| enum `GraphEventFeature` (`TTL`, `TEMPORAL`) | new enum | 5 | KV `graph_ttl` bool, TMP `temporal`, `feed_version` |
| `ChangesRequest.graph_events` (repeated) | 3 (`vlite.proto:455` uses 1-2) | 5 | KV `graph_ttl=3`; TMP `temporal=100` |
| `WatchChangesRequest.graph_events` | 4 (3 is `poll_interval_ms`, `vlite.proto:456`) | 5 | KV `graph_ttl=4` |
| `ChangesResponse.graph_events` (echo) | 5 (`vlite.proto:525` uses 1-4) | 5 | TMP `feed_version=100` |
| `GraphChangeOperation` TTL fields (`expires_at_ms`, `expiry_changed`, `removal_reason`, `removed_at_ms`) | 19-22 (`vlite.proto:487` uses 1-18) | 5 | KV §7 ~609-613 |
| `GraphChangeOperation` temporal arm | 23 | 6 | TMP tag 100 |
| `GraphChangeOp.GRAPH_CHANGE_OP_TEMPORAL` | 10 (`vlite.proto:472` ends at 9) | 6 | TMP value 100 |
| `RERANK_MODE_CUSTOM=3`, `SearchRequest.rerank_provider=18`, `Hit.rerank_score=3`, `SearchResponse.ranking=3` | as BASE | 2 | none |
| `Collection.precision=7`, `Collection.compact=8`, `ListCollectionsResponse.compact=3` | as PRC §4 ~184-189 | 11 | none |

### 1.4 Service versus arm, prefix and package rules (C16)

| Case | Rule |
|---|---|
| Own lifecycle or binding (KV namespaces and leases, memory bindings) | its own service in `vlite.v1`; messages carry the domain prefix (`Kv*`, `Memory*`) |
| Reads and writes on a named graph (algorithms, timeline, TTL reads) | RPCs appended to `Graph`; messages are `Graph<Noun>*` |
| Must commit atomically with core graph writes (temporal, TTL) | ONE band arm in `WriteGraphBatch` (1.3); a domain-only batch RPC is allowed only if it wraps that same message |
| Optional field on a baseline message | next free low tag (1.3) |
| Transport-only result schema, no service | `vlite.serving.v1` (P5) |

| Rule | Detail | Evidence |
|---|---|---|
| Prefix | the domain noun is the source noun: `Algorithm`, `Temporal`, `Expiry` (`feed.rs:274-275` has `NodeExpiry`, `EdgeExpiry`), `Kv`, `Memory`. Graph-attached messages are `GraphAlgorithm*`, `GraphTemporal*`, `GraphExpiry*`; separate services use `Kv*`, `Memory*`. No bare `Temporal*`, `Algorithm*` or `Expiry*`. KV's `GraphExpiry*` stays; GR12's `GraphKv*` and `GraphMemory*` examples and TMP's bare `Temporal*` change | `property_graph/feed.rs:274-275`; GR12 §3 ~74-75; TMP §7 ~666-762 |
| RPC short names | unique across all `vlite.v1` services, because `grpc_keys` keys on the short name. Bare generic verbs are out (KV `Get`, `Put`, `Delete`, `List` become `GetKvEntry`, `PutKvEntry`, `DeleteKvEntry`, `ListKvEntries`); any other unique name stays (`Remember`, `Recall`, `AcquireLease`, ALG `AlgorithmDegree`, TMP `EdgeTimeline`) | `vlite.proto:16-60`; `api_reference.py:445-446`; decision 16 |
| Own-message reserve | every domain-owned message declares `reserved 100 to 199` (MEM, ALG practice); TMP's `reserved 20 to 99` becomes `100 to 199` | MEM §8 ~682; ALG ~858-1080; TMP §7 ~669-758 |
| Package | every service, and every message reachable from a service, lives in `vlite.v1` in `vlite.proto`. `vlite.serving.v1` is only for transport-only schemas that share no name with a `vlite.v1` message | BASE:209; `vlite.proto:14`; P5 ~156 |
| `HybridHit` (C16) | P5's `message HybridHit` (~169-176, `reserved 7 to 15`) duplicates `vlite.v1.HybridHit` (tags 1-6): import the `vlite.v1` message (decision 17). P5 also names the build wiring for `results.proto` and adds a test compiling it with `vlite.proto`, because `grpc_smoke` compiles only `vlite.proto` | P5 ~169-176, 493; `grpc_smoke.py` |

### 1.5 Size and count caps (C14)

| Surface | Request | Response | Evidence |
|---|---|---|---|
| gRPC, every service | 8 MiB decode | 16 MiB encode | `grpc.rs:21-25,98-99`; `docs/GRPC.md:157-160`; BASE:177 |
| REST, non-graph | 8 MiB JSON body | no cap: no response limit layer in `src/server*` | `server.rs:60,742` |
| REST graph reads | 1 MiB | no cap | `graph.rs:80,184` |
| Graph batch | 4,096 ops; engine cap 16,777,216 encoded bytes (the 8 MiB wire cap bites first) | none | `graph.rs:110,717`; BASE:175 |
| G8 provider calls | embedding input 16,384 UTF-8 bytes; encoded request 1 MiB | 4 MiB | G8 limits table |
| gRPC graph reads, including ALG | 1 MiB of the decoded request's re-encoded protobuf `encoded_len()`; adapter preflight after tonic's 8 MiB decode cap | 16 MiB | ALG ~1186; decision 18; not a pre-decode or process-memory bound |

## 2. Error registry (C2-C9)

The gRPC column is what the shared mapper returns (`grpc.rs:181-196`). It keeps every existing arm and adds only the arms marked new, as BASE:126-127 does for `provider_*`: explicit per-code arms, existing codes unchanged, `vlite-error-code` trailer carried (`grpc.rs:199-216`). A failure has one code; no draft defines a second. No row needs a new public `Error` variant: new codes are built as an `ApiError` by the handler or adapter, or are new `From<Error>` arms for existing variants.

### 2.1 Registry

| HTTP | Code | gRPC | Owner doc | existing/new |
|---|---|---|---|---|
| 400 | `invalid_query`, `invalid_id` | INVALID_ARGUMENT (generic 4xx, `grpc.rs:194`) | src `server.rs:1664,1825` | existing |
| 400, 413, 422 | `invalid_json` (extractor status: syntax, oversize, shape) | INVALID_ARGUMENT | src `server.rs:1658-1662`; `graph.rs:344-365` | existing; the only malformed or oversize REST body code |
| 401 | existing codes | UNAUTHENTICATED (`grpc.rs:185`) | src | existing |
| 403 | `forbidden_scope`, `read_only` | PERMISSION_DENIED (`grpc.rs:186`) | src `server.rs:939,993` | existing |
| 404 | `collection_not_found`, `import_not_found`; KV `kv_not_found`; MEM `memory_not_found`, `receipt_not_found`, `erasure_not_found` | NOT_FOUND (`grpc.rs:187`) | src `server.rs:1488,1492`; KV ~846; MEM ~467 | existing, new |
| 406, 422 | `binary_unsupported_mode`, `binary_result_limit` | INVALID_ARGUMENT (generic) | P5 ~139-140 | new; REST only |
| 409 | `collection_exists` | ALREADY_EXISTS (`grpc.rs:188`) | src `server.rs:1495` | existing |
| 409 | `rebuild_busy` | ABORTED (`grpc.rs:189`) | src | existing |
| 409 | `collection_limit`, `default_collection_immutable`, `collection_not_empty`, `import_conflict`, `import_limit`, `upgrade_required` (batch, metadata and expiry variants), `requires_format_v4` (collections, imports, change feed) | FAILED_PRECONDITION (`grpc.rs:190`) | src `server.rs:1484-1521` | existing; a code follows its engine variant, so no draft renames or merges the two format codes |
| 409 | graph state conflicts: `graph_not_found`, `graph_node_not_found`, `graph_edge_not_found`, `graph_attached_record_expiry`, `graph_cursor_stale` and the rest of `graph_code` | FAILED_PRECONDITION (`grpc.rs:190`) | src `server.rs:1597-1615,1631` | existing; G6 §7 proposal 4 (404 for a missing graph) rejected |
| 409 | `cursor_compacted`, `cursor_ahead`, `cursor_before_history` | FAILED_PRECONDITION | src `server.rs:1556-1563` | existing |
| 409 | `cursor_mode_mismatch` (feed cursor of another `graph_events` set, section 4) | FAILED_PRECONDITION | KV §6 ~425-427 | new; the only name |
| 409 | `unsupported_feature` (`details.feature`, `details.category`), scoped to the temporal and explicit-precision operations | FAILED_PRECONDITION | TMP ~254, 885; PRC ~474, 498-501 | new `ApiError` built by those handlers only; legacy routes keep 500 `internal_error` (`server.rs:1624`); decision 7 |
| 409 | `precision_not_enabled`, `compact_required`, `unsupported_version`, scoped to explicit-precision operations | FAILED_PRECONDITION | PRC ~471-475, 498-501 | new; built like `unsupported_feature` (decision 7) |
| 409 | `kv_slots_exhausted`, `not_kv_collection` (new arms for existing variants), `lease_held`, `stale_fencing_token`, `lease_unavailable`, `fencing_recovery_required` | FAILED_PRECONDITION | KV §10 ~851, 861 | new |
| 409 | `memory_feed_unsupported` and the other MEM domain 409s | FAILED_PRECONDITION | MEM ~468-472 | new |
| 409 | `provider_not_configured` | FAILED_PRECONDITION (generic arm) | BASE:118 | new |
| 412 | `precondition_failed` (MEM revision or job mismatch as `details.field`) | ABORTED (`grpc.rs:189`) | src `server.rs:1523`; MEM ~473 | existing |
| 413 (gRPC adapter only) | `response_too_large` (no cursor advance) | RESOURCE_EXHAUSTED (new arm before `grpc.rs:194`) | GR12 ~841; TMP ~265; MEM ~474 | internal `ApiError` status, not a new REST cap; replaces `graph_response_too_large`, `memory_response_too_large` and TMP's INVALID_ARGUMENT mapping |
| 422 | `invalid_input` (limit or field named through `details`, section 3); KV `InvalidKvKey` through a new arm; invalid precision values | INVALID_ARGUMENT | src `server.rs:1446`; KV §10 ~849; PRC ~476 | existing; new arm; decision 6 rejects a separate `invalid_precision` code, while wrong JSON type/shape remains `invalid_json` |
| 422 | `invalid_cursor`; `invalid_graph_name`, `graph_limit_exceeds_ceiling`, `graph_invalid_options`, `graph_pivot_too_expensive` | INVALID_ARGUMENT | src `server.rs:1545,1616-1623` | existing |
| 422 | `invalid_provider_input` | INVALID_ARGUMENT | BASE:119 | new |
| 429 | `rate_limited`, `quota_exceeded`, `provider_busy` | RESOURCE_EXHAUSTED with `retry-after: 1` (`grpc.rs:184,201-203`) | src `server.rs:1216-1219`; BASE:123 | existing; `provider_busy` new |
| 500 | `internal_error` | INTERNAL | src | existing |
| 502 | `provider_invalid_response` | UNAVAILABLE (new arm) | BASE:120,126-127 | new |
| 503 | `busy`, `unavailable` | UNAVAILABLE (`grpc.rs:191`) | src `grpc.rs:249-252` | existing |
| 503 | `provider_unavailable` | UNAVAILABLE (new arm: `grpc.rs:191` matches only `busy`, `unavailable`) | BASE:121 | new |
| 503 | `commit_indeterminate` | UNKNOWN (`grpc.rs:193`) | src `server.rs:1577` | existing |
| 503 | `poisoned` | INTERNAL (`grpc.rs:195`) | src `server.rs:1589`; MEM ~479 | existing |
| 504 | `provider_timeout` | DEADLINE_EXCEEDED (new arm) | BASE:122 | new |
| 504 | `deadline_exceeded` (before the commit point, no partial allowed; built by the handler from the engine stop reason) | DEADLINE_EXCEEDED (new arm) | decision 4 | new; replaces `graph_timeout`, `graph_deadline_exceeded`, `memory_deadline_exceeded` |
| 408, 499, other 504 | none emitted | INVALID_ARGUMENT, INVALID_ARGUMENT, INTERNAL (generic arms) | src `grpc.rs:194-195` | existing; unreachable on REST today, so no draft may rely on them |
| offline | `locked`, `destination_exists`, `encryption_*`, `corrupt`, `incomplete_tail`, `publication_indeterminate` and the offline `invalid_input` cases | none: CLI or startup only | PRC ~487-493, 501-503; `graph_json.rs:26-72` | CLI codes only; no HTTP or gRPC mapping |

### 2.2 One reachable family: oversize, deadline, cancel, busy

| Failure | REST | gRPC | Rule |
|---|---|---|---|
| Request over cap | 413 `invalid_json` (graph reads 1 MiB `graph.rs:80,184`; others 8 MiB `server.rs:60,742`) | OUT_OF_RANGE with no `vlite-error-code` trailer, raised by tonic before the handler (tonic `codec/decode.rs:192-194`); an adapter preflight of a graph read's `encoded_len()` answers `invalid_json` (GR12 ~718-721, 828), not ALG's `invalid_input` (ALG ~386) | drop `request_too_large`, `memory_request_too_large` and graph oversize variants; the 1 MiB gRPC preflight is decision 18 |
| Count or shape bound (ops, `limit`, `k`, depth) | 422 `invalid_input`, the bound named per section 3 (`graph.rs:717-719`; `features.rs:109-110`) | INVALID_ARGUMENT | one code for every domain |
| Response over 16 MiB | no cap exists (decision 3) | an `encoded_len()` preflight in the adapter returns `response_too_large`; without a preflight tonic returns OUT_OF_RANGE with no trailer (tonic `codec/encode.rs:187-189`) | never a truncated success (`docs/GRPC.md:159-160`); a feed or list page does not advance its cursor |
| `grpc-timeout` expiry | n/a | CANCELLED with no trailer: tonic drops the handler (tonic `status.rs:644-645`, `transport/service/grpc_timeout.rs:89`) | accept it; no draft promises DEADLINE_EXCEEDED for it |
| Engine or adapter deadline | typed graph read: 200 with `complete=false` and reason `deadline`; otherwise 504 `deadline_exceeded` | OK with partial; otherwise DEADLINE_EXCEEDED with trailer | only before the commit point; after it the result is committed or `commit_indeterminate` |
| Client cancel | no wire code | handler dropped; the spawned call keeps its permit (`grpc.rs:243-252`); a disconnect never rolls back a commit (`docs/GRPC.md:152-155`) | the 408/499 `*_cancelled` rows are dropped (audit label only); a `cancelled` partial reason exists only where the channel survives (MCP, CLI, embedded) |
| Busy | 503 `busy`, `unavailable` | UNAVAILABLE (`grpc.rs:191,249-252`) | no domain busy codes; G7 busy admission uses `busy`, not `InvalidQuery` |
| Credential limit | 429 with `Retry-After: 1` (`server.rs:1414-1419`) | RESOURCE_EXHAUSTED | existing |
| Work budget exhausted, no partial allowed | 422 `invalid_input`, the reason named per section 3; existing 422 graph codes stay | INVALID_ARGUMENT | G6 §7 proposal 1 (~544-546); decision 5 |

### 2.3 Partial versus error

| Rule | Detail | Evidence |
|---|---|---|
| Partial is a success | HTTP 200, gRPC OK, MCP `isError:false`, with `complete=false` and ordered reasons from ONE vocabulary: `visited_limit`, `edge_limit`, `vertex_limit`, `memory_limit`, `output_limit`, `deadline`, `cancelled`, `iteration_limit` | ALG ~230-271, 260, 781; BASE:178,181 |
| Who may return one | typed graph reads only (GR12, ALG, TMP); MEM reads only with `allow_partial: true` (decision 19) | MEM ~299-306 |
| Never partial | graph batch and KV writes, memory mutations, G6 writes, G7 ingestion, provider calls, response-size failures, authorization and storage errors, transport expiry and cancel | BASE:181; G6 §6 |
| Expired channel | cannot carry a partial; the call ends CANCELLED without trailer | ALG ~356-362 |
| JSON | malformed or oversize body is `invalid_json`; a semantic failure is 422 `invalid_input`; structured context follows the `details` rule of section 3 | `server.rs:1658-1662`; `grpc.rs:199-216` |
| MCP | one failure shape: `isError:true` with `structuredContent.error` a STRING, `"<code>: <message>"` with the engine's code; `invalid_request` is tool-local and never a REST or gRPC code; object-shaped MCP failures change (decision 22) | `vlite-mcp.rs:673-682`; `graph_mcp.rs:56-62`; `graph_proxy.rs:190`; KV §9 ~717-719 |
| CLI | preserve coded JSON `{"error":{"code":...,"message":...}}` on stderr and a nonzero exit; do not apply MCP's `isError` / `structuredContent` envelope | `vlite-graph-cli.rs:8-13,346-351` at 1d531a26; decision 22 leaves CLI behavior unchanged |

## 3. Encoding rules (C12)

| Concept | Rule | Evidence | Docs that change |
|---|---|---|---|
| u64 and i64 in JSON: ids, revisions, sequences, timestamps on graph-domain routes | output decimal string; input canonical decimal string or exact JSON integer in the field's full u64/i64 range, through one shared parser; no extra JavaScript-safe-range restriction; unknown fields rejected; proto uses native uint64 and int64 | `graph.rs:4-8,806-825`; BASE:205 | KV (decimal strings for every new u64, ~153-155, 967; integer revision input kept only inside the graph fragment, ~673-675, as `graph_json.rs:98,161-168`), MEM (strings only, ~76-80), TMP (integer input capped at +-(2^53-1), larger values strings, out-of-range 422 `invalid_json`, ~72-78): decision 12 |
| Existing numeric fields | stay JSON numbers (`committed_at_ms`, vector `expires_at_ms`); clients do not unify them with graph strings | `features.rs:64-87`; `changes.rs:172` | none |
| `deadline_ms` | uint64 everywhere, a JSON integer; default 250, ceiling 5,000; above the ceiling is rejected, never clamped | `limits.rs:68-81,93,104`; `graph.rs:1176`; GR12 ~137; ALG ~886 | MEM (uint32, ~717-721), G7 (`u32`, ~98), G6 `write_deadline_ms` (~433, 453, 693) |
| Other limits | `GraphLimits` is eight `u64` fields; page `limit` and `k` are uint32; bounded durations (`poll_interval_ms`, KV `ttl_ms`) stay uint32 | `limits.rs:68-81`; `vlite.proto:455-456`; `features.rs:109-110` | MEM `MemoryLimits` |
| `*_ms` names | millisecond values end in `_ms`; valid time is `as_of_ms`; the TTL clock is `ttl_clock_ms` (source name; TMP, G6, MEM and KV already agree) | `compact/graph.rs:2569-2589` | ALG `read_time_unix_ms` (~241, 823, 1075) |
| Validity window | `valid_from_ms`, `valid_to_ms` (decision 11) | TMP §3 ~83-99 | MEM `from_ms`, `to_ms` (~610, 705) |
| Idempotency key | body field `idempotency_key` only, no header alias; the digest excludes deadline fields; G7 uses content-digest receipts (decision 13) | MEM ~119, 122-126, 182; no source precedent | KV, MEM, G7 state it once |
| Cursors | the feed token belongs to `changes.rs` (section 4); graph read cursors are opaque `cursor` and `next_cursor` owned by the engine; keyed lists use `after` and `next_after` | `changes.rs:26-35`; `openapi_graph.json:526`; `openapi.json:56,114`; `vlite-mcp.rs:468,575` | KV and MEM list routes |
| Pagination | feed `limit` 1..=10000, default 1000, whole commits per page unless section 4 says otherwise | `features.rs:109-110`; `changes.rs:22-23,445-448` | KV, TMP |
| Error `details` | gRPC: the optional JSON string an `ApiError` carries, sent as `vlite-error-details-base64` (one producer today, `import_rows_invalid`). REST currently has `{error:{code,message}}`; decision 21 approves ONE additive optional `error.details` object (same JSON; keys `field`, `limit`, `requested`, `feature`, `category`), preserving `code` and `message`. No other new error-body member and no `vlite-feature` trailer | `server.rs:1380-1388,1409-1412`; `grpc.rs:206-209,2262-2268`; PRC ~461-465; MEM ~454-458 | MEM (keys), TMP D8 (`feature` and `category` move into `details`), G6 (reason text only) |

Decision 12 does not make an already-rounded JavaScript number exact: clients
must send large values as strings before rounding can occur. Canonical string
parsing rejects padding, leading zeros other than `"0"` and nondecimal syntax,
with the corresponding signed form for i64. Ordinary JSON layout whitespace is
unaffected. Existing numeric fields retain their current representation.

## 4. Graph events in the change feed (C11)

| Item | Ruling | Evidence |
|---|---|---|
| Opt-in | ONE mechanism: a `graph_events` feature set (`ttl`, `temporal`) that generalizes the engine's `temporal: bool`. REST `GET /v1/changes?graph_events=ttl,temporal`; proto `repeated GraphEventFeature graph_events` (tags in 1.3); webhooks take an immutable per-subscription set; an empty set is byte-identical legacy output. The response echoes the set because an older gRPC server drops the unknown request field silently; an older REST server answers 400 `invalid_query` | `changes.rs:207,236,246`; `features.rs:91-92`; `vlite.proto:455-456,525`; decision 8 |
| Replaces | KV `graph_ttl` (~412-415, 599-601); TMP `temporal=100` and `feed_version` (~441-447, 484-492, 649, 783-785) | KV §6; TMP §6 |
| Cursor owner | `FeedCursor` in `src/changes.rs`: `vlc1` for the default feed; `vlc2` whenever the set is non-empty, binding lineage, sequence, ordinal and the set; a cursor of another set is 409 `cursor_mode_mismatch`. The TTL slice lands it once and temporal consumes it | `changes.rs:26-35`; KV §6 ~425-427 |
| Page splitting | the default feed never splits a commit; only TTL reclaim runs split (greedy, 256 ops or 1 MiB) because their expansion is unbounded; temporal commits stay whole (4,096 ops at most); one unsplittable unit over the response cap is `response_too_large` with no cursor advance (decision 9) | `openapi.json:33`; `changes.rs:445-448`; `graph.rs:110`; KV §6 ~473-476 |
| Webhooks | the dispatcher is a consumer, not a second cursor owner: it persists progress per feature set; the legacy `webhooks.cursor` path and format stay for the empty set | `webhooks.rs:81,391-450`; KV §6 ~420-424; TMP ~484-492, D11 ~888 |
| Envelope | the existing `graph_batch` event gains additive fields (`GraphChangeOperation` tags 19-23); no new top-level event type; the `unsupported_temporal` marker stays for legacy output only | `changes.rs:575`; KV §6 ~434 |
| MEM | the frozen `feed` block and `require_complete_feed` stay; MEM points here for `graph_events` | MEM §7 ~508-534 |

## 5. Files, order and guards

### 5.1 Order

BASE:253-255 fix steps 1-3; the later steps are the ratified serving order and agree with `next-major.md:33-35` (KV, TTL and time-aware graph precede memory). A slice rebases onto the previous slice's proto and descriptor test before it edits; two slices never edit the same message at once.

| Step | Slice | Doc | Step | Slice | Doc |
|---|---|---|---|---|---|
| 1 | Gap 8 registry | G8 | 7 | Gap 1D memory | MEM |
| 2 | Gap 8 REST, gRPC, Python | G8 | 8 | Gap 5 S5 algorithms | ALG |
| 3 | Gap 12 graph gRPC | GR12 | 9 | Gap 6/7 serving | G6, G7 |
| 4 | Gap 2 KV | KV | 10 | P5 binary results | P5 |
| 5 | Gap 3 TTL | KV | 11 | P3 precision API | PRC |
| 6 | Gap 4E temporal | TMP | | | |

### 5.2 Per-file owner and sequence

| File | Editors (steps) | Obligation at each landing |
|---|---|---|
| `src/server/grpc.rs` | 2-11 | add only: `add_service`, health name, caps, adapter wiring and the section 2 arms marked new (the first consumer lands them: `provider_*` at 2; `response_too_large`, `deadline_exceeded` at 3); never reorder `grpc.rs:184-195` |
| `src/server.rs` `From<Error>` | 4 | new arms before `server.rs:1624`: `InvalidKvKey`, `KvSlotsExhausted`, `NotKvCollection` (KV X3). `UnsupportedFeature` gets no global arm: steps 6 and 11 build a scoped `ApiError` in the temporal and explicit-precision handlers (decision 7) |
| `proto/vlite/v1/vlite.proto` | 2-9, 11 | append-only; tags per 1.3; the descriptor test gets named additions |
| `proto/vlite/serving/v1/results.proto` | 10 | package rule of 1.4; build wiring named; compiled together with `vlite.proto` in a test |
| `src/openapi.json` | 2, 4, 6, 7, 9, 10, 11 | every component schema (the graph file has none) plus all non-graph paths and the `/v1/changes` parameters; never a `/v1/graphs` path |
| `src/openapi_graph.json` | 3, 5, 6, 8, 9 | the only file that may hold `/v1/graphs` paths; paths only, because `merge_openapi` copies only `paths` (`graph.rs:1772-1786`). `GraphBatch` (C10): the item stays ONE object with an `op` enum and `additionalProperties:true` (`openapi_graph.json:2141-2165`); each domain adds its op literals to the enum plus one `allOf` `if`/`then` branch on the `op` const that `$ref`s one domain component (KV §8 ~677-685; the TTL slice, step 5, lands it first); no `oneOf` alternatives on `items` (decision 23); `ops` keeps `maxItems` 4096 (`openapi_graph.json:2140`) |
| `docs/GRPC.md` | 2, then 3, 4, 7 | the registry slice edits the failure section once (`:138-160`); later slices add one section per service |
| `site/api.html` | every step with a route, RPC or tool | one `data-api` row per REST, gRPC and MCP key |
| `tests/grpc_smoke.py` | 3, 4, 7 | the health loop (`:184-186`) names each new service; one oversize-request assertion per service |
| `tests/api_route_inventory.py` | any step with routes | literal route strings and known constants only (`:23-27`) |
| `tests/api_reference.py` | 3 | scanner decisions of 5.3 (decision 16) |
| `src/bin/tool_support/*`, `vlite-mcp.rs`, `vlite-graph-cli.rs` | 3, 5, 6, 8, 9 | `graph_mcp.rs` stays the home of `vlite_graph_*`; limits stay `u64` (`graph_json.rs:469`); `invalid_request` stays tool-local |
| `clients/*` | 2, then every step | method names derived per 5.4; one test per language |

### 5.3 Guard tests

| Guard | Today | Obligation |
|---|---|---|
| `GrpcHealth::set`, `grpc_smoke` | names `""` and `vlite.v1.VLite` only (`grpc.rs:66-68`; `grpc_smoke.py:184-186`) | add each service in its own slice |
| Size caps | set on `VLiteServer` only (`grpc.rs:98-99`) | each service sets both caps and has a test |
| `grpc_keys` | reads only `vlite.proto`, key `grpc:<ShortName>` (`api_reference.py:445-446`) | all RPCs stay in `vlite.proto`; unique short names (decision 16); `results.proto` has no service |
| `mcp_keys` | scans only `vlite-mcp.rs` and `tool_support/graph_mcp.rs` (`api_reference.py:449-451`) | new `vlite_kv_*` and `vlite_memory_*` tools live in a scanned file, or the slice extends the scanner |
| `agent_docs.py` | tools in `vlite-mcp.rs` equal the `docs/MCP.md` table and `len(tools) == 9` | a tool added there updates both |
| `rest_keys`, `page_keys` | routes in `src/server.rs`, `src/server/*.rs` and both OpenAPI files, against `data-api` rows (`api_reference.py:431-435,483-484`) | code, spec and `site/api.html` rows land together |
| `api_route_inventory.py` | rejects `.route(format!(...))`, `.nest(...)`, unknown constants (`:23-27`) | literal routes only |
| Descriptor equivalence | GR12 ~931 freezes `GraphChangeOperation` byte for byte (BASE:206) | KV (19-22) and TMP (23) add named entries |

### 5.4 Naming rules

| Surface | Rule | Evidence |
|---|---|---|
| REST operationId (C15) | PascalCase, `Graph*` for graph routes; KV `kvGet` and MEM `memoryRemember` change (PRC is already PascalCase, ~580-591) | all 12 ids in `openapi.json`; `openapi_graph.json:2128`; KV ~236-240; MEM ~203-204 |
| OpenAPI component | PascalCase with the 1.4 domain prefix (`GraphTemporal*`, `GraphExpiry*`, `GraphAlgorithm*`, `Kv*`, `Memory*`); no generic names (`Limits`, `Key`, `Scope`, `Path`, `Interval`, `Expected`, `Mutation`, `Ranking`); components only in `openapi.json` (44 today) | MEM ~605-629; TMP ~646-648 |
| Proto message, RPC | 1.4 | `vlite.proto:16-60` |
| MCP tool | `vlite_<domain>_<verb>` in snake_case: `vlite_graph_*` (13 exist), `vlite_kv_*`, `vlite_memory_*`; ALG's `vlite_graph_degree` and siblings fit | `graph_mcp.rs`; KV ~709-712; ALG ~116-121 |
| MCP input and failure (C13) | flat arguments with `additionalProperties:false` and one top-level selector property (`collection`, `graph` or `binding`); no `{binding, request}` nesting; no `outputSchema`; a failure is the string of 2.3 (decision 22) | `vlite-mcp.rs:457-458,464-474,673-682`; `graph_mcp.rs:56-62,286,306`; KV ~709-720; MEM ~779-781, 788; PRC ~283-288 |
| CLI subcommand | kebab-case mirror of the tool verb (`find-nodes`, `shortest-path`) | `vlite-graph-cli.rs:78-81` |
| Client method | flat methods derived mechanically from the RPC or operation short name: Python `snake_case`, JavaScript `camelCase`, .NET `PascalCase` plus `Async`; no new scoped facade or hand-shortened aliases (`create_collection_with_precision` is the pattern). An option on an existing operation (P5 `resultFormat`) is a keyword or options argument in Python and JavaScript and a distinct `<Operation>With<Option>Async` method in .NET, so existing calls keep binding | PRC ~429-437; P5 ~406-413; MEM ~807-816; decision 10 |

## 6. Owner decisions (23)

All choices below are resolved by the 2026-11-02 one-by-one owner walkthrough:
option A for 1-17, 19 and 21-23; option B for 18 and 20. Decisions 14 and 15
carry forward the already-approved G7 reserved-object policy and G6 separate
write budget rather than asking them again. For decision 20 the owner rejected
the original recommendation to drop the hold, then explicitly approved the
GR12 owner, future-core-operation purpose and separate tag-release review.
These are shared design approvals only; exact feature contracts, independent
implementation acceptance, qualification and final release go/no-go remain separate.

| # | Decision | Options | Owner selection | Evidence |
|---|---|---|---|---|
| 1 | Tags 100-499 in Gap 12 shared messages | A sibling-owned bands, one arm per band carved from `reserved`; B permanent empty fences, sibling messages and RPCs only | A: keeps temporal and TTL writes atomic with core writes and `GraphBatch` single | BASE:152; KV ~589-590; TMP ~777, D3 ~880; GR12 ~71-77, 1026 |
| 2 | `GraphTtl` | A fold into `Graph`; B separate service | A: one health name, one cap setting, one parity table | BASE:131; KV ~548-553 (ledger X1) |
| 3 | REST response cap | A none (today), `response_too_large` is gRPC-only; B a REST cap returning 413 `response_too_large` | A: no layer exists and adding one is a policy change | `src/server*` has no response limit layer; `docs/GRPC.md:157-160` |
| 4 | Deadline status | A explicit 504 `deadline_exceeded` to DEADLINE_EXCEEDED; B 503 `unavailable` to UNAVAILABLE | A: precedent BASE:122; B invites blind retry | BASE:122,126-127; TMP D9 ~886 (B is TMP's provisional fallback); MEM ~481 |
| 5 | MEM `memory_budget_exceeded` (429) | A per-request work budget is 422 `invalid_input`, a rate or quota is the existing 429; B keep the new code | A: no new code; 429 stays the credential limiter | MEM ~477; `server.rs:1216-1219`; G6 §7 proposal 1 (~544-546) |
| 6 | PRC `invalid_precision` (422) | A fold into `invalid_input`, the field named through `details` (decision 21) or the message; B keep a new domain code on a generic 422 arm | A: semantic is 422 `invalid_input` and PRC already uses it for its other value failures; PRC also files "wrong precision type" here, yet a wrong type is a shape failure (`invalid_json`, ~478). B follows `invalid_cursor`, `invalid_id`, `invalid_graph_name` | PRC ~476, 478, 480-481; `server.rs:1446` |
| 7 | `unsupported_feature` mapping | A scoped per-operation `ApiError` (409; feature and category in `details`; legacy routes keep 500); B a global `From<Error>` arm (changes legacy 500 to 409); C TMP D8: `vlite-feature` trailer plus new body members; D leave 500 | A: no legacy mapping change; context uses the shared optional `details` extension approved in decision 21 | `server.rs:1624`; TMP ~254, 271-275, 290-294, D8 ~885; PRC ~498-501; G6 §7 proposal 2 (~547) |
| 8 | Feed opt-in | A `graph_events` set; B separate booleans (`graph_ttl`, `temporal`); C `feed_version` | A: one parameter, one cursor token, extends the engine boolean | `changes.rs:207,236,246`; KV ~412-415; TMP ~441-447 |
| 9 | Feed page splitting | A split only TTL reclaim runs; B split every opted-in commit; C never split | A: temporal commits are bounded at 4,096 ops, reclaim is not | `openapi.json:33`; `changes.rs:445-448`; KV ~473-476 |
| 10 | Client facade shape | A flat methods derived from RPC names; B scoped objects (`client.graph(name)`, `client.memory(binding)`) | A: the 5.4 derivation is mechanical; B needs the same wrapper in four languages | MEM ~807-816; PRC ~429-437 |
| 11 | Validity-window names | A TMP `valid_from_ms`, `valid_to_ms`; B MEM `from_ms`, `to_ms` | A: self-describing and already on TMP's write and read surfaces | TMP §3 ~83-99; MEM ~610 |
| 12 | u64 and i64 input on new domains | A the `graph.rs:4-8` rule with one shared parse helper; B strings only (KV, MEM); C numbers within +-2^53 (TMP) | A: the source already decides for graph routes; one helper and one client behaviour | `graph.rs:4-8`; KV ~153-155, 967; MEM ~76-80; TMP ~72-78 |
| 13 | Idempotency key | A body field only; B body plus an `Idempotency-Key` header alias | A: no source precedent; one place for REST and gRPC | MEM ~119, 122-126 |
| 14 | `G7*` reserved objects and raw writers | A reject raw writes to `G7*`; B allow with an explicit `raw_access` grant; C accept forgeable receipts | A: receipts are evidence and A adds no scope | G7 ~517-520; `graph.rs:14-43` |
| 15 | G6 `write_deadline_ms` | A keep a separate write budget (uint64, rejected above its ceiling); B one `deadline_ms` | A: G6 sets different defaults (100 and 1,000 against 250 and 5,000) | G6 ~433, 453, 693; `limits.rs:93,104` |
| 16 | RPC short-name uniqueness | A descriptor-level uniqueness test, scanner unchanged; B qualify keys as `grpc:<Service>.<Rpc>` | A: B rewrites every `site/api.html` gRPC row | `api_reference.py:445-446`; KV ~700-701 (ledger X6); GR12 ~910-915 |
| 17 | `vlite.serving.v1` and `HybridHit` | A import `vlite.v1.HybridHit`; B rename and convert; C fold `results.proto` into `vlite.v1` | A: one concept, one message | P5 ~156,169-176 |
| 18 | gRPC graph-read input cap | A tonic 8 MiB only (OUT_OF_RANGE, no trailer); B an adapter `encoded_len()` check at 1 MiB returning `invalid_json` as INVALID_ARGUMENT | B for graph reads only: decoded request's re-encoded protobuf bytes, not original wire bytes or a pre-decode memory bound; ALG's trailer becomes `invalid_json` and S5-D5 uses this rule; tonic decoding and writes keep 8 MiB | `graph.rs:80`; BASE:177; GR12 ~718-721, 828, 842, 931; ALG ~386, 1186 |
| 19 | MEM `allow_partial` | A MEM reads fail unless `allow_partial: true`; B partial by default like graph reads | A, documented as MEM's one exception | MEM ~299-306; BASE:178 |
| 20 | `GraphWriteOperation` `reserved 14 to 31` hold | A drop it, so Gap 12 core may use 14-99; B name an owner and purpose | B: KEEP the hold. GR12 / the Gap 12 core graph protocol owner owns tags 14-31 for future core atomic graph-batch operation arms; a separate exact-schema review must name an operation and explicitly release its tag | Owner choice and clarification, 2026-11-02; GR12 section 3; BASE:204 |
| 21 | Structured error context on REST | A one optional `error.details` object, additive, the same JSON as the gRPC trailer; B none on REST (message text only, gRPC trailer only); C per-draft members (MEM `details` on memory routes, TMP `feature` and `category`) | A: one schema extension serves decisions 5-7 and needs no new trailer; today only gRPC can carry the context | `server.rs:1380-1388,1409-1412`; `grpc.rs:206-209,2262-2268`; MEM ~454-458; TMP ~271-275, 885; PRC ~461-465 |
| 22 | MCP tool input and failure shape (C13) | A flat arguments with one top-level selector and a string failure `"<code>: <message>"` (what the source, KV, G6 and ALG do); B MEM's nested `{binding, request}` input with `MemoryError` failures, and PRC's `{error:{code,message}}` objects | A: every existing tool takes flat arguments and returns a string error, so one client parser serves all tool families; B adds a second parsing path | `vlite-mcp.rs:457-458,464-474,673-682`; `graph_mcp.rs:56-62`; KV ~709-720; G6 ~536; ALG ~286, 424; MEM ~779-781, 788; PRC ~283-288 |
| 23 | `GraphBatch` schema mechanism (C10) | A additive: new op literals in the `op` enum plus one `allOf` `if`/`then` `$ref` per domain (KV); B closed `oneOf` alternatives per domain appended to `items` (TMP) | A: `items` is one open object today, so A restructures nothing and each later domain adds one branch; B turns `items` into a `oneOf` that every domain must join | `openapi_graph.json:2141-2165`; KV §8 ~677-685; TMP ~645-646 |

## 7. Per-document conformance checklist

Every document: for each new code, state its producer (an existing `Error` variant through a new `From` arm, or an `ApiError` built by the handler); none adds a public `Error` variant. Cite this note instead of restating a rule. Unchecked items remain conformance work: owner ratification does not assert that every sibling draft or implementation has been reconciled.

**KV**
- [ ] operationIds `kvGet`, `kvPut` and the rest (~236-240) become PascalCase.
- [ ] `graph_ttl` (~412-415, 599-601) becomes the `graph_events` set with tags 3, 4 and echo 5; `vlc2` is named the single non-default feed token owned by `changes.rs`; per-mode dispatcher cursors (~420-424) become per-set progress.
- [ ] TTL writes use the band arm `GraphExpiryOperation expiry = 400` (~572); `WriteExpiryBatch` stays only as a wrapper of that message (~553, 589-590); `GraphExpiryEdge` (~555) reuses `GraphSelector` (~591-592).
- [ ] Fold `GraphTtl` (~548-553) into `Graph`; rename `Get`, `Put`, `Delete`, `List` (~508-511); add `vlite.v1.Kv` to health and caps (1.2).
- [ ] Register `GraphChangeOperation` 19-22 (~610-613) and `GraphRemovalReason` (~603-608) as named descriptor additions.
- Conforming: §10 error table (~843-862: `invalid_json`, no size codes, `cursor_mode_mismatch`, the 409 family); §8 `GraphBatch` extension (~677-685, decision 23); §9 MCP input and string failure (~709-720, decision 22).

**G6**
- [ ] §7 proposal 1 (~544-546) keeps 422 `invalid_input` plus the fixed reason for a budget; its deadline clause becomes 504 `deadline_exceeded` and cancel has no wire code; the draft names no `details` member.
- [ ] Withdraw proposal 5 (~550-552), proposal 3 (~548; `BatchTooLarge` is already 422 `invalid_input`) and proposal 4 (~549; a missing graph stays 409 `graph_not_found`); proposal 2 (~547) becomes the scoped 409 `unsupported_feature` (decision 7).
- [ ] §10 (~667): state that `/v1/graphs` paths go to `openapi_graph.json` and `openapi.json` holds components only.
- [ ] `write_deadline_ms` (~433, 453, 693) is uint64 per decision 15.
- Conforming: `ttl_clock_ms` (~344); mutating statements need both scopes (~562); `collections:["*"]` (~572); MCP failure text `"<code>: <message>"` (~536).

**G7**
- [ ] Errors (~474-497): provider timeout, unavailable, invalid response, not configured and busy use the G8 `provider_*` codes at the wire (BASE:118-123); busy admission uses `busy`; static reasons stay internal labels.
- [ ] `IngestRequest.deadline_ms` `u32` (~98) becomes uint64.
- [ ] `gap7-serving` (~594): name `openapi_graph.json` for any `/v1/graphs` path and `openapi.json` for components; schedule it at step 9.
- [ ] Settle the `G7*` raw-writer forgery (~517-520; decision 14).
- Conforming: `collections:["*"]` grants no graph scope (~509, 578).

**TMP**
- [ ] Tags: `TemporalWriteOperation temporal=100` and every tag-100 on a shared message (~771-777) become 200; the `GraphChangeOperation` arm is 23 (~781) and the enum value 10 (~779); the feed fields (~783-785) become `graph_events`.
- [ ] Prefix: bare `Temporal*` messages (~666-762) and the six `Temporal*` components (~646) become `GraphTemporal*`; `reserved 20 to 99` (~669-678, 758) becomes `100 to 199`.
- [ ] Errors: delete 413 `request_too_large` (~264), 504 `graph_timeout` (~266) and 408 `graph_cancelled` (~267); `response_too_large` (~265) keeps its name but maps to RESOURCE_EXHAUSTED; `unsupported_feature` (~254) is scoped and carries `feature` and `category` in `details`, with no `vlite-feature` trailer or new body members (D8 ~885, ~271-275, 290-294); remove the D9 provisional status (~269-271, 886).
- [ ] Feed: `temporal=100` and `feed_version` (~441-447, 484-492, 649) become `graph_events=temporal` with the section 4 cursor; whole commits stay whole.
- [ ] The `GraphBatch` row (~645) and the six `TemporalWriteOperation` `oneOf` schemas (~581, 625, 629, 646) become op literals in the `op` enum plus one `allOf` `if`/`then` `$ref` (5.2, decision 23).
- Conforming: `ttl_clock_ms` (~196, 227); 412 to ABORTED (~256); `invalid_json` (~258).

**MEM**
- [ ] Errors: drop 413 `memory_request_too_large` and `memory_response_too_large` (~474; use `invalid_json` and `response_too_large`), 504 `memory_deadline_exceeded` (~481; use `deadline_exceeded`) and 499 `memory_cancelled` (~482); delete the "add explicit gRPC mapping for the new 413/499/504 codes" sentence (~485-488); `memory_budget_exceeded` (~477) per decision 5.
- [ ] `deadline_ms` and `MemoryLimits` become uint64 (~717-721).
- [ ] operationIds `memoryRemember`, `memoryRecall` (~203-204, 590-591) become PascalCase; components `Ranking`, `Limits`, `Key`, `Scope`, `Path`, `Interval`, `Expected`, `Mutation`, `RecallRequest`, `FactsRequest` (~605-629) become `Memory*`.
- [ ] MCP input (~779-781) becomes flat arguments with `binding` as a top-level property, not `{binding, request}`; failures (~788) use the string `"<code>: <message>"`, not `MemoryError` (decision 22).
- [ ] Decisions 11, 12, 13 and 19 as ratified; the frozen `feed` block (~508-534) points to section 4; client names (~807-816) follow 5.4 (the table lists `facts(...)` beside `FindFactsAsync`, ~814); the REST `details` (~454-458) follows decision 21.
- Conforming: `poisoned` to INTERNAL (~479); 412 `precondition_failed` (~473); `invalid_json` (~465).

**PRC**
- [ ] `invalid_precision` (~476) per decision 6; reconcile its "wrong precision type" case with the strict-type `invalid_json` row (~478).
- [ ] MCP failure (~283-288) becomes the string `"<code>: <message>"`, not `{error:{code,message}}`; the "existing MCP errors stay strings" exception (~287) goes (decision 22).
- [ ] `unsupported_feature`, `unsupported_version`, `precision_not_enabled` and `compact_required` (~471-475, 498-501) stay scoped to explicit-precision operations (decision 7); confirm the offline rows (~487-493, 501-503) create no wire mapping.
- [ ] At step 11 confirm `Collection.precision=7`, `compact=8` (~184-185), `ListCollectionsResponse.compact=3` (~189) and the `reserved` ranges (~173, 180, 186, 190) against the descriptor.
- Conforming: operationIds (`ListCollections`, `CreateCollection`, `GetCollection`, ~580-591); client names `create_collection_with_precision` (~429-437); `requires_format_v4`, `collection_limit`, `default_collection_immutable` and `import_conflict` are existing codes (`server.rs:1484-1521`).

**GR12**
- [ ] §3 (~71-80): replace "reserved, not placeholders to reclaim" with the band rule of 1.3 for every shared message; rename band 400 "TTL" (~243); use `GraphExpiry*` and the separate `Kv*`, `Memory*` services in place of the `GraphKv*`, `GraphMemory*` examples (~74-75); retain and label `reserved 14 to 31` for GR12 future core atomic graph-batch arms, with separate review before any release (decision 20).
- [ ] §6 (~839-841): `graph_response_too_large` becomes `response_too_large`, `graph_deadline_exceeded` becomes `deadline_exceeded`, `graph_cancelled` stays an audit label with no wire code.
- [ ] §4 (~712-721): keep the `encoded_len()` preflight and state the unit of the graph-read input cap (decision 18).
- [ ] Add the 1.1 and 1.2 tables: health, caps and 17 methods at landing, with later slices appending in landing order; the sentence that fixes the service at 22 methods after ALG (~1029) goes, because no slice promises an absolute total.
- [ ] The scanner note (~910-915) becomes decision 16; the descriptor test (~931) accepts named additions.
- Conforming: `package vlite.v1` (~84); 17 unary RPCs (~87-105); `u64` limits (~137); `collections:["*"]` (BASE:163).

**ALG**
- [ ] `read_time_unix_ms` (~241, 807, 823, 1075) becomes `ttl_clock_ms`.
- [ ] The gRPC half of ~386 (`invalid_input` trailer on the 1 MiB read cap) becomes `invalid_json`; S5-D5 (~1186) becomes decision 18: the 1 MiB input is a graph-read rule, not an ALG-only rule.
- [ ] Replace the absolute "17 to 22" Graph method count (~479, 1165) with "append five RPCs (~839-843) to whatever `Graph` holds at step 8", per the 1.1 manifest; the slice sits at step 8, after memory, and owns only its five RPCs and rows.
- Conforming: `reserved 100 to 199` per message (~858-1080); the REST half of ~386 (`invalid_json` 413); partial vocabulary (~260, 781); expired channel (~356-362); tool-local `invalid_request` (~302, 385); MCP string failure with no `outputSchema` (~286, 424).

**P5**
- [ ] `HybridHit` (~169-176) per decision 17; no duplicate of any `vlite.v1` message.
- [ ] Name the build wiring for `results.proto` (~493, 499) and add the descriptor test of 1.4.
- [ ] Ledger (~481-498): 5C (`grpc.rs`, `server.rs`) and 5D (`openapi.json`) land at step 10, after the shared-file slices of steps 2-9.
- Conforming: REST-only rows `binary_unsupported_mode` and `binary_result_limit` (~139-140); client options (~405-411) follow 5.4.

**G8**
- [ ] The REST and gRPC slice lands explicit arms for `provider_invalid_response` (502), `provider_unavailable` (503) and `provider_timeout` (504); the other three codes use generic arms (BASE:118-127; `grpc.rs:184,190,194`).
- [ ] That slice adds the `grpc:EmbedText` row and any changed route rows to `site/api.html` (`api_reference.py:431-451,483-484`); the registry slice adds none (the `api_reference.py:204` skip concerns only the embedded Rust API scan).
- [ ] Answer the open item at BASE:257 (snapshot-conflict mapping, graph field and tag manifest) by pointing to sections 2 and 1.3.
- Conforming: startup-only inert registry; caps of 16,384 bytes, 1 MiB and 4 MiB; custom ranking tags (BASE:200-201).

### Areas with no conflict

| Area | Finding |
|---|---|
| Authorization scopes | every draft keeps `collections:["*"]` from granting graph, memory or KV scope (BASE:163; `graph.rs:14-43`; G6 §8 ~572; G7 §7) |
| Routes, operationIds, components | no two drafts claim the same path and method or the same identifier string; only casing and generic names differ (5.4) |
| RPC short names | none duplicate today; messages and tags clash only through the bands (1.3) and `HybridHit` (1.4) |
| Core caps | 8 MiB and 16 MiB, 4,096 ops and 16,777,216 bytes agree everywhere; the differences are decision 18 and the separate G8 provider caps |
| Delivery order | no draft places a slice before its dependency; the open problem is shared-file ownership (5.2) |
