# Serving contract: embeddings, rerankers and gRPC graphs

Status: design proposal. Nothing here approves a wire change.
All requirements below are proposed unless labeled Current.
Open means not verified or not decided. Evidence uses repository `path:line`.
Scope: gaps 8 and 12. No storage format change is proposed.
The serving track is listed in `docs/projects/roadmap/next-major.md:23`.
That roadmap freezes OpenAPI pending approval (`docs/projects/roadmap/next-major.md:82`).

## Current coverage

Evidence keys below resolve to code citations, not planned support.
No = absent from the inspected dispatcher or service, not an ecosystem-wide claim.
Python distinguishes the HTTP client from the embedded binding.

| Capability | Rust API | REST | gRPC | MCP | CLI | Python |
|---|---|---|---|---|---|---|
| Text embeddings | Trait, registry, hashing, optional HTTP [E1] | Hashing/HTTP [E2] | No RPC [E4] | No tool [E5] | Open | HTTP client yes [E7]; embedded Open |
| Exact/MMR post-reranking | Yes [E3] | Yes [E6] | Yes [E8] | No search option [E5] | No shell search option [E9] | Embedded yes [E10]; HTTP forwards request, typing incomplete [E7] |
| Custom reranker | `Reranker` trait [E3] | No selectable provider [E6] | No selectable provider [E8] | No option [E5] | Open | Open |
| Graph lifecycle, typed reads, batch | Yes [G1] | Yes [G2] | No graph RPC [E4] | Yes [G3] | Yes [G4] | Embedded yes [G5]; HTTP Open |
| Graph schema/retrieve/query text | Yes [G6] | Yes [G2] | No graph RPC [E4] | Yes [G3] | Query text only of these [G4] | Open |
| Graph change feed | Yes [F1] | Yes [F2] | Changes + WatchChanges [F3] | Open | Open | Embedded/HTTP feed calls [F4] |

- [E1] `src/embedding.rs:4-24`, `src/embedding.rs:26-33`, `src/embedding.rs:56-96`.
- [E2] `src/server/features.rs:570-634`; `Cargo.toml:15-17` keeps defaults empty and HTTP opt-in.
- [E3] `src/rerank.rs:10-23`, `src/rerank.rs:54-68`, `src/rerank.rs:106-131`.
- [E4] `proto/vlite/v1/vlite.proto:16-60` lists the existing RPCs.
- [E5] `src/bin/vlite-mcp.rs:294-321`, `src/bin/vlite-mcp.rs:456-476`.
- [E6] `src/server.rs:2633-2669`, `src/server.rs:3026-3047`.
- [E7] `clients/python/vlite_client/__init__.py:94-120` lacks a typed `rerank` field; `clients/python/vlite_client/__init__.py:535-558` forwards search and exposes embedding.
- [E8] `src/server/grpc.rs:1063-1158`; `proto/vlite/v1/vlite.proto:352-356` has none/exact/MMR only.
- [E9] `src/bin/vlite-shell.rs:197-221` executes vector search without a reranker.
- [E10] `bindings/python/vlite_embedded/__init__.py:478-481`.
- [G1] `src/property_graph/mod.rs:334`, `src/property_graph/mod.rs:530-595`, `src/property_graph/read.rs:812-838`.
- [G2] `src/server/graph.rs:123-184`.
- [G3] `src/bin/tool_support/graph_mcp.rs:119-136`, `src/bin/tool_support/graph_mcp.rs:152-252`, `src/bin/tool_support/graph_mcp.rs:256-282`.
- [G4] `src/bin/vlite-graph-cli.rs:33-82`; schema/retrieve are not commands in these dispatch lists.
- [G5] `bindings/python/vlite_embedded/__init__.py:535-589`.
- [G6] `src/property_graph/read.rs:824-878`, `src/property_graph/retrieve.rs:301`.
- [F1] `src/changes.rs:442` reads log changes; `src/changes.rs:535` exposes graph event JSON; graph events are consumed in `src/server/grpc.rs:641-657`.
- [F2] `src/server/features.rs:12`, `src/server/graph.rs:85-108` gates graph feed visibility.
- [F3] `proto/vlite/v1/vlite.proto:41-42`, `proto/vlite/v1/vlite.proto:458-523`.
- [F4] `bindings/python/vlite_embedded/__init__.py:489`, `clients/python/vlite_client/__init__.py:911`.

## Correcting the gap descriptions

Current: gap 8 says the server and SDKs cannot use these traits (`docs/projects/product-gaps/product-gaps.md:53`).
That is too broad: REST embeds, both transports call `CollectionRef::rerank`, and Python exposes embedding/reranking [E2, E6-E8, E10].
The remaining gap is a managed provider registry, collection policy and custom server rerankers.
Current: REST accepts at most 1,000 rerank candidates, default `max(k,50)` (`src/server.rs:2652-2656`).
gRPC instead defaults to `4*k` and checks at most 4,096 (`src/server/grpc.rs:1083-1094`).
Do not silently unify these existing contracts.
Current: gap 12's "vectors only" wording (`docs/projects/product-gaps/product-gaps.md:57`) also misses graph feed events [F3].
The missing part is graph query/mutation RPCs, not graph event streaming [E4, F3].

## Gap 8: provider policy

New external providers and custom rerankers are off by default; keep local hashing and explicit exact/MMR working [E2, E6, E8].
Only a server admin may install provider definitions or bind them to collections.
Use a startup config file, not a new configuration endpoint in this slice.
Store the registry and bindings outside the database; never write secrets into `.vlite`.
Secrets come only from named environment variables or private files.
Reject literal secrets, unreadable files and invalid config at startup; never silently disable a binding.
Resolve bindings by database lineage, collection identity and tenant, not a bare collection name.
A server default is only a fallback for explicitly allowed collections; a collection override may narrow it.
Requests select an authorized provider alias. They cannot supply URLs, models, headers or secret references.
No data leaves the host without an admin's explicit endpoint and egress configuration.
Allowlist endpoint schemes, hosts and resolved addresses; disable redirects and recheck DNS on connection.
Require TLS for non-loopback endpoints; local HTTP requires an explicit exception.
Never infer provider enablement from ambient credentials alone.
Current HTTP code uses env settings, a 30 s timeout, no redirects and a 4 MiB response cap (`src/embedding.rs:66-96`, `src/embedding.rs:116-135`).
The managed contract must not claim those defaults provide the new isolation policy.
Open: migrate legacy env-configured `http` into admin bindings without silently changing existing grants.

| Proposed bound | Contract |
|---|---|
| Embedding input | One UTF-8 text per request; 16,384 bytes maximum; no new batch endpoint |
| Rerank input | `k <= candidates <= 1000` for custom mode; default `max(k,50)`; existing modes unchanged |
| Provider batching | One embedding input or at most 1,000 candidates per call; 1 MiB encoded request; no hidden splitting |
| Response | At most 4 MiB per provider call; finite vectors/scores; exact dimensions and cardinality |
| Time | 2 s connect, 10 s total including queue/serialization/retry; admin may lower; client deadline wins |
| Retry | At most one jittered retry for connection failure, 429 or 503, only if provider declares replay safe |
| Retry budget | Respect Retry-After only within remaining deadline; no retries for auth, invalid data or timeout |
| Admission | Bounded per-provider and per-tenant concurrency; reject overload rather than unbounded queues |

These are proposed defaults, not measurements; current text/dimension checks are `src/server/features.rs:585-588`, `src/server/features.rs:625-629`.
Authorize `search` plus the collection binding before resolving aliases or making network calls.
Current REST embedding already needs `search` (`src/server.rs:885-899`).
Add no credential scope in this slice. Provider access is an additional admin-controlled allowlist.
Read-only credentials may invoke enabled read operations; they cannot configure providers.
Current read-only routing includes embedding (`src/server.rs:947-960`).
A tenant alias cannot resolve another tenant's secret or provider binding.
Do not share response caches across tenants; disable provider caching initially.
Snapshot candidate IDs, vectors and approved payload fields, then release the database guard before provider work.
Reauthorize before dispatch if credential or binding state changes while queued.
Return only unique candidate IDs, at most k results; reject foreign IDs and nonfinite scores.
No fallback after provider failure; do not expose response bodies, secrets, text, vectors or payloads in logs/errors.
Record alias, tenant-safe attribution, duration, batch count and sanitized outcome only.

Current `Reranker` accepts a vector query, candidate vectors/payloads, and returns `SearchHit` (`src/rerank.rs:4-17`).
Propose custom adapters with finite lower-is-better costs in the trait's result.
Expose `rerank_score = -cost`, ordered descending then numeric ID; retain original metric `distance`.
Allowlist payload fields per binding; reject payload-free adapters that unexpectedly request text.
Open: a raw-text cross-encoder context API. Do not pretend the current vector-only trait supplies query text.
Open: provider model/version pinning and the treatment of candidates deleted during external work.

| Proposed failure | REST | gRPC |
|---|---|---|
| Missing bearer / wrong scope or tenant | 401 `unauthorized` / 403 `forbidden_scope` | UNAUTHENTICATED / PERMISSION_DENIED |
| Alias absent or disabled within authorized collection | 409 `provider_not_configured` | FAILED_PRECONDITION |
| Invalid input or custom candidate depth | 422 `invalid_provider_input` | INVALID_ARGUMENT |
| Response shape, dimensions, IDs or score invalid | 502 `provider_invalid_response` | UNAVAILABLE |
| Provider connect/auth/availability failure | 503 `provider_unavailable` | UNAVAILABLE |
| Total deadline | 504 `provider_timeout` | DEADLINE_EXCEEDED |
| Provider/tenant admission full | 429 `provider_busy` | RESOURCE_EXHAUSTED |

All new codes/mappings need approval; provider auth failure is not caller HTTP 401.
The current generic mapper would turn new 502/504 errors into INTERNAL (`src/server/grpc.rs:181-196`).
Add explicit mappings; keep existing codes unchanged and carry `vlite-error-code` (`src/server/grpc.rs:199-215`).

## Gap 12: graph RPC parity

Propose `vlite.v1.Graph` beside VLite, sharing admission/authentication (`src/server/grpc.rs:94-105`, `src/server/grpc.rs:220-306`).
All methods below are unary. Prefix REST suffixes with `/v1/graphs/{graph}` unless shown absolute.
Routes are verified in `src/server/graph.rs:123-184`; MCP equivalents in [G3].

| Proposed RPC | REST method and route | MCP equivalent | Scope |
|---|---|---|---|
| ListGraphs | GET `/v1/graphs` | `vlite_graph_list` | graph_read |
| CreateGraph | POST `/v1/graphs` | `vlite_graph_create` | graph_write |
| GetGraph | GET base | No separate info tool; list | graph_read |
| DropGraph | DELETE base | `vlite_graph_drop` | graph_write |
| GetNode | POST `/nodes/get` | `vlite_graph_get` with node | graph_read |
| GetEdge | POST `/edges/get` | `vlite_graph_get` with edge | graph_read |
| FindNodes | POST `/nodes/find` | `vlite_graph_find_nodes` | graph_read |
| Neighbors | POST `/neighbors` | `vlite_graph_neighbors` | graph_read |
| Traverse | POST `/traverse` | `vlite_graph_traverse` | graph_read |
| ShortestPath | POST `/shortest-path` | `vlite_graph_shortest_path` | graph_read |
| Explain | POST `/explain` | Typed read tool with `explain:true` | graph_read |
| GetSchema | GET `/schema` | `vlite_graph_schema` | graph_read |
| PutSchema | PUT `/schema` | `vlite_graph_put_schema` | graph_write |
| Retrieve | POST `/retrieve` | `vlite_graph_retrieve` | graph_read + pivot read |
| QueryText | POST `/query` | `vlite_graph_query` | graph_read |
| WriteGraphBatch | POST `/batch` | `vlite_graph_write` | graph_write + affected collection write |
| GetGraphStats | GET `/v1/admin/graph/stats` | None | admin only |

HEAD on REST GET routes needs no separate RPC. Stats is admin-only (`src/server/graph.rs:54-56`, `src/server.rs:926`).
Reuse graph binding checks before lookup and filtered lists (`src/server/graph.rs:317-325`, `src/server/graph.rs:372-388`).
Qualify tenant-local graph names and unqualify responses; never accept a physical name as an isolation bypass.
Current qualification is `src/server/graph.rs:37-44`, `src/server/graph.rs:317-325`.
Preserve stored collection names in attachments/pivots; do not tenant-qualify them a second time.
Retrieve requires pivot `read`; omit unauthorized evidence and report `redacted` (`src/server/graph.rs:572-646`).
Batch authorization must include attach/detach/vector mutations and deleting attached nodes, under the write guard.
Reuse quotas and atomic authorization (`src/server/graph.rs:713-731`, `src/server/graph.rs:735-792`).
Never treat `collections:["*"]` as graph scope (`src/server/graph.rs:20-34`).

| Read limit | Default / ceiling |
|---|---|
| hops; visited nodes | 2 / 4; 1,000 / 10,000 |
| examined edges; edges per node | 10,000 / 100,000; 1,000 / 10,000 |
| deadline milliseconds | 250 / 5,000 |
| output nodes; edges; paths | 100 / 1,000; 200 / 2,000; 20 / 100 |

Source: `src/property_graph/engine/limits.rs:87-108`; reject above ceilings, never clamp (`src/property_graph/engine/limits.rs:125-157`).
Zero hops/output is valid; zero work budgets are not (`src/property_graph/engine/limits.rs:65-69`).
Carry all model/schema caps too: 64 graphs, 256-byte keys, 8 labels, 64 properties and 65,536 property bytes.
Batch caps remain 4,096 entries and 16,777,216 encoded bytes; schema scan default/ceiling is 100,000/1,000,000.
These and the remaining schema/name/string limits come from `src/property_graph/engine/limits.rs:10-60`.
Current gRPC transport caps are 8 MiB request / 16 MiB response (`proto/vlite/v1/vlite.proto:11`).
Keep these smaller wire caps; preserve view_sequence, cursors, complete, truncation and stats (`src/property_graph/read.rs:800-805`).
Use paged unary reads; do not stream a traversal across changing snapshots.
Keep Changes/WatchChanges for graph events; add no second watch API [F3].
Pass cancellation into reads; use the smaller graph/client deadline. Partial results must remain explicitly incomplete.
Open: deadline cancellation versus transport DEADLINE_EXCEEDED when a bounded partial result is ready.
Preserve graph conflict 409 -> FAILED_PRECONDITION, including graph_not_found and stale cursors; invalid input 422 -> INVALID_ARGUMENT.
Sources: `src/server.rs:1587-1650`, `src/server/graph.rs:689-700`, `src/server/grpc.rs:181-215`.

## Exact approval inventory: no schema edits in this draft

OpenAPI changes proposed for `src/openapi.json`:
1. Extend existing EmbedRequest `provider` from hashing/http to validated aliases; add a typed EmbedResponse.
2. Extend RerankOptions with mode `custom` and `provider`; provider is required only for custom mode.
3. Add optional `rerank_score` to search hit schemas and optional `ranking:{provider,order}` to search responses.
4. On existing embed and both search POST routes, document auth, alias policy, custom limits and all new error rows above.
5. Add named ProviderError response schemas using the existing Error envelope; document Retry-After for provider_busy.
No new REST routes, config routes, credential scopes or graph paths are proposed.
Current embed and rerank schemas are `src/openapi.json:47`, `src/openapi.json:145-154`.
Graph paths already merge from `src/openapi_graph.json` (`src/server/graph.rs:1757-1787`); no fragment change proposed.

Proto additions proposed for `proto/vlite/v1/vlite.proto`:
1. Add VLite.EmbedText with new EmbedTextRequest `{collection=1,text=2,provider=3}` and EmbedTextResponse `{vector=1,dimensions=2,provider=3}`.
2. Add RERANK_MODE_CUSTOM=3; optional SearchRequest.rerank_provider=18; optional Hit.rerank_score=3.
3. Add Ranking `{provider=1,order=2}` and optional SearchResponse.ranking=3; define order as `score_desc_id_asc`.
4. Add Graph service with exactly the 17 unary methods in the parity table.
5. Add `<Method>Request`/`<Method>Response` for each of the 17 methods; shared GraphSelector, GraphLimits, GraphProjection, GraphReadMeta, GraphSchema and GraphWriteOperation.
6. GraphWriteOperation must cover create/upsert/patch/delete node, create/patch/delete/ensure edge, attach/detach, put/delete vector and put schema.
7. Preserve typed properties, expected revisions, optional presence, u64 IDs and opaque cursor strings; reuse MetadataValue/Expected.
8. Add documented provider error mappings/trailers; keep GraphChangeOperation and existing stream messages unchanged.
Current search tags/enums: `proto/vlite/v1/vlite.proto:239-261`, `proto/vlite/v1/vlite.proto:266-269`, `proto/vlite/v1/vlite.proto:289-292`, `proto/vlite/v1/vlite.proto:352-356`.
Open: review all new graph fields/tags and supporting node/edge/path/schema/retrieval types before schema approval.
Use additive v1 services/messages/fields only. Never reuse tags or change existing enum meanings.
Old clients keep existing modes; new clients must handle UNIMPLEMENTED from older servers, without silent fallback.

## Acceptance sketches and test matrix

Examples are proposed sketches; auth is omitted. Collection `docs` allows alias `local-semantic` and has three dimensions.
`POST /v1/collections/docs/embed {"text":"hello","provider":"local-semantic"}`
=> `200 {"vector":[0.1,0.2,0.3],"dimensions":3,"provider":"local-semantic"}`.
`Search {collection:"docs",vector:[0.1,0.2,0.3],k:1,rerank_mode:CUSTOM,rerank_provider:"rank-a",rerank_candidates:50}`
=> `{hits:[{id:7,distance:0.4,rerank_score:0.8}],ranking:{provider:"rank-a",order:"score_desc_id_asc"}}`.
`Graph.Traverse {graph:"notes",start:[{key:"a"}],limits:{max_hops:1}}`
=> `{nodes:[{id:1,key:"a"}],edges:[],view_sequence:42,complete:true,truncation:[],stats:{...}}`.
`Graph.Traverse {graph:"notes",limits:{max_hops:5}}`
=> `INVALID_ARGUMENT`, trailer `vlite-error-code:graph_limit_exceeds_ceiling`.
`Graph.GetGraph {graph:"outside-grant"}` => PERMISSION_DENIED / forbidden_scope, whether it exists or not.

| Area | Required cases and assertion |
|---|---|
| Defaults | No provider config: zero outbound calls; hashing and exact/MMR unchanged; old clients still work |
| Auth negative | Missing/expired/revoked key; wrong collection, operation or graph grant; fail before provider call/lookup |
| Admin negative | Viewer/editor/collection manager cannot install aliases, change endpoints or read secrets |
| Tenant negative | Cross-tenant alias, physical graph name, attachment collection and wildcard; no data or existence leak |
| Tenant lifecycle | Suspended/deleted tenant and binding revoked while queued; reject without egress |
| Graph scope | Collection wildcard without graph_read denied; read-only token write denied; filtered lists match REST |
| Retrieve/batch | Missing pivot read denied; evidence redacted; vector write scope/quota failure rolls back whole batch |
| Provider robustness | Timeout, overload, malformed JSON, NaN, wrong dimensions/cardinality, foreign/duplicate IDs, secret redaction |
| Egress/retry | Redirect/DNS escape blocked; safe transient retry <=1; no auth retry; deadline includes queue and attempts |
| Limits | Every default, ceiling, ceiling+1 and zero case; UTF-8 bytes, transport sizes, batch count/bytes |
| Parity | Every listed graph RPC matches REST/MCP output, ordering, schema, conditional writes and error code |
| Pagination | Same view metadata; mutation invalidates cursor; exhaustion is not certified absence |
| Streaming | Existing graph feed auth/filter/resume unchanged; disconnect cannot cause replayed graph writes |
| Compatibility | Old generated client/new server and reverse; absent optional fields; custom mode on old server fails explicitly |

## Competitors, slices and decisions

Public documentation could not be retrieved for this draft; no competitor capability is asserted.
| Product | Evidence status / comparison to resolve |
|---|---|
| Qdrant | unverified; Open: inference configuration and reranking ownership |
| Weaviate modules | unverified; Open: vectorizer/reranker module enablement and credential handling |
| Milvus | unverified; Open: embedding functions, batch limits and reranking contracts |
| Neo4j drivers | unverified; Open: driver query/result streaming and cancellation semantics |

Delivery: first approve schemas, defaults and auth rules; then shared admin provider registry and bounded adapters.
Next wire REST/gRPC embedding/custom ranking and Python types; then graph unary parity and generated clients.
Last add parity/auth/compatibility tests and update API/MCP/security/graph references. No benchmark campaign is needed.
Risks: external data disclosure, unsafe endpoint resolution, tenant secret mixing, lock-held inference and score confusion.
Open: text cross-encoder API, candidate deletion semantics, model pinning, concurrency defaults and graph field/tag manifest.
Open: whether to reconcile existing REST/gRPC rerank defaults in a separately approved compatibility change.
