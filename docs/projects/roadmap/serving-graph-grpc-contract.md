# Graph gRPC exact schema contract (Gap 12)

Status: exact-schema review draft, 2026-11-02; not implemented or shipped.
This document is conformed to the [serving wire conventions proposal](serving-wire-conventions.md), whose owner decisions remain open.
The public API direction is approved, conditional on exact-schema review before
code (`docs/projects/roadmap/next-major.md:49-55`). This document supplies that
review artifact; it does not itself approve implementation or change
`proto/vlite/v1/vlite.proto`. All wire names below are **proposed protobuf
names**, not claims about existing Rust symbols.

Parent: [serving contract](serving-contract.md), Gap 12. Scope: the exact
unary `vlite.v1.Graph` methods defined in this document, REST/MCP parity,
clients and acceptance.
Non-goals: new query-language features, algorithms, temporal selectors, memory,
KV/TTL, providers, storage formats, streaming traversal or a second change feed.
`QueryText` only transports the existing read-only language; it does not extend
it (`src/server/graph.rs:650-686`). Sibling contracts append separately.

## 1. Testable requirements

1. **G12-01:** append exactly the service and messages in section 3 after schema
   approval. Preserve every existing descriptor, tag, enum meaning and stream.
2. **G12-02:** every RPC follows section 2's coverage and section 5's route,
   authorization and limits matrix, with no HTTP loopback or silent fallback.
3. **G12-03:** preserve full-width IDs/revisions/sequences, typed properties,
   optional presence, ordering, schema observations, outcomes and opaque cursors.
4. **G12-04:** reject ceilings rather than clamp; distinguish valid zero output
   budgets from invalid zero work/page budgets. Do not change REST defaults.
5. **G12-05:** enforce graph grants before lookup; qualify graph names only,
   filter lists, redact unauthorized evidence, and authorize batches atomically.
6. **G12-06:** return precisely section 6's status/trailer pairs; incomplete
   success must carry reasons and cannot certify a negative path result.
7. **G12-07:** preserve one-view reads and cursor invalidation, apply read
   deadlines/cancellation as section 4 states, and fail explicitly at the
   response size cap.
8. **G12-08:** require independent descriptor/oracle, parity, compatibility,
   authorization, bounds and mutation tests before each delivery is accepted.

## 2. Current coverage and evidence

The following describes this checkout, not the proposed implementation.

| Surface | Current coverage and exact evidence |
|---|---|
| Proto service | Only application service `VLite`, 34 RPCs; `WatchChanges` and `ExportVectors` are server streams, `StreamImport` a client stream; no `Graph` service (`proto/vlite/v1/vlite.proto:16-60`). Health is registered separately (`src/server/grpc.rs:94-101`). |
| Reusable messages | `GeoPoint` tags 1-2; `MetadataValue.kind` tags 1-5; `Expected.kind` tags 1-3, nested empty `Any`/`Absent` (`proto/vlite/v1/vlite.proto:98-123`). `Condition` contains Eq/Range/GeoRadius oneof tags 1-3 (`proto/vlite/v1/vlite.proto:222-238`). |
| Existing vector/search allocations | `Vector` 1-6; `PutVectorRequest` 1-7; `SearchRequest` 1-17; `Hit` 1-2; `SearchResponse` 1-2; `RerankMode` 0-2 (`proto/vlite/v1/vlite.proto:130-148`, `proto/vlite/v1/vlite.proto:240-266`, `proto/vlite/v1/vlite.proto:289-292`, `proto/vlite/v1/vlite.proto:352-356`). No changes here. Gap 8 reserves `VLite.EmbedText`, `RERANK_MODE_CUSTOM=3`, `SearchRequest.rerank_provider=18`, `Hit.rerank_score=3`, `Ranking` and `SearchResponse.ranking=3` (`docs/projects/roadmap/serving-contract.md:199-201`); this document uses none of those names or tags. |
| Existing graph feed | `GraphChangeOp` values 0-9; `GraphChangeOperation` tags 1-18; `ChangeEvent` 1-15; `ChangesResponse` 1-4; `ChangesRequest` 1-2; `WatchChangesRequest` 1-3 (`proto/vlite/v1/vlite.proto:455-530`). Keep all unchanged, including `schema_json=18`. |
| REST lifecycle | `list`, `create`, `info_route`, `drop_route`, registered on `/v1/graphs` and `/v1/graphs/{graph}` (`src/server/graph.rs:175-179`, `src/server/graph.rs:372-438`). |
| REST typed reads | Six `QUERIES` and `query_route`/`run_query`; `explain`, `schema`, `put_schema`, `retrieve`, `query_text`, `batch`, `guard_stats` (`src/server/graph.rs:123-184`, `src/server/graph.rs:440-574`, `src/server/graph.rs:596-792`). |
| REST published shapes | Typed selectors/limits/projection and read envelope (`src/openapi_graph.json:405-564`); batch, schema, retrieve and query declarations also exist (`src/openapi_graph.json:2127-2171`, `src/openapi_graph.json:2326-2391`, `src/openapi_graph.json:2660-2808`, `src/openapi_graph.json:2991-3021`). Executable parsers/encoders are the precision authority where those declarations are loose: `src/server/graph.rs:911-1013`, `src/bin/tool_support/graph_json.rs:1134-1185`, `src/bin/tool_support/graph_json.rs:1358-1486`, `src/bin/tool_support/graph_json.rs:1524-1626`. |
| MCP | Tool dispatch: list/create/drop/write, node-or-edge get, find/neighbors/traverse/shortest path, schema/put-schema/retrieve/query; typed tools accept `explain:true` (`src/bin/tool_support/graph_mcp.rs:34-53`, `src/bin/tool_support/graph_mcp.rs:119-136`, `src/bin/tool_support/graph_mcp.rs:152-282`). No separate graph-info or stats tool in that dispatch. |
| MCP security boundary | Local tools operate on a supplied database and read-only flag, not server bearer grants; writes require writable mode and graph tools require a graph-enabled file (`src/bin/tool_support/graph_mcp.rs:1-12`, `src/bin/tool_support/graph_mcp.rs:139-157`). Do not claim local MCP authenticates REST credentials. |
| Python client | Packaged client is synchronous REST, no runtime dependencies (`clients/python/README.md:1-16`); HTTP implementation imports urllib (`clients/python/vlite_client/__init__.py:9-11`). gRPC is available through externally generated Python stubs in smoke tests, not that package (`tests/grpc_smoke.py:1-10`, `tests/grpc_smoke.py:51-83`). |
| JavaScript client | Packaged client is ESM REST, no runtime dependencies (`clients/javascript/README.md:1-8`); transport uses fetch (`clients/javascript/index.js:124-139`), not a packaged gRPC path. |
| .NET client | REST HTTP client (`clients/dotnet/VLite.Client/Client.cs:30-34`, `clients/dotnet/VLite.Client/Client.cs:59-67`); project declares a dependency-free REST client, no gRPC package/proto generation (`clients/dotnet/VLite.Client/VLite.Client.csproj:1-12`). |
| Build/reference | Rust builds client/server descriptors via protox and tonic, gated by `grpc` (`build.rs:1-18`); Python smoke generates stubs in a temporary directory (`tests/grpc_smoke.py:51-83`). Public scan extracts RPC names from the shipped proto (`tests/api_reference.py:445-446`); reference rows use `data-api="grpc:Rpc"` (`site/api.html:1509-1519`). |

## 3. Exact proposed proto manifest

Compile this as `graph_contract.proto` alongside the unchanged current proto.
For eventual delivery, append its definitions to the existing file, omitting
the duplicate syntax/package and self-import. Existing `MetadataValue`,
`Expected`, `Condition`, `Metric` and `VectorInput` are reused, not redefined.
Their wire declarations are `proto/vlite/v1/vlite.proto:98-123`,
`proto/vlite/v1/vlite.proto:200-206`, `proto/vlite/v1/vlite.proto:222-238`,
and `proto/vlite/v1/vlite.proto:378`.

Tags are message-local, not service-global. Allocate core tags 1-99 additively;
never renumber or reuse a removed field/enum value, and reserve its number
**and name**. Never use protobuf's 19000-19999 range.
**Withdrawn:** permanent empty sibling fences prevent atomic mixed writes;
the owner-approved replacement is [wire conventions section 1.3](serving-wire-conventions.md#13-field-number-allocation-c1),
decision 1 (option A, 2026-11-02). Its sibling-owned bands apply to `GraphSelector`,
`GraphLimits`, `GraphProjection`, `GraphReadMeta`, `GraphSchema`,
`GraphWriteOperation`, the nine read requests and `GraphEdge`; the block below
declares their initial fences. A sibling carves its band's first tag for one
domain-message field or arm and updates the named descriptor manifest.
Graph-attached messages use `GraphAlgorithm*`, `GraphTemporal*`, `GraphExpiry*`;
separate `Kv` and `Memory` services use `Kv*` and `Memory*`, per
[section 1.4](serving-wire-conventions.md#14-service-versus-arm-prefix-and-package-rules-c16).
The proposed TTL arm is `GraphExpiryOperation expiry = 400`;
`WriteExpiryBatch` only wraps that same message. Tags 500 and up require
another exact-schema review.
**Owner-approved (2026-11-02):** KEEP `GraphWriteOperation`'s
`reserved 14 to 31;` under wire decision 20 option B, contrary to the earlier
recommendation to drop it. GR12 / the Gap 12 core graph protocol owner owns
these tags for future core atomic graph-batch operation arms. They remain
reserved until a separate exact-schema review names an operation and explicitly
releases its tag. This does not approve any new operation or lift the proto freeze.

The following summaries follow [wire conventions sections 1.1 and 1.2](serving-wire-conventions.md#11-services-all-package-vlitev1-appended-to-protovlitev1vliteproto);
they are landing obligations, not claims of shipped services.

| Service | RPCs at landing / later additions | Health name | Owner |
|---|---|---|---|
| `VLite` | Existing baseline; Gap 8 and precision append their named RPCs | `""`, `vlite.v1.VLite` | Existing and sibling slices |
| `Graph` | 17 unary RPCs below; later slices append in landing order: TTL `GetNodeExpiry`, `GetEdgeExpiry`, `WriteExpiryBatch`; temporal `EdgeTimeline`; five ALG RPCs | `vlite.v1.Graph` | Gap 12; later slices append |
| `Kv` | Seven RPCs in the note's manifest | `vlite.v1.Kv` | KV |
| `Memory` | Nine RPCs in the note's manifest | `vlite.v1.Memory` | MEM |
| `GraphTtl` | Proposed not created; TTL RPCs append to `Graph` | None | TTL |
| `vlite.serving.v1` | Transport-only messages, no service | None | Binary results |

| Per-service obligation | Gap 12 landing requirement |
|---|---|
| Registration | Own adapter module; one `add_service` on shared `Routes`; `grpc.rs` retains routing, health, caps and shared error mapping |
| Message caps | Set 8 MiB decode and 16 MiB encode on each service, with a cap test |
| Health | Add `vlite.v1.Graph` to `GrpcHealth::set`, the `set_not_serving` doc comment and the `grpc_smoke` health loop |
| Descriptor test | List every service, unique RPC short name and shared-message tag manifest; later slices append named entries |

```proto3
syntax = "proto3";
package vlite.v1;
import "vlite/v1/vlite.proto";

service Graph {
  rpc ListGraphs(ListGraphsRequest) returns (ListGraphsResponse);
  rpc CreateGraph(CreateGraphRequest) returns (CreateGraphResponse);
  rpc GetGraph(GetGraphRequest) returns (GetGraphResponse);
  rpc DropGraph(DropGraphRequest) returns (DropGraphResponse);
  rpc GetNode(GetNodeRequest) returns (GetNodeResponse);
  rpc GetEdge(GetEdgeRequest) returns (GetEdgeResponse);
  rpc FindNodes(FindNodesRequest) returns (FindNodesResponse);
  rpc Neighbors(NeighborsRequest) returns (NeighborsResponse);
  rpc Traverse(TraverseRequest) returns (TraverseResponse);
  rpc ShortestPath(ShortestPathRequest) returns (ShortestPathResponse);
  rpc Explain(ExplainRequest) returns (ExplainResponse);
  rpc GetSchema(GetSchemaRequest) returns (GetSchemaResponse);
  rpc PutSchema(PutSchemaRequest) returns (PutSchemaResponse);
  rpc Retrieve(RetrieveRequest) returns (RetrieveResponse);
  rpc QueryText(QueryTextRequest) returns (QueryTextResponse);
  rpc WriteGraphBatch(WriteGraphBatchRequest) returns (WriteGraphBatchResponse);
  rpc GetGraphStats(GetGraphStatsRequest) returns (GetGraphStatsResponse);
}

enum GraphDirection { GRAPH_DIRECTION_OUT = 0; GRAPH_DIRECTION_IN = 1; GRAPH_DIRECTION_BOTH = 2; }
enum GraphPropertyType {
  GRAPH_PROPERTY_TYPE_UNSPECIFIED = 0; GRAPH_PROPERTY_TYPE_STRING = 1;
  GRAPH_PROPERTY_TYPE_INTEGER = 2; GRAPH_PROPERTY_TYPE_FLOAT = 3;
  GRAPH_PROPERTY_TYPE_BOOLEAN = 4; GRAPH_PROPERTY_TYPE_GEOPOINT = 5;
}
enum GraphPropertyIndex { GRAPH_PROPERTY_INDEX_UNSPECIFIED = 0; GRAPH_PROPERTY_INDEX_EXACT = 1; GRAPH_PROPERTY_INDEX_RANGE = 2; }
enum GraphMultiplicity { GRAPH_MULTIPLICITY_MULTI = 0; GRAPH_MULTIPLICITY_UNIQUE_TRIPLE = 1; }
enum GraphSchemaOrigin { GRAPH_SCHEMA_ORIGIN_UNSPECIFIED = 0; GRAPH_SCHEMA_ORIGIN_DECLARED = 1; GRAPH_SCHEMA_ORIGIN_OBSERVED = 2; GRAPH_SCHEMA_ORIGIN_BOTH = 3; }
enum GraphTruncationKind {
  GRAPH_TRUNCATION_UNSPECIFIED = 0; GRAPH_TRUNCATION_VISITED_LIMIT = 1;
  GRAPH_TRUNCATION_EDGE_LIMIT = 2; GRAPH_TRUNCATION_DEGREE_LIMIT = 3;
  GRAPH_TRUNCATION_OUTPUT_LIMIT = 4; GRAPH_TRUNCATION_DEADLINE = 5; GRAPH_TRUNCATION_CANCELLED = 6;
}
enum GraphIndexKind {
  GRAPH_INDEX_UNSPECIFIED = 0; GRAPH_INDEX_NODE_ID_LOOKUP = 1; GRAPH_INDEX_NODE_KEY_INDEX = 2;
  GRAPH_INDEX_EDGE_ID_LOOKUP = 3; GRAPH_INDEX_EDGE_KEY_INDEX = 4; GRAPH_INDEX_LABEL_INDEX_SCAN = 5;
  GRAPH_INDEX_FULL_NODE_SCAN = 6; GRAPH_INDEX_PROPERTY_EXACT_INDEX = 7;
  GRAPH_INDEX_PROPERTY_RANGE_INDEX = 8; GRAPH_INDEX_ADJACENCY_RANGE_SCAN = 9;
}
enum GraphPathOutcome { GRAPH_PATH_UNSPECIFIED = 0; GRAPH_PATH_FOUND = 1; GRAPH_PATH_NO_PATH_WITHIN_HOPS = 2; GRAPH_PATH_EXHAUSTED = 3; }

message GraphUnit {}
message GraphStrings { repeated string values = 1; }
message GraphProperties { map<string, MetadataValue> values = 1; }
message GraphSelector {
  oneof kind { uint64 id = 1; string key = 2; }
  reserved 100 to 199; reserved 200 to 299; reserved 300 to 399; reserved 400 to 499;
}
message GraphProjection {
  oneof kind { GraphUnit all = 1; GraphUnit none = 2; GraphStrings only = 3; }
  reserved 100 to 199; reserved 200 to 299; reserved 300 to 399; reserved 400 to 499;
}
message GraphLimits {
  optional uint64 max_hops = 1; optional uint64 max_visited_nodes = 2;
  optional uint64 max_edges_examined = 3; optional uint64 max_edges_per_node = 4;
  optional uint64 deadline_ms = 5; optional uint64 max_output_nodes = 6;
  optional uint64 max_output_edges = 7; optional uint64 max_output_paths = 8;
  reserved 100 to 199; reserved 200 to 299; reserved 300 to 399; reserved 400 to 499;
}
message GraphNodeFilter { repeated string labels = 1; repeated Condition conditions = 2; }
message GraphGetNode { GraphSelector node = 1; }
message GraphGetEdge { GraphSelector edge = 1; }
message GraphFindNodes { optional string label = 1; repeated Condition conditions = 2; optional uint64 limit = 3; optional string cursor = 4; }
message GraphNeighbors {
  GraphSelector node = 1; GraphDirection direction = 2; GraphStrings types = 3;
  repeated Condition edge_conditions = 4; optional uint64 limit = 5; optional string cursor = 6;
}
message GraphTraverse {
  repeated GraphSelector start = 1; GraphDirection direction = 2; GraphStrings types = 3;
  GraphNodeFilter expand_filter = 4; GraphNodeFilter return_filter = 5;
  repeated Condition edge_filter = 6; bool include_paths = 7;
}
message GraphShortestPath { GraphSelector source = 1; GraphSelector destination = 2; GraphDirection direction = 3; GraphStrings types = 4; }
message GraphReadQuery {
  oneof operation {
    GraphGetNode get_node = 1; GraphGetEdge get_edge = 2; GraphFindNodes find_nodes = 3;
    GraphNeighbors neighbors = 4; GraphTraverse traverse = 5; GraphShortestPath shortest_path = 6;
  }
}
message GraphTruncation { GraphTruncationKind kind = 1; repeated uint64 node_ids = 2; }
message GraphIndexChoice { GraphIndexKind kind = 1; optional GraphDirection direction = 2; optional bool typed = 3; }
message GraphReadStats {
  uint64 nodes_visited = 1; uint64 nodes_scanned = 2; uint64 edges_examined = 3;
  uint64 nodes_returned = 4; uint64 edges_returned = 5; uint64 paths_returned = 6;
  uint64 elapsed_ms = 7; repeated GraphIndexChoice indexes = 8;
}
message GraphReadMeta {
  string graph = 1; uint64 view_sequence = 2; bool complete = 3;
  repeated GraphTruncation truncation = 4; GraphReadStats stats = 5; optional string cursor = 6;
  reserved 100 to 199; reserved 200 to 299; reserved 300 to 399; reserved 400 to 499;
}
message GraphInfo { string name = 1; uint64 node_count = 2; uint64 edge_count = 3; }
message GraphNode { uint64 id = 1; string key = 2; repeated string labels = 3; map<string, MetadataValue> properties = 4; uint64 revision = 5; }
message GraphEdge {
  uint64 id = 1; optional string key = 2; string relationship = 3; uint64 source = 4;
  string source_key = 5; uint64 target = 6; string target_key = 7;
  map<string, MetadataValue> properties = 8; uint64 revision = 9;
  reserved 100 to 199; reserved 200 to 299; reserved 300 to 399; reserved 400 to 499;
}
message GraphPath { repeated uint64 nodes = 1; repeated uint64 edges = 2; }
message GraphTraversedNode { uint64 depth = 1; GraphNode node = 2; }
message GraphNodeResult { GraphNode node = 1; }
message GraphEdgeResult { GraphEdge edge = 1; }
message GraphNodesResult { repeated GraphNode nodes = 1; }
message GraphNeighborsResult { repeated GraphNode nodes = 1; repeated GraphEdge edges = 2; }
message GraphTraversalResult { repeated GraphTraversedNode nodes = 1; repeated GraphEdge edges = 2; repeated GraphPath paths = 3; }
message GraphPathResult {
  GraphPathOutcome outcome = 1; GraphPath path = 2; repeated GraphNode nodes = 3;
  repeated GraphEdge edges = 4; repeated GraphTruncation truncation = 5;
}
message GraphReadResult {
  GraphReadMeta meta = 1;
  oneof output {
    GraphNodeResult node = 2; GraphEdgeResult edge = 3; GraphNodesResult nodes = 4;
    GraphNeighborsResult neighbors = 5; GraphTraversalResult traversal = 6; GraphPathResult shortest_path = 7;
  }
}
message GraphExplanation { GraphReadQuery query = 1; GraphLimits limits = 2; GraphProjection projection = 3; repeated GraphIndexChoice indexes = 4; }

message GraphPropertyDefinition { GraphPropertyType type = 1; bool required = 2; bool unique = 3; optional GraphPropertyIndex index = 4; }
message GraphLabelDefinition { string description = 1; map<string, GraphPropertyDefinition> properties = 2; }
message GraphEndpoint { string source = 1; string destination = 2; }
message GraphRelationshipDefinition {
  string description = 1; repeated GraphEndpoint endpoints = 2;
  GraphMultiplicity multiplicity = 3; map<string, GraphPropertyDefinition> properties = 4;
}
message GraphSchema {
  map<string, GraphLabelDefinition> labels = 1; map<string, GraphRelationshipDefinition> relationships = 2;
  reserved 100 to 199; reserved 200 to 299; reserved 300 to 399; reserved 400 to 499;
}
message GraphObservedProperty { string name = 1; GraphSchemaOrigin origin = 2; uint64 count = 3; repeated GraphPropertyType types = 4; }
message GraphObservedLabel { string name = 1; GraphSchemaOrigin origin = 2; uint64 count = 3; repeated GraphObservedProperty properties = 4; }
message GraphObservedEndpoint { string source = 1; string destination = 2; GraphSchemaOrigin origin = 3; uint64 count = 4; }
message GraphObservedRelationship {
  string name = 1; GraphSchemaOrigin origin = 2; uint64 count = 3;
  repeated GraphObservedEndpoint endpoints = 4; repeated GraphObservedProperty properties = 5;
}
message GraphObservedSchema { bool complete = 1; uint64 entities_scanned = 2; repeated GraphObservedLabel labels = 3; repeated GraphObservedRelationship relationships = 4; }

message GraphCreateNode { string key = 1; repeated string labels = 2; map<string, MetadataValue> properties = 3; }
message GraphUpsertNode { string key = 1; repeated string labels = 2; map<string, MetadataValue> properties = 3; Expected expected = 4; }
message GraphPatchNode {
  string key = 1; repeated string add_labels = 2; repeated string remove_labels = 3;
  map<string, MetadataValue> set = 4; repeated string unset = 5; Expected expected = 6;
}
message GraphDeleteNode { string key = 1; Expected expected = 2; }
message GraphCreateEdge { optional string key = 1; string source = 2; string relationship = 3; string destination = 4; map<string, MetadataValue> properties = 5; }
message GraphPatchEdge { GraphSelector edge = 1; map<string, MetadataValue> set = 2; repeated string unset = 3; Expected expected = 4; }
message GraphDeleteEdge { GraphSelector edge = 1; Expected expected = 2; }
message GraphAttach { string node = 1; string collection = 2; uint64 record = 3; }
message GraphDetach { string node = 1; string collection = 2; }
message GraphPutVector { string collection = 1; uint64 id = 2; repeated float vector = 3; string payload = 4; GraphProperties metadata = 5; }
message GraphDeleteVector { string collection = 1; uint64 id = 2; }
message GraphEnsureEdge { string source = 1; string relationship = 2; string destination = 3; map<string, MetadataValue> properties = 4; }
message GraphPutSchema { GraphSchema schema = 1; }
message GraphWriteOperation {
  oneof operation {
    GraphCreateNode create_node = 1; GraphUpsertNode upsert_node = 2;
    GraphPatchNode patch_node = 3; GraphDeleteNode delete_node = 4;
    GraphCreateEdge create_edge = 5; GraphPatchEdge patch_edge = 6; GraphDeleteEdge delete_edge = 7;
    GraphAttach attach = 8; GraphDetach detach = 9; GraphPutVector put_vector = 10;
    GraphDeleteVector delete_vector = 11; GraphEnsureEdge ensure_edge = 12; GraphPutSchema put_schema = 13;
  }
  reserved 14 to 31; // GR12 future core atomic batch arms; separate exact-schema review to release
  reserved 100 to 199; // algorithms
  reserved 200 to 299; // temporal
  reserved 300 to 399; // held for memory; no current arm
  reserved 400 to 499; // TTL
}
message GraphWrite { uint64 id = 1; bool created = 2; uint64 revision = 3; }
message GraphDeleted { uint64 id = 1; }
message GraphAttached { uint64 node = 1; }
message GraphDetached { uint64 node = 1; uint64 record = 2; }
message GraphWriteOutcome {
  oneof kind {
    GraphWrite node = 1; GraphWrite edge = 2; GraphDeleted node_deleted = 3;
    GraphDeleted edge_deleted = 4; GraphAttached attached = 5; GraphDetached detached = 6;
    GraphUnit vector = 7; GraphUnit schema = 8;
  }
}

message GraphPivot {
  string collection = 1; VectorInput vector = 2; optional string text = 3; optional uint64 k = 4;
  repeated Condition filter = 5; Metric metric = 6; optional uint64 max_exact_scan_records = 7;
  optional uint64 candidates = 8; optional uint32 rrf_k = 9;
  optional double vector_weight = 10; optional double text_weight = 11;
}
message GraphExpansion {
  GraphDirection direction = 1; GraphStrings types = 2; GraphNodeFilter expand_filter = 3;
  GraphNodeFilter return_filter = 4; repeated Condition edge_filter = 5; bool include_paths = 6;
}
message GraphEvidenceSpec { repeated string collections = 1; GraphProjection fields = 2; optional uint64 max_excerpt_bytes = 3; optional uint64 max_items = 4; }
message GraphPivotHit {
  uint64 record_id = 1; uint64 node_id = 2; string node_key = 3; double score = 4;
  optional double distance = 5; optional double bm25 = 6; optional uint32 vector_rank = 7; optional uint32 text_rank = 8;
}
message GraphUnmappedPivots { uint64 count = 1; repeated uint64 record_ids = 2; }
message GraphEvidence {
  string collection = 1; uint64 record_id = 2; uint64 revision = 3; uint64 view_sequence = 4;
  uint64 node_id = 5; map<string, MetadataValue> metadata = 6; bytes payload_excerpt = 7;
  uint64 payload_bytes = 8; bool excerpt_truncated = 9;
}
message GraphRedacted { uint64 count = 1; repeated string collections = 2; }
message GraphRetrieveStats {
  uint64 pivot_ms = 1; uint64 expand_ms = 2; uint64 pivot_candidates = 3; uint64 pivots_mapped = 4;
  uint64 pivots_rejected = 5; uint64 evidence_items = 6; uint64 records_scanned = 7;
  uint64 distance_evaluations = 8; GraphReadStats expansion = 9;
}
message GraphRetrievalResult {
  string graph = 1; uint64 view_sequence = 2; bool complete = 3; repeated GraphTruncation truncation = 4;
  repeated GraphPivotHit pivots = 5; GraphUnmappedPivots unmapped_pivots = 6;
  repeated GraphTraversedNode nodes = 7; repeated GraphEdge edges = 8; repeated GraphPath paths = 9;
  repeated GraphEvidence evidence = 10; GraphRedacted redacted = 11; GraphRetrieveStats stats = 12;
}
message GraphGuardStats { uint64 count = 1; uint64 samples = 2; optional uint64 p50_us = 3; optional uint64 p99_us = 4; optional uint64 max_us = 5; }
message GraphRequestCounts { uint64 read = 1; uint64 write = 2; }

message ListGraphsRequest {}
message ListGraphsResponse { repeated GraphInfo graphs = 1; }
message CreateGraphRequest { string graph = 1; }
message CreateGraphResponse { GraphInfo graph = 1; }
message GetGraphRequest { string graph = 1; }
message GetGraphResponse { GraphInfo graph = 1; }
message DropGraphRequest { string graph = 1; }
message DropGraphResponse { string dropped = 1; }
message GetNodeRequest {
  string graph = 1; GraphGetNode query = 2; GraphLimits limits = 3; GraphProjection projection = 4;
  reserved 100 to 199; reserved 200 to 299; reserved 300 to 399; reserved 400 to 499;
}
message GetNodeResponse { GraphReadMeta meta = 1; GraphNode node = 2; }
message GetEdgeRequest {
  string graph = 1; GraphGetEdge query = 2; GraphLimits limits = 3; GraphProjection projection = 4;
  reserved 100 to 199; reserved 200 to 299; reserved 300 to 399; reserved 400 to 499;
}
message GetEdgeResponse { GraphReadMeta meta = 1; GraphEdge edge = 2; }
message FindNodesRequest {
  string graph = 1; GraphFindNodes query = 2; GraphLimits limits = 3; GraphProjection projection = 4;
  reserved 100 to 199; reserved 200 to 299; reserved 300 to 399; reserved 400 to 499;
}
message FindNodesResponse { GraphReadMeta meta = 1; repeated GraphNode nodes = 2; }
message NeighborsRequest {
  string graph = 1; GraphNeighbors query = 2; GraphLimits limits = 3; GraphProjection projection = 4;
  reserved 100 to 199; reserved 200 to 299; reserved 300 to 399; reserved 400 to 499;
}
message NeighborsResponse { GraphReadMeta meta = 1; repeated GraphNode nodes = 2; repeated GraphEdge edges = 3; }
message TraverseRequest {
  string graph = 1; GraphTraverse query = 2; GraphLimits limits = 3; GraphProjection projection = 4;
  reserved 100 to 199; reserved 200 to 299; reserved 300 to 399; reserved 400 to 499;
}
message TraverseResponse { GraphReadMeta meta = 1; repeated GraphTraversedNode nodes = 2; repeated GraphEdge edges = 3; repeated GraphPath paths = 4; }
message ShortestPathRequest {
  string graph = 1; GraphShortestPath query = 2; GraphLimits limits = 3; GraphProjection projection = 4;
  reserved 100 to 199; reserved 200 to 299; reserved 300 to 399; reserved 400 to 499;
}
message ShortestPathResponse { GraphReadMeta meta = 1; GraphPathResult shortest_path = 2; }
message ExplainRequest {
  string graph = 1; GraphReadQuery query = 2; GraphLimits limits = 3; GraphProjection projection = 4;
  reserved 100 to 199; reserved 200 to 299; reserved 300 to 399; reserved 400 to 499;
}
message ExplainResponse { string graph = 1; GraphExplanation explain = 2; }
message GetSchemaRequest { string graph = 1; optional uint64 budget = 2; }
message GetSchemaResponse { string graph = 1; GraphSchema declared = 2; GraphObservedSchema observed = 3; }
message PutSchemaRequest { string graph = 1; GraphSchema schema = 2; }
message PutSchemaResponse { string graph = 1; uint64 revision = 2; }
message RetrieveRequest {
  string graph = 1; GraphPivot pivot = 2; GraphExpansion expansion = 3;
  GraphEvidenceSpec evidence = 4; GraphLimits limits = 5; GraphProjection projection = 6;
  reserved 100 to 199; reserved 200 to 299; reserved 300 to 399; reserved 400 to 499;
}
message RetrieveResponse { GraphRetrievalResult result = 1; }
message QueryTextRequest {
  string graph = 1; string query = 2; map<string, MetadataValue> params = 3; GraphLimits limits = 4; bool explain = 5;
  reserved 100 to 199; reserved 200 to 299; reserved 300 to 399; reserved 400 to 499;
}
message QueryTextResponse { string graph = 1; oneof result { GraphReadResult read = 2; GraphExplanation explain = 3; } }
message WriteGraphBatchRequest { string graph = 1; repeated GraphWriteOperation ops = 2; }
message WriteGraphBatchResponse { string graph = 1; optional uint64 revision = 2; repeated GraphWriteOutcome outcomes = 3; }
message GetGraphStatsRequest {}
message GetGraphStatsResponse {
  GraphGuardStats read_guard = 1; GraphGuardStats write_guard = 2; GraphRequestCounts requests = 3;
  map<string, uint64> truncations = 4;
}
```

### Presence and field-table conventions

The tables are normative field tables for every new message, including empty
messages. `I` = implicit proto3 scalar; `O` = explicit `optional`; `M` =
message presence (optional on wire); `R` = repeated; `K` = map; `1` = oneof arm.
Application-required message fields are stated below: protobuf wire presence
does not itself enforce them. Each comma-separated field tuple in a table
cell is independent and aligned with its number/type/mirror in order.
Unqualified REST names mean the identically named field within that object.
`query` wrappers flatten into the REST body; `meta` flattens into the REST
read envelope; `result` wrappers flatten into the response. No JSON blob is
substituted for a typed graph object. The parent's flat Traverse sketches
(`docs/projects/roadmap/serving-contract.md:219-222`) are shorthand; these
nested wrappers are authoritative.

Bounds vocabulary, applying also when a table says "model":

| Symbol | Units, default, limit and evidence for the existing rule |
|---|---|
| G | Logical graph name; stored regex `^[a-z][a-z0-9_-]{0,62}$`, 63 bytes; 64 graphs/file (`src/property_graph/engine/limits.rs:10-14`). Tenant local names have at most 28 bytes after the 35-byte prefix for a 32-byte tenant ID (`src/server/credentials.rs:141-145`, `src/server/tenants.rs:91-103`); no physical-name escape. |
| Key | 1-256 UTF-8 bytes; label/type 1-63 ASCII `[A-Za-z0-9_]`; at most 8 node labels (`src/property_graph/engine/limits.rs:15-22`). |
| Props | At most 64 fields, 65,536 encoded property bytes; field names 1-256 UTF-8 bytes, string values at most 4,096 bytes; at most 64 filter conditions (`src/property_graph/engine/limits.rs:23-37`). Typed string/i64/f64/bool/geopoint, not arbitrary JSON (`src/property_graph/engine/model.rs:22-53`). |
| ID | Exact uint64, no JavaScript number conversion; revisions and sequences also uint64. REST output decimal strings (`src/server/graph.rs:4-8`). Counts remain counts, not IDs. |
| L | Eight read limits in table below; milliseconds only for deadline, counts otherwise. |
| S | Schema: 256 labels, 256 relationships, 64 properties/definition, 64 endpoint pairs/type, 1,024 description UTF-8 bytes, 32 indexed/unique label properties across schema (`src/property_graph/engine/limits.rs:44-60`). |
| B | At most 4,096 entries and 16,777,216 encoded engine-body bytes, **in addition** to smaller transport caps (`src/property_graph/engine/limits.rs:31-35`). No hidden batch splitting. |
| P | Projection defaults all; only at most 64 field names (`src/property_graph/read.rs:50-58`). |

| GraphLimits field | Tag/type/presence | REST mirror | Default / ceiling; zero |
|---|---|---|---|
| max_hops | 1 uint64 O | limits.max_hops | 2 / 4; valid |
| max_visited_nodes | 2 uint64 O | limits.max_visited_nodes | 1,000 / 10,000; invalid |
| max_edges_examined | 3 uint64 O | limits.max_edges_examined | 10,000 / 100,000; invalid |
| max_edges_per_node | 4 uint64 O | limits.max_edges_per_node | 1,000 / 10,000; invalid |
| deadline_ms | 5 uint64 O | limits.deadline_ms | 250 / 5,000 ms; invalid |
| max_output_nodes | 6 uint64 O | limits.max_output_nodes | 100 / 1,000; valid, except zero implicit page size |
| max_output_edges | 7 uint64 O | limits.max_output_edges | 200 / 2,000; valid, except zero implicit page size |
| max_output_paths | 8 uint64 O | limits.max_output_paths | 20 / 100; valid |

Defaults, ceilings and validation order are existing
`src/property_graph/engine/limits.rs:87-157`. Explicit page limits default to
the appropriate output cap, ceilings 1,000 nodes/2,000 edges, and **zero page
size is invalid**, even when inherited from a zero output cap
(`src/property_graph/engine/query.rs:511-527`,
`src/property_graph/engine/query.rs:686-692`,
`src/property_graph/engine/query.rs:739-745`).

### Shared query and read field tables

Imported messages retain these existing fields unchanged. Their declarations
are the proto citations at the start of section 3; geographic validation is
`src/metadata.rs:76-95`, `src/metadata.rs:250-307`. `Metric` retains existing values 0 unspecified,
1 squared-L2, 2 cosine, 3 inner-product.

| Imported message | Field | Number | Type / presence | REST mirror | Units / limits |
|---|---|---|---|---|---|
| GeoPoint | latitude; longitude | 1; 2 | double I; double I | value.latitude; value.longitude | Finite degrees [-90,90]; [-180,180] |
| MetadataValue | string_value; integer_value; float_value; boolean_value; geo_point | 1; 2; 3; 4; 5 | string 1; int64 1; double 1; bool 1; GeoPoint 1 | type/value tagged property | Props; exact i64; finite f64; bool; geographic degrees |
| Expected.Any; Expected.Absent | (none) | - | empty | "any"; "absent" | Markers |
| Expected | any; absent; revision | 1; 2; 3 | Expected.Any 1; Expected.Absent 1; uint64 1 | expected | Unset any; explicit absence; exact ID revision |
| Condition.Eq | field; value | 1; 2 | string I; MetadataValue M | field; value | Field name; required typed value |
| Condition.Range | field; min; max | 1; 2; 3 | string I; MetadataValue M; MetadataValue M | field; min; max | Field name; at least one numeric bound; same numeric type and min<=max when both present |
| Condition.GeoRadius | field; latitude; longitude; radius_meters | 1; 2; 3; 4 | string I; double I; double I; double I | field; latitude; longitude; radius_meters | Field name; geographic degrees; finite nonnegative metres |
| Condition | eq; range; geo_radius | 1; 2; 3 | Condition.Eq 1; Condition.Range 1; Condition.GeoRadius 1 | op:eq/range/georadius + fields | One known arm required; <=64 conditions |
| VectorInput | vector | 1 | float R | pivot.vector | Finite f32, exact collection dimensions |

| Message | Field | Number | Type / presence | REST mirror | Units / limits |
|---|---|---|---|---|---|
| GraphUnit | (none) | - | empty | marker arm | No fields |
| GraphStrings | values | 1 | string R | wrapped array | Element rules at use site |
| GraphProperties | values | 1 | MetadataValue K | metadata object | Props; wrapper distinguishes absent/empty |
| GraphSelector | id; key | 1; 2 | uint64 1; string 1 | id; key | ID; Key; exactly one required |
| GraphProjection | all; none; only | 1; 2; 3 | GraphUnit 1; GraphUnit 1; GraphStrings 1 | "all"; "none"; only | P; absent/unset = all; empty only valid |
| GraphNodeFilter | labels; conditions | 1; 2 | string R; Condition R | labels; conditions | All listed labels required; valid label names; Props conditions |
| GraphGetNode | node | 1 | GraphSelector M | node | Required selector |
| GraphGetEdge | edge | 1 | GraphSelector M | edge | Required selector |
| GraphFindNodes | label; conditions; limit; cursor | 1; 2; 3; 4 | string O; Condition R; uint64 O; string O | label; conditions; limit; cursor | Label; Props; page L; opaque lowercase hex |
| GraphNeighbors | node; direction; types | 1; 2; 3 | GraphSelector M; GraphDirection I; GraphStrings M | node; direction; types | Required; out default; absent = all, empty invalid |
| GraphNeighbors | edge_conditions; limit; cursor | 4; 5; 6 | Condition R; uint64 O; string O | edge_conditions; limit; cursor | Props; page L; opaque lowercase hex |
| GraphTraverse | start; direction; types | 1; 2; 3 | GraphSelector R; GraphDirection I; GraphStrings M | start; direction; types | 1..10,000 starts, normalize duplicates; out; as above |
| GraphTraverse | expand_filter; return_filter; edge_filter; include_paths | 4; 5; 6; 7 | GraphNodeFilter M; GraphNodeFilter M; Condition R; bool I | same names | Missing filters empty; Props; false default |
| GraphShortestPath | source; destination; direction; types | 1; 2; 3; 4 | GraphSelector M; GraphSelector M; GraphDirection I; GraphStrings M | same names | Both selectors required; out; as above |
| GraphReadQuery | get_node; get_edge; find_nodes; neighbors; traverse; shortest_path | 1; 2; 3; 4; 5; 6 | GraphGetNode 1; GraphGetEdge 1; GraphFindNodes 1; GraphNeighbors 1; GraphTraverse 1; GraphShortestPath 1 | operation + flattened body | Exactly one arm; REST operation strings use hyphens |
| GraphTruncation | kind; node_ids | 1; 2 | GraphTruncationKind I; uint64 R | reason string or degree_limit.node_ids | Known nonzero kind; only degree reason has IDs, at most 64 |
| GraphIndexChoice | kind; direction; typed | 1; 2; 3 | GraphIndexKind I; GraphDirection O; bool O | name or adjacency_range_scan.{direction,typed} | Known nonzero kind; latter two present iff adjacency |
| GraphReadStats | nodes_visited; nodes_scanned; edges_examined | 1; 2; 3 | uint64 I; uint64 I; uint64 I | same names | Work counts |
| GraphReadStats | nodes_returned; edges_returned; paths_returned | 4; 5; 6 | uint64 I; uint64 I; uint64 I | same names | Output counts, L |
| GraphReadStats | elapsed_ms; indexes | 7; 8 | uint64 I; GraphIndexChoice R | elapsed_ms; indexes | Milliseconds; ordered access paths |
| GraphReadMeta | graph; view_sequence; complete | 1; 2; 3 | string I; uint64 I; bool I | graph; view_sequence; complete | G display name; ID; true iff no reasons |
| GraphReadMeta | truncation; stats; cursor | 4; 5; 6 | GraphTruncation R; GraphReadStats M; string O | truncation; stats; cursor | Ordered unique reasons; required stats; cursor or null |
| GraphInfo | name; node_count; edge_count | 1; 2; 3 | string I; uint64 I; uint64 I | same names | G display name; counts |
| GraphNode | id; key; labels; properties; revision | 1; 2; 3; 4; 5 | uint64 I; string I; string R; MetadataValue K; uint64 I | same names | ID; Key; sorted labels; Props; ID |
| GraphEdge | id; key; relationship; source; source_key | 1; 2; 3; 4; 5 | uint64 I; string O; string I; uint64 I; string I | same names | ID; nullable Key; type; ID; Key |
| GraphEdge | target; target_key; properties; revision | 6; 7; 8; 9 | uint64 I; string I; MetadataValue K; uint64 I | same names | ID; Key; Props; ID; not destination on output |
| GraphPath | nodes; edges | 1; 2 | uint64 R; uint64 R | nodes; edges | IDs in path order, nodes = edges + 1 |
| GraphTraversedNode | depth; node | 1; 2 | uint64 I; GraphNode M | depth; node | Hops <= L.max_hops; required node |
| GraphNodeResult | node | 1 | GraphNode M | node | Absent = REST null, not an error |
| GraphEdgeResult | edge | 1 | GraphEdge M | edge | Absent = REST null, not an error |
| GraphNodesResult | nodes | 1 | GraphNode R | nodes | Ascending ID, L |
| GraphNeighborsResult | nodes; edges | 1; 2 | GraphNode R; GraphEdge R | nodes; edges | Adjacency order, L |
| GraphTraversalResult | nodes; edges; paths | 1; 2; 3 | GraphTraversedNode R; GraphEdge R; GraphPath R | nodes; edges; paths | Discovery/examination order, L |
| GraphPathResult | outcome; path; nodes; edges; truncation | 1; 2; 3; 4; 5 | GraphPathOutcome I; GraphPath M; GraphNode R; GraphEdge R; GraphTruncation R | same names | Found has path; exhausted has reasons; no-path has neither |
| GraphReadResult | meta | 1 | GraphReadMeta M | flattened envelope | Required |
| GraphReadResult | node; edge; nodes; neighbors; traversal; shortest_path | 2; 3; 4; 5; 6; 7 | GraphNodeResult 1; GraphEdgeResult 1; GraphNodesResult 1; GraphNeighborsResult 1; GraphTraversalResult 1; GraphPathResult 1 | corresponding typed output | Exactly one arm, matching parsed query |
| GraphExplanation | query; limits; projection; indexes | 1; 2; 3; 4 | GraphReadQuery M; GraphLimits M; GraphProjection M; GraphIndexChoice R | explain.{query,limits,projection,indexes} | Required normalized query/options; all 8 limit fields present |

Read inputs and ordering follow `src/property_graph/read.rs:78-169`;
traversal start ceiling is `src/property_graph/engine/query.rs:781-795`.
Read output and explanation shapes are
`src/bin/tool_support/graph_json.rs:938-1010`; reasons, indexes and statistics
are `src/property_graph/read.rs:269-345`. In particular missing direct lookups
are nullable outputs, whereas invalid referenced endpoints may be errors.
Node/edge expiry is deliberately not added: current public Rust views have it
(`src/property_graph/read.rs:194-224`), but REST encoders do not emit it
(`src/server/graph.rs:1419-1441`). A later temporal/TTL contract owns that gap.

### Schema field tables

| Message | Field | Number | Type / presence | REST mirror | Units / limits |
|---|---|---|---|---|---|
| GraphPropertyDefinition | type; required; unique; index | 1; 2; 3; 4 | GraphPropertyType I; bool I; bool I; GraphPropertyIndex O | type; required; unique; index | Nonzero type required; false defaults; absent index = none, zero present invalid |
| GraphLabelDefinition | description; properties | 1; 2 | string I; GraphPropertyDefinition K | description; properties | S; property names Props |
| GraphEndpoint | source; destination | 1; 2 | string I; string I | source; destination | Required label names |
| GraphRelationshipDefinition | description; endpoints; multiplicity; properties | 1; 2; 3; 4 | string I; GraphEndpoint R; GraphMultiplicity I; GraphPropertyDefinition K | same names | S; default multi; model schema validation |
| GraphSchema | labels; relationships | 1; 2 | GraphLabelDefinition K; GraphRelationshipDefinition K | labels; relationships | S; empty means remove declaration |
| GraphObservedProperty | name; origin; count; types | 1; 2; 3; 4 | string I; GraphSchemaOrigin I; uint64 I; GraphPropertyType R | same names | Props name; nonzero origin; occurrences; ordered observed types |
| GraphObservedLabel | name; origin; count; properties | 1; 2; 3; 4 | string I; GraphSchemaOrigin I; uint64 I; GraphObservedProperty R | same names | Label; nonzero origin; count; ordered summary |
| GraphObservedEndpoint | source; destination; origin; count | 1; 2; 3; 4 | string I; string I; GraphSchemaOrigin I; uint64 I | same names | Labels; nonzero origin; count |
| GraphObservedRelationship | name; origin; count; endpoints; properties | 1; 2; 3; 4; 5 | string I; GraphSchemaOrigin I; uint64 I; GraphObservedEndpoint R; GraphObservedProperty R | same names | Type; nonzero origin; count; ordered summaries |
| GraphObservedSchema | complete; entities_scanned; labels; relationships | 1; 2; 3; 4 | bool I; uint64 I; GraphObservedLabel R; GraphObservedRelationship R | observed.{same names} | Scan completeness, not asserted global absence; budget counts entities |

Exact declarations and observed encodings:
`src/bin/tool_support/graph_json.rs:1110-1235`,
`src/bin/tool_support/graph_json.rs:1260-1343`.
No extra schema constraints or inferred properties become declarations.

### Write field tables

| Message | Field | Number | Type / presence | REST mirror | Units / limits |
|---|---|---|---|---|---|
| GraphCreateNode | key; labels; properties | 1; 2; 3 | string I; string R; MetadataValue K | same names | Key; model; Props |
| GraphUpsertNode | key; labels; properties; expected | 1; 2; 3; 4 | string I; string R; MetadataValue K; Expected M | same names | Key; model; Props; absent/unset = any |
| GraphPatchNode | key; add_labels; remove_labels; set; unset; expected | 1; 2; 3; 4; 5; 6 | string I; string R; string R; MetadataValue K; string R; Expected M | same names | Key; model label sets; Props; field names; any default |
| GraphDeleteNode | key; expected | 1; 2 | string I; Expected M | same names | Key; any default; attached collections also authorized |
| GraphCreateEdge | key; source; relationship; destination; properties | 1; 2; 3; 4; 5 | string O; string I; string I; string I; MetadataValue K | same names | Nullable edge Key; node Keys; type; Props |
| GraphPatchEdge | edge; set; unset; expected | 1; 2; 3; 4 | GraphSelector M; MetadataValue K; string R; Expected M | same names | Required selector; Props; field names; any default |
| GraphDeleteEdge | edge; expected | 1; 2 | GraphSelector M; Expected M | same names | Required selector; any default |
| GraphAttach | node; collection; record | 1; 2; 3 | string I; string I; uint64 I | same names | Node Key; stored collection name; ID |
| GraphDetach | node; collection | 1; 2 | string I; string I | same names | Node Key; stored collection name |
| GraphPutVector | collection; id; vector; payload; metadata | 1; 2; 3; 4; 5 | string I; uint64 I; float R; string I; GraphProperties M | same names | Stored collection; ID; finite f32, collection dimensions 1..65,536; UTF-8 <=1,048,576 bytes; Props |
| GraphDeleteVector | collection; id | 1; 2 | string I; uint64 I | same names | Stored name; ID |
| GraphEnsureEdge | source; relationship; destination; properties | 1; 2; 3; 4 | string I; string I; string I; MetadataValue K | same names | Node Keys; type declared unique_triple; Props |
| GraphPutSchema | schema | 1 | GraphSchema M | schema | Required, empty schema allowed; S |
| GraphWriteOperation | create_node; upsert_node; patch_node; delete_node | 1; 2; 3; 4 | GraphCreateNode 1; GraphUpsertNode 1; GraphPatchNode 1; GraphDeleteNode 1 | op + matching fields | Exactly one operation arm |
| GraphWriteOperation | create_edge; patch_edge; delete_edge; attach; detach | 5; 6; 7; 8; 9 | GraphCreateEdge 1; GraphPatchEdge 1; GraphDeleteEdge 1; GraphAttach 1; GraphDetach 1 | op + matching fields | B |
| GraphWriteOperation | put_vector; delete_vector; ensure_edge; put_schema | 10; 11; 12; 13 | GraphPutVector 1; GraphDeleteVector 1; GraphEnsureEdge 1; GraphPutSchema 1 | op + matching fields | B |
| GraphWrite | id; created; revision | 1; 2; 3 | uint64 I; bool I; uint64 I | same names | ID; whether created; ID |
| GraphDeleted | id | 1 | uint64 I | id | ID |
| GraphAttached | node | 1 | uint64 I | node | ID |
| GraphDetached | node; record | 1; 2 | uint64 I; uint64 I | node; record | IDs |
| GraphWriteOutcome | node; edge; node_deleted; edge_deleted | 1; 2; 3; 4 | GraphWrite 1; GraphWrite 1; GraphDeleted 1; GraphDeleted 1 | same names | Exactly one outcome per operation in order |
| GraphWriteOutcome | attached; detached; vector; schema | 5; 6; 7; 8 | GraphAttached 1; GraphDetached 1; GraphUnit 1; GraphUnit 1 | attached; detached; "vector"; "schema" | Successful markers, not default successes |

The 13 operation forms, including ensure-edge and put-schema, follow actual
`OpIn` rather than the older 11-item OpenAPI op enumeration
(`src/server/graph.rs:911-1013`, `src/openapi_graph.json:2146-2161`).
`metadata` absent in put-vector means the existing `Mutation::Put` semantics;
present-empty means `Mutation::PutWithMetadata` with empty metadata
(`src/server/graph.rs:1095-1115`). Vector dimensions/payload bounds are
`src/lib.rs:93-98`. No new vector expected/expiry option is
invented. Outcomes and nullable batch revision follow
`src/bin/tool_support/graph_json.rs:1016-1057`.

### Retrieval and statistics field tables

| Message | Field | Number | Type / presence | REST mirror | Units / limits |
|---|---|---|---|---|---|
| GraphPivot | collection; vector; text; k | 1; 2; 3; 4 | string I; VectorInput M; string O; uint64 O | pivot.{same names} | Stored name; present vector differs from absent; text; default 20/ceiling 100 |
| GraphPivot | filter; metric; max_exact_scan_records | 5; 6; 7 | Condition R; Metric I; uint64 O | pivot.{same names} | Props; unspecified = squared_l2; default 100,000/ceiling 1,000,000 live records |
| GraphPivot | candidates; rrf_k; vector_weight; text_weight | 8; 9; 10; 11 | uint64 O; uint32 O; double O; double O | pivot.{same names} | max(k,50)/1,000, >=k; 60/1,000, >=1; each 1/10, finite >=0, not both 0 |
| GraphExpansion | direction; types; expand_filter; return_filter; edge_filter; include_paths | 1; 2; 3; 4; 5; 6 | GraphDirection I; GraphStrings M; GraphNodeFilter M; GraphNodeFilter M; Condition R; bool I | expansion.{same names} | Out default; absent types all, empty invalid; empty filters; Props; false |
| GraphEvidenceSpec | collections; fields; max_excerpt_bytes; max_items | 1; 2; 3; 4 | string R; GraphProjection M; uint64 O; uint64 O | evidence.{same names} | <=64 distinct stored collections; P; 512/4,096 bytes; 100/1,000 items; zeros valid |
| GraphPivotHit | record_id; node_id; node_key; score | 1; 2; 3; 4 | uint64 I; uint64 I; string I; double I | same names | IDs; Key; finite fused score |
| GraphPivotHit | distance; bm25; vector_rank; text_rank | 5; 6; 7; 8 | double O; double O; uint32 O; uint32 O | same names | Optional finite scores; 1-based ranks |
| GraphUnmappedPivots | count; record_ids | 1; 2 | uint64 I; uint64 R | unmapped_pivots.{same names} | Total count; first 64 IDs in pivot order |
| GraphEvidence | collection; record_id; revision; view_sequence; node_id | 1; 2; 3; 4; 5 | string I; uint64 I; uint64 I; uint64 I; uint64 I | same names | Stored collection name; IDs |
| GraphEvidence | metadata; payload_excerpt; payload_bytes; excerpt_truncated | 6; 7; 8; 9 | MetadataValue K; bytes I; uint64 I; bool I | metadata; payload_excerpt or payload_excerpt_base64; payload_bytes; excerpt_truncated | Props/P; raw engine excerpt <=4,096 bytes, partial trailing UTF-8 kept; REST drops it (`src/bin/tool_support/graph_json.rs:1511-1522`), so parity trims before comparing; original byte count; exact truncation flag |
| GraphRedacted | count; collections | 1; 2 | uint64 I; string R | redacted.{same names} | Removed item count; sorted distinct denied names, REST parity only |
| GraphRetrieveStats | pivot_ms; expand_ms; pivot_candidates; pivots_mapped; pivots_rejected | 1; 2; 3; 4; 5 | uint64 I; uint64 I; uint64 I; uint64 I; uint64 I | stats.{same names} | First two milliseconds; remainder counts |
| GraphRetrieveStats | evidence_items; records_scanned; distance_evaluations; expansion | 6; 7; 8; 9 | uint64 I; uint64 I; uint64 I; GraphReadStats M | stats.{same names} | Authorized returned evidence count; search work; required expansion stats |
| GraphRetrievalResult | graph; view_sequence; complete; truncation | 1; 2; 3; 4 | string I; uint64 I; bool I; GraphTruncation R | same names | Display G; ID; true iff no reasons; ordered reasons |
| GraphRetrievalResult | pivots; unmapped_pivots; nodes; edges; paths | 5; 6; 7; 8; 9 | GraphPivotHit R; GraphUnmappedPivots M; GraphTraversedNode R; GraphEdge R; GraphPath R | same names | Pivot score order; required unmapped summary; L |
| GraphRetrievalResult | evidence; redacted; stats | 10; 11; 12 | GraphEvidence R; GraphRedacted M; GraphRetrieveStats M | same names | Evidence bounds above; both summaries required, including zero redaction |
| GraphGuardStats | count; samples; p50_us; p99_us; max_us | 1; 2; 3; 4; 5 | uint64 I; uint64 I; uint64 O; uint64 O; uint64 O | same names | Total count; <=4,096 retained samples; microseconds, absent if none |
| GraphRequestCounts | read; write | 1; 2 | uint64 I; uint64 I | requests.{read,write} | Request counts |

Retrieval defaults and shapes are
`src/property_graph/retrieve.rs:43-140`,
`src/property_graph/retrieve.rs:154-234`,
`src/bin/tool_support/graph_json.rs:1358-1486`,
`src/bin/tool_support/graph_json.rs:1524-1595`.
Pivot text is <=1,024 UTF-8 bytes and 32 unique tokens; fusion bounds/defaults
are `src/hybrid.rs:11-15`, `src/hybrid.rs:34-51`,
`src/hybrid.rs:114-140`. Require vector or nonempty tokenized text, preserve
the current exact-only REST vector leg
(`src/bin/tool_support/graph_json.rs:1448-1456`).
Do not expose an ANN selector REST does not accept.
The engine permits pivot k=0 (no minimum check); the OpenAPI says minimum 1
(`src/property_graph/retrieve.rs:317-322`, `src/hybrid.rs:114-119`,
`src/openapi_graph.json:2697-2700`). Preserve runtime zero behavior pending
the explicit owner decision below, rather than silently tightening validation.
Guard metrics mirror `src/server/graph.rs:196-216`,
`src/server/graph.rs:262-269`.

### Per-method request/response field tables

The grouped rows explicitly define each named message; no alias changes its
descriptor. Empty requests have no fields. All `graph` input strings are
application-required G, including create; no default graph exists here.
All output graph names are unqualified display names for tenanted callers.

| Message(s) | Field | Number | Type / presence | REST mirror | Units / limits |
|---|---|---|---|---|---|
| ListGraphsRequest; GetGraphStatsRequest | (none) | - | empty | No body | No fields |
| ListGraphsResponse | graphs | 1 | GraphInfo R | graphs | Filtered, name-sorted; <=64 |
| CreateGraphRequest; GetGraphRequest; DropGraphRequest | graph | 1 | string I | body.graph or path.graph | G, required |
| CreateGraphResponse; GetGraphResponse | graph | 1 | GraphInfo M | graph | Required info |
| DropGraphResponse | dropped | 1 | string I | dropped | G display name |
| GetNodeRequest | graph; query; limits; projection | 1; 2; 3; 4 | string I; GraphGetNode M; GraphLimits M; GraphProjection M | path.graph; node; limits; projection | G; required query; L; P |
| GetEdgeRequest | graph; query; limits; projection | 1; 2; 3; 4 | string I; GraphGetEdge M; GraphLimits M; GraphProjection M | path.graph; edge; limits; projection | G; required query; L; P |
| FindNodesRequest | graph; query; limits; projection | 1; 2; 3; 4 | string I; GraphFindNodes M; GraphLimits M; GraphProjection M | path.graph; flattened body; limits; projection | G; absent query = empty find; L; P |
| NeighborsRequest | graph; query; limits; projection | 1; 2; 3; 4 | string I; GraphNeighbors M; GraphLimits M; GraphProjection M | path.graph; flattened body; limits; projection | G; required query; L; P |
| TraverseRequest | graph; query; limits; projection | 1; 2; 3; 4 | string I; GraphTraverse M; GraphLimits M; GraphProjection M | path.graph; flattened body; limits; projection | G; required query; L; P |
| ShortestPathRequest | graph; query; limits; projection | 1; 2; 3; 4 | string I; GraphShortestPath M; GraphLimits M; GraphProjection M | path.graph; flattened body; limits; projection | G; required query; L; P |
| ExplainRequest | graph; query; limits; projection | 1; 2; 3; 4 | string I; GraphReadQuery M; GraphLimits M; GraphProjection M | path.graph; operation+body; limits; projection | G; required query/arm; L; P |
| GetNodeResponse | meta; node | 1; 2 | GraphReadMeta M; GraphNode M | envelope; node | Required meta; absent node = null |
| GetEdgeResponse | meta; edge | 1; 2 | GraphReadMeta M; GraphEdge M | envelope; edge | Required meta; absent edge = null |
| FindNodesResponse | meta; nodes | 1; 2 | GraphReadMeta M; GraphNode R | envelope; nodes | Required meta; L |
| NeighborsResponse | meta; nodes; edges | 1; 2; 3 | GraphReadMeta M; GraphNode R; GraphEdge R | envelope; nodes; edges | Required meta; L |
| TraverseResponse | meta; nodes; edges; paths | 1; 2; 3; 4 | GraphReadMeta M; GraphTraversedNode R; GraphEdge R; GraphPath R | envelope; nodes; edges; paths | Required meta; L |
| ShortestPathResponse | meta; shortest_path | 1; 2 | GraphReadMeta M; GraphPathResult M | envelope; shortest_path | Both required; L |
| ExplainResponse | graph; explain | 1; 2 | string I; GraphExplanation M | graph; explain | G; required plan |
| GetSchemaRequest | graph; budget | 1; 2 | string I; uint64 O | path.graph; query.budget | G; 100,000/1,000,000 entities; zero invalid |
| GetSchemaResponse | graph; declared; observed | 1; 2; 3 | string I; GraphSchema M; GraphObservedSchema M | same names | G; both schemas required |
| PutSchemaRequest | graph; schema | 1; 2 | string I; GraphSchema M | path.graph; schema | G; required schema, empty allowed |
| PutSchemaResponse | graph; revision | 1; 2 | string I; uint64 I | graph; revision | G; ID |
| RetrieveRequest | graph; pivot; expansion; evidence; limits; projection | 1; 2; 3; 4; 5; 6 | string I; GraphPivot M; GraphExpansion M; GraphEvidenceSpec M; GraphLimits M; GraphProjection M | path.graph; pivot; expansion; evidence; limits; projection | G; required pivot; default expansion/evidence; L; P |
| RetrieveResponse | result | 1 | GraphRetrievalResult M | flattened retrieval | Required |
| QueryTextRequest | graph; query; params; limits; explain | 1; 2; 3; 4; 5 | string I; string I; MetadataValue K; GraphLimits M; bool I | path.graph; query; params; limits; explain | G; <=16,384 UTF-8 bytes/1,024 tokens; typed parameters; L; false |
| QueryTextResponse | graph; read; explain | 1; 2; 3 | string I; GraphReadResult 1; GraphExplanation 1 | graph; typed output; explain | G; exactly one matching explain request |
| WriteGraphBatchRequest | graph; ops | 1; 2 | string I; GraphWriteOperation R | path.graph; ops | G; B; every entry needs arm |
| WriteGraphBatchResponse | graph; revision; outcomes | 1; 2; 3 | string I; uint64 O; GraphWriteOutcome R | graph; revision; outcomes | G; absent = REST null for no revision; input-order outcomes |
| GetGraphStatsResponse | read_guard; write_guard; requests; truncations | 1; 2; 3; 4 | GraphGuardStats M; GraphGuardStats M; GraphRequestCounts M; uint64 K | same names | Required summaries; reason-name keys and counts |

Schema scan zero rejection is
`src/property_graph/engine/schema.rs:1130-1143`; language bounds and
missing/unused-parameter behavior are
`src/property_graph/engine/language.rs:13-29`.

### Enum, scalar and validation rules

Every enum spelling/value is fixed by the block. GraphDirection zero means
out, GraphMultiplicity zero means multi. Other new enums' zero values are
sentinels, not valid reported observations/outcomes. Reject unknown numeric
request enum values as `INVALID_ARGUMENT / invalid_input`; clients retain
unknown response numbers and report unsupported semantics, never reinterpret
them as zero. Absent optional index means no index; present UNSPECIFIED is
invalid. Existing Metric unspecified resolves to squared_l2 for graph pivots,
not the collection default.

Every MetadataValue needs one known kind; reuse validation for finite numbers,
geopoints and filters. An integer remains i64 rather than double; a float
remains f64 rather than f32. Vectors alone are f32. Reject missing required
selectors/messages/oneof arms as `invalid_input`. `Expected` absent or unset
means any; its explicit any/absent/revision (including revision zero) retains
presence. No missing outcome arm becomes a successful write.

Protobuf duplicate singular fields and duplicate map entries follow protobuf
last-value semantics; JSON duplicate-key rejection remains a JSON-parser
rule (`src/server/graph.rs:827-852`). The adapter validates the decoded value;
it must not claim wire-level duplicate rejection. Unknown proto fields are
ignored/preserved by generated runtimes, not treated as REST unknown JSON
fields. Unknown operation arms decode as unset and fail required-arm
validation before mutation.

## 4. Consistency, pagination, deadlines and size

One RPC reads one committed view under one database read guard; writes use
one write guard (`src/server/graph.rs:273-308`). Carry the captured
view_sequence, not a later head. Query normalization, output order and
shortest-path tie-breaking match the existing model
(`src/property_graph/read.rs:78-169`, `src/property_graph/read.rs:255-266`).

Only FindNodes and Neighbors, including equivalent QueryText queries, return
cursors. Keep the REST lowercase hex representation, not bytes/base64 or an
interpretable offset (`src/server/graph.rs:1309-1332`,
`src/bin/tool_support/graph_json.rs:723-725`,
`src/bin/tool_support/graph_json.rs:950-960`). Authorize the database/graph
on every call, then use the engine's graph-ID/sequence/query/projection/page
identity checks. Its cursor is checksummed, not a MAC, and has no lineage
field (`src/property_graph/engine/cursor.rs:1-16`); do not claim cryptographic
or cross-file cursor binding, or snapshot pinning across calls. A graph
change or attached base-record change
invalidates continuation; another query is invalid input
(`src/property_graph/read.rs:18-21`). A missing cursor is not permission to
restart silently. No streaming traversal or client-generated cursor.

**Proposed deadline resolution:** for bounded typed reads, validate the
requested graph deadline first, then use min(graph deadline, remaining client
deadline). Never round a positive sub-millisecond remainder to a zero-invalid
GraphLimits value; retain an absolute monotonic transport deadline separately.
Reasons cannot certify no-path: incomplete shortest path is exhausted.
Existing engine cancellation is checked before work and at least every 64
budget units (`src/property_graph/read.rs:61-69`,
`src/property_graph/engine/limits.rs:38-43`).

No `Graph` RPC exists yet, so the table separates what the unchanged tonic
transport and adapter do today from the code this contract proposes. tonic
0.14.6 (`Cargo.lock`) parses the client's `grpc-timeout`, uses the smaller of
it and any server timeout, and drops the handler when it elapses; the status is
CANCELLED ("Timeout expired") with no `vlite-error-code` (tonic
`src/transport/service/grpc_timeout.rs:41-93`,
`src/transport/server/mod.rs:1238`, `src/status.rs:644-645`); the server is built
with `tonic::transport::Server::builder()` (`src/server/grpc.rs:55`,
`src/server/grpc.rs:155-161`).
Standard clients also expire a call locally, so tests accept CANCELLED without
`vlite-error-code` or DEADLINE_EXCEEDED, never OK with a partial result.

| Condition | Observable today (unchanged tonic + adapter) | Needs new adapter code (proposed) |
|---|---|---|
| Graph budget expires, client waiting | No Graph RPC exists | OK, complete=false, deadline reason |
| Client `grpc-timeout` expires | CANCELLED, no `vlite-error-code` | DEADLINE_EXCEEDED + `deadline_exceeded` (proposed; wire conventions decision 4) only if an adapter timer set from the same header (kept in `request.metadata()`, tonic `src/request.rs:160-162`) fires before tonic's; never OK with a partial result |
| Client cancels or disconnects | Handler dropped, no status deliverable; the spawned pipeline keeps its permits (`src/server/grpc.rs:243`) | `graph_cancelled` is an audit/metrics label, never on the wire |
| Server cancel signal, client connected, before deadline | Not wired: the REST read path passes `cancel: None` (`src/server/graph.rs:455-459`) | OK, complete=false, cancelled reason |
| Request frame > 8 MiB | OUT_OF_RANGE, no `vlite-error-code` (tonic `src/codec/decode.rs:193-197`) | None: transport behavior |
| Response frame > 16 MiB | OUT_OF_RANGE, no `vlite-error-code` (tonic `src/codec/encode.rs:188-192`) | `encoded_len()` preflight, then RESOURCE_EXHAUSTED + `response_too_large` |

Typed reads and retrieve expansion can pass an adapter-owned `AtomicBool`
through the public `ReadOptions.cancel` while the adapter retains
admission/observation until the blocking work ends. The gRPC adapter already
retains permits through cancellation (`src/server/grpc.rs:220-306`), but the
REST read path it would reuse passes `cancel: None`
(`src/server/graph.rs:455-459`). Do not cancel or
replay a committed write. No new default timeout is imposed on lifecycle,
schema writes or batch writes; an explicit client deadline limits waiting, not
commit durability.

**Retrieve and schema caveat:** today the pivot phase accepts no deadline or
cancellation; only expansion/evidence is bounded
(`src/property_graph/retrieve.rs:12-18`, `src/property_graph/retrieve.rs:301`),
and `observed_schema` takes only an entity budget
(`src/property_graph/mod.rs:189`, `src/property_graph/engine/schema.rs:1130`).
The adapter must fail the RPC at the client deadline, retain permits until the
blocking call returns, and never report a pivot prefix as a complete pivot set;
that needs no engine change. Interrupting pivot or schema work itself needs a
new public parameter or method: an owner decision (section 10), with no new
`Error` variant. GetSchema retains its entity budget, not an invented 250-ms
default.

**Size decision:** keep 8 MiB request / 16 MiB response protobuf-message caps,
as currently configured (`src/server/grpc.rs:22-25`,
`src/server/grpc.rs:97-99`). The threshold is the encoded protobuf length,
excluding the five-byte gRPC message prefix; equality passes, cap+1 fails
(`len > limit` in the tonic paths above). No compression is enabled
(`Cargo.toml:54`; no `accept_compressed`/`send_compressed` under `src/`). Today
both caps fail natively as OUT_OF_RANGE with no `vlite-error-code`. To return
RESOURCE_EXHAUSTED with the proposed
`response_too_large` trailer instead, the adapter must compute each graph
response's `encoded_len()` before returning it and fail above 16,777,216 bytes;
that is new code and an owner decision (section 10). Either way the failure is
**not** truncation and not a smaller success. This preserves the documented
explicit-failure policy (`docs/GRPC.md:157-160`), avoids manufacturing a
resumable cursor for traversal, and does not lose evidence silently. Callers
may explicitly reduce page size, projection or evidence and retry reads. Do not
auto-retry a write whose response was lost/oversized.

The engine batch byte budget is additional, not permission for a 16-MiB
request. **Read-body admission (proposed; owner decision):** REST caps the
received JSON body of typed/explain/retrieve/query reads at 1 MiB
(`src/server/graph.rs:79-80`, `src/server/graph.rs:183-184`), a count of the
bytes the client sent, so two spellings of one query can differ at the cap.
gRPC has no JSON body, and a canonical-JSON unit would need a normative encoder
this document does not supply. Proposed unit: the re-encoded protobuf length
(`encoded_len()`) of the whole decoded read request, excluding the five-byte
prefix; at most 1,048,576, equality passes, cap+1 fails as
`INVALID_ARGUMENT / invalid_json` (the REST 413 code). This is separate from the
8-MiB decoder cap; do not clamp any field. This proposes option B of
[wire conventions decision 18](serving-wire-conventions.md#6-owner-decisions-23);
the unit is encoded protobuf bytes, not JSON bytes or a field count.

## 5. Per-RPC behavior, authorization and retries

In the matrix, prefix suffix routes with `/v1/graphs/{graph}`; `/` means that
base. Request/response names are literally `<RPC>Request`/`<RPC>Response` for
**each row**, including WriteGraphBatch and GetGraphStats.
`T` = qualify graph input, authorize stored binding before lookup, unqualify
graph output; never rewrite attached/pivot collection names.
`ListT` = filter bound tenant-owned graphs then unqualify. `A` = admin only,
no graph lookup. `L` refers to all eight default/ceiling values above.
`D` = graph 250/5,000 ms, client deadline wins; `C` = explicit client deadline
only, no new default. `R` = safe to retry read as a **new view**, no automatic
retry; `W` = do not blindly retry mutations.

| RPC | Request -> response | REST | Scope | Tenant | Defaults / ceilings | Deadline | Retry | MCP tool |
|---|---|---|---|---|---|---|---|---|
| ListGraphs | ListGraphsRequest -> ListGraphsResponse | GET `/v1/graphs` | graph_read | ListT | <=64 graphs | C | R | vlite_graph_list |
| CreateGraph | CreateGraphRequest -> CreateGraphResponse | POST `/v1/graphs` | graph_write | T | G, <=64 graphs | C | W | vlite_graph_create |
| GetGraph | GetGraphRequest -> GetGraphResponse | GET `/` | graph_read | T | G | C | R | No info tool; list |
| DropGraph | DropGraphRequest -> DropGraphResponse | DELETE `/` | graph_write | T | Empty graph only | C | W | vlite_graph_drop |
| GetNode | GetNodeRequest -> GetNodeResponse | POST `/nodes/get` | graph_read | T | L, P, Key | D | R | vlite_graph_get(node) |
| GetEdge | GetEdgeRequest -> GetEdgeResponse | POST `/edges/get` | graph_read | T | L, P, Key | D | R | vlite_graph_get(edge) |
| FindNodes | FindNodesRequest -> FindNodesResponse | POST `/nodes/find` | graph_read | T | L; page 100/1,000 | D | R | vlite_graph_find_nodes |
| Neighbors | NeighborsRequest -> NeighborsResponse | POST `/neighbors` | graph_read | T | L; page 200/2,000 | D | R | vlite_graph_neighbors |
| Traverse | TraverseRequest -> TraverseResponse | POST `/traverse` | graph_read | T | L; starts 1..10,000 | D | R | vlite_graph_traverse |
| ShortestPath | ShortestPathRequest -> ShortestPathResponse | POST `/shortest-path` | graph_read | T | L | D | R | vlite_graph_shortest_path |
| Explain | ExplainRequest -> ExplainResponse | POST `/explain` | graph_read | T | L validated, not executed | C | R | Matching typed tool, explain:true |
| GetSchema | GetSchemaRequest -> GetSchemaResponse | GET `/schema` | graph_read | T | budget 100,000/1,000,000 | C | R | vlite_graph_schema |
| PutSchema | PutSchemaRequest -> PutSchemaResponse | PUT `/schema` | graph_write | T | S, existing data validated | C | W | vlite_graph_put_schema |
| Retrieve | RetrieveRequest -> RetrieveResponse | POST `/retrieve` | graph_read + pivot read | T | L; pivot 20/100; evidence 100/1,000, 512/4,096 bytes | D expansion; C total | R | vlite_graph_retrieve |
| QueryText | QueryTextRequest -> QueryTextResponse | POST `/query` | graph_read | T | L + existing language bounds | D read; C explain | R | vlite_graph_query |
| WriteGraphBatch | WriteGraphBatchRequest -> WriteGraphBatchResponse | POST `/batch` | graph_write + affected collection write | T | B; transport 8 MiB | C | W | vlite_graph_write |
| GetGraphStats | GetGraphStatsRequest -> GetGraphStatsResponse | GET `/v1/admin/graph/stats` | admin only | A | 4,096 samples/guard | C | R | None |

Route registration/scopes are `src/server/graph.rs:123-184`; MCP mapping is
`src/bin/tool_support/graph_mcp.rs:34-53`,
`src/bin/tool_support/graph_mcp.rs:119-136`. Stats is admin by default route
policy (`src/server.rs:926-932`). Missing graph scope always denies, including
`collections:["*"]`; read-only credentials can read but cannot mutate
(`src/server/graph.rs:15-34`). Lists are filtered
(`src/server/graph.rs:372-388`); qualification/binding happens before lookup
(`src/server/graph.rs:317-325`, `src/server/graph.rs:1710-1713`).
List order is name-sorted (`src/property_graph/mod.rs:686-694`).

Use the same admission -> authenticate -> rate limit -> scope -> core ->
observation pipeline as existing gRPC (`src/server/grpc.rs:220-306`), with
graph-specific Need policy added explicitly, not the legacy collection policy.
Suspended/deleted/revoked/expired credentials fail exactly as REST; tenant
admission already checks active state (`src/server/tenants.rs:132-150`).
Never accept another tenant's physical graph name as a bypass; qualify it as
local or reject it under the same name rules, never strip a caller prefix.

Retrieve requires pivot `read` plus stored collection coverage under the
read guard; denied evidence items are removed, sorted distinct collection
names/counts reported in redacted, and evidence_items counts only returned
items (`src/server/graph.rs:572-646`,
`src/bin/tool_support/graph_json.rs:1582-1594`). Redaction alone does not mean
incomplete traversal. Return no vectors or denied evidence metadata/payload,
and disclose no more denied collection information than REST already reports.

Under the write guard, attach/detach/vector mutations need `write` and all
affected collection bindings; deleting an attached node checks every attached
collection too. Enforce existing live-vector quotas before one atomic batch,
never partially apply or authorize against an earlier snapshot
(`src/server/graph.rs:713-792`). No graph scope follows from collection scope.

Reuse shared route templates (not user graph names) and method=`grpc` for
metrics, request IDs and denial audit; preserve existing credential-denied
events and quota/rate counters (`src/server.rs:1206-1278`). Record graph guard
times/truncation reasons through the same accounting
(`src/server/graph.rs:225-308`). No unbounded graph/tenant/collection labels,
tokens, query text, property values or evidence in logs/traces. Attribute
only through existing sanitized principal/request identifiers. Local MCP
parity fixtures compare engine semantics in its writable/read-only context;
authenticated remote use must apply the server policy, not inherit local
MCP's lack of a bearer boundary.

## 6. Exact error and partial-result contract

Every application failure sets `vlite-error-code` to the exact code below;
preserve the sanitized REST message and x-request-id. Current mapper/trailer
implementation: `src/server/grpc.rs:181-215`. All following existing graph
codes come from `src/server.rs:1594-1656`, not guessed Rust variant spellings.
Each comma-separated code below is a separate exact status/trailer pair.

| Stable trailer value(s) | gRPC status | Meaning |
|---|---|---|
| graphs_not_enabled | FAILED_PRECONDITION | File lacks graph support |
| graph_exists | FAILED_PRECONDITION | Graph already exists; **not** ALREADY_EXISTS |
| graph_not_found | FAILED_PRECONDITION | Authorized graph missing; **not** NOT_FOUND |
| graph_not_empty | FAILED_PRECONDITION | Drop requires empty graph |
| too_many_graphs | FAILED_PRECONDITION | File graph count cap |
| graph_key_exists, graph_key_conflict | FAILED_PRECONDITION | Key/state conflict |
| graph_node_not_found, graph_edge_not_found | FAILED_PRECONDITION | Required referenced entity missing |
| graph_node_has_edges | FAILED_PRECONDITION | Deletion blocked by relationships |
| graph_attachment_conflict, graph_attachment_not_found | FAILED_PRECONDITION | Attachment state conflict |
| graph_record_not_attachable | FAILED_PRECONDITION | Invalid record attachment state |
| graph_attached_record, graph_attached_record_expiry | FAILED_PRECONDITION | Attached record mutation/expiry conflict |
| graph_cursor_stale | FAILED_PRECONDITION | Mutation invalidated cursor |
| graph_schema_violation | FAILED_PRECONDITION | Declared constraint violated |
| invalid_graph_name | INVALID_ARGUMENT | Graph name invalid |
| graph_limit_exceeds_ceiling | INVALID_ARGUMENT | Any graph budget above its ceiling |
| graph_invalid_options | INVALID_ARGUMENT | Invalid graph options/query structure |
| graph_pivot_too_expensive | INVALID_ARGUMENT | Exact pivot exceeds live-record budget |
| graph_query_syntax, graph_query_unsupported, graph_query_parameter, graph_query_limit | INVALID_ARGUMENT | Existing query parser error family |
| invalid_input | INVALID_ARGUMENT | Semantic validation (REST 422): invalid vector/property/filter/selector, unset required arm, unknown request enum, malformed cursor |
| invalid_json | INVALID_ARGUMENT | Malformed JSON or over-cap read body (REST 400/413); gRPC emits it only for the proposed read-body cap (section 4) |
| precondition_failed | ABORTED | Expected revision mismatch, not graph_schema_violation |
| unauthorized, credential_expired | UNAUTHENTICATED | Missing/invalid bearer or expired credential |
| forbidden_scope, read_only | PERMISSION_DENIED | Scope/binding/admin/write denial |
| tenant_suspended, tenant_unavailable | PERMISSION_DENIED | Suspended or deleted/unavailable tenant |
| collection_not_found | NOT_FOUND | Required collection missing |
| rate_limited, quota_exceeded | RESOURCE_EXHAUSTED | Existing credential/tenant bounds; retry-after:1 |
| busy, unavailable | UNAVAILABLE | Admission saturation (503 `busy`, `src/server/grpc.rs:249-254`) or unavailable |
| commit_indeterminate | UNKNOWN | Write might already be durable; inspect, never blind retry |
| poisoned | INTERNAL | Existing generic mapping of this 503; no silent mapper change |
| internal_error | INTERNAL | Unexpected sanitized internal failure |
| deadline_exceeded (**proposed**) | DEADLINE_EXCEEDED | Handler-built `ApiError` from the engine/adapter stop reason, before the commit point with no partial allowed; wire conventions decision 4 |
| response_too_large (**proposed**) | RESOURCE_EXHAUSTED | Adapter-built `ApiError` from the response `encoded_len()` preflight above 16,777,216 bytes; wire conventions section 2 |
| (none: tonic transport) | OUT_OF_RANGE | Request frame above 8 MiB; response frame above 16 MiB when no preflight runs |
| (none: tonic transport) | CANCELLED | `grpc-timeout` expiry enforced by tonic |
| (none: tonic transport) | INTERNAL | Undecodable protobuf (tonic-prost `src/codec.rs:143-146`) |

Query-text names are verified in
`src/bin/tool_support/graph_json.rs:1613-1626`; expected revision mapping is
`src/server.rs:1523-1535`; poisoned/indeterminate are
`src/server.rs:1577-1593`. Expired credentials and suspended/unavailable
tenants are `src/server/credentials.rs:468-473`,
`src/server/tenants.rs:143-148`, `src/server/tenants.rs:170-175`;
rate/quota codes are `src/server.rs:3497-3503`, `src/server.rs:3575-3581`.
Generic engine/vector errors continue through
ApiError and the unchanged mapper, not an invented blanket graph status.
An invalid cursor encoding is invalid_input at the REST cursor parser
(`src/server/graph.rs:1308-1329`); a well-encoded but invalid/mismatched
engine cursor maps through InvalidQuery to `INVALID_ARGUMENT / invalid_input`
(`src/property_graph/convert.rs:173-177`, `src/server.rs:1528-1535`),
not graph_cursor_stale.

Transport failures carry no `vlite-error-code`: undecodable protobuf and the
8-MiB decoder cap fail before the adapter runs, so they also lack x-request-id
(tonic `src/server/grpc.rs:236-243`); `grpc-timeout` expiry and a disconnected
client get no adapter response; a 16-MiB encoder failure keeps the adapter's
x-request-id header (`src/server/grpc.rs:297-302`, tonic
`src/codec/encode.rs:321-326`). Clients must tolerate absent values and must
not invent them. The two new codes above are built as `ApiError` by the
handler/adapter; the proposed read-cap `invalid_json` also comes from an
adapter-built `ApiError`, not a new engine variant. Explicit shared mapper arms
for `response_too_large` and `deadline_exceeded` would land in Gap 12, if wire
conventions decisions 3 and 4 are approved, per
[wire conventions sections 2 and 5.2](serving-wire-conventions.md#2-error-registry-c2-c9);
no public `vlite::Error` variant is added. `graph_cancelled` remains an
audit/metrics label only, with no wire code or status/trailer pair.
A disconnected client cannot receive an adapter response. REST also reports JSON
shape errors as 422 `invalid_json` (`src/server/graph.rs:346-365`); protobuf
has no JSON shape, so equivalent adapter-detected cases are `invalid_input`.
Do not add retry-after:1 to the new size error: it is not transient quota
pressure. The current status helper adds it for HTTP 429
(`src/server/grpc.rs:202-204`); add the explicit response-size mapper arm without
changing existing mappings. The proposed response-size code is gRPC-only;
REST retains no response cap per wire conventions decision 3.

OK partial reads retain data and complete=false with distinct ordered reasons:
visited_limit, edge_limit, degree_limit{node_ids}, output_limit, deadline,
cancelled. Do not raise RPC errors for these ordinary read-budget outcomes.
`complete` is true exactly when reasons are empty; degree IDs at most 64;
shortest_path exhausted repeats the same reasons. Hop bound is a query
parameter, not a truncation. No-path-within-hops is allowed only after a
complete search (`src/property_graph/read.rs:255-289`,
`src/property_graph/engine/limits.rs:72-73`). GetSchema's observed.complete
is its bounded-scan result, not a GraphReadMeta cursor or an error. Response
size failure is always an error, never a new truncation reason.

## 7. Client and reference impact

Keep the dependency-free REST packages REST-only by default; do not silently
add gRPC runtimes. Supply optional generated-stub usage for Python,
JavaScript (a runtime preserving uint64 via bigint/Long/string), and .NET
(ulong). Python has a verified generation path in tests, not a packaged
Graph client; JavaScript and .NET have no packaged gRPC transport in the
implementations cited in section 2. Embedded Python is a separate surface,
not proof that the HTTP package supports gRPC.

New client -> old server: surface UNIMPLEMENTED for vlite.v1.Graph/<method>;
never retry via REST/MCP automatically. Old client -> new server: unchanged
VLite descriptors and old feed arms still work. New optional fields stay
absent when absent; absent node/edge is not an empty synthesized entity.
Expose status, application trailer, request ID, incomplete metadata and
redaction without coercing uint64 or dropping reasons.

Implementation documentation/reference ownership, **not changes in this draft**:
`docs/GRPC.md` (service inventory, generation commands, graph grants, bounds,
errors, partial reads, size/deadline policy); `clients/README.md`,
`clients/python/README.md`, `clients/javascript/README.md` (optional generated
stub notes and no fallback); .NET generated-client notes in
`clients/README.md` beside `clients/dotnet/VLite.Client/VLite.Client.csproj`.
Add a `data-api="grpc:<RPC>"` row per method under `site/api.html`'s gRPC section with
scope and request/response names when the proto ships. The current scan is
name-based, not service-qualified (`tests/api_reference.py:445-446`); these
names do not collide with existing VLite methods
(`proto/vlite/v1/vlite.proto:16-60`). Proposed option A of
[wire conventions decision 16](serving-wire-conventions.md#6-owner-decisions-23)
keeps the scanner and `grpc:<ShortName>` keys unchanged and requires a
descriptor-level RPC short-name uniqueness test across every `vlite.v1` service.
Later slices add named services and RPCs to that test.
Update graph/security references `docs/GRAPH.md` and `docs/SECURITY.md` with
transport parity only; correct directly related OpenAPI descriptive omissions
only in an approved later slice, never by adding REST routes here.

## 8. Required acceptance and mutation matrix

All tests below are **delivery requirements**, not tests claimed to exist or
pass today. Existing build/smoke entry points are `build.rs:10-18`,
`tests/grpc_smoke.py:1-10`, `tests/grpc_smoke.py:51-83`.

| Fixture / check | Required assertions |
|---|---|
| Shared REST/gRPC/MCP golden graph | Every method vs REST; all mapped MCP tools; local MCP read-only/write rules tested separately. Nodes/edges/properties, deterministic order, self-loops, duplicate starts, paths, null lookups, declared/observed schema, retrieve pivots/evidence and every batch outcome. Ignore only timing counters when comparing deterministic data. |
| Field-by-field round trips | Exercise every field/tag/enum/oneof arm in the manifest, empty vs absent wrappers/maps, Expected any/absent/revision, IDs 0/2^53+1/u64::MAX, i64 min/max, f64 vs f32, UTF-8, geopoints, arbitrary binary excerpt, nullable edge key, revision null, cursors and redaction. Nonfinite numeric negatives. |
| Independent Python oracle | Generate Python descriptors/stubs from the proposed manifest and unchanged proto outside the source tree. Separate hand-written fixture/value and protobuf wire expectations from the Rust adapter; encode Python -> decode Rust and encode Rust -> decode Python. Assert literal tags, wire types, values, HasField/WhichOneof, not just self-round-trip. Include all 13 operation arms, six read arms, eight outcomes and optional zero values. |
| Descriptor stability | Compare FileDescriptorSets normalized only for source locations/order noise. Preserve existing fields, types, labels, oneofs, signatures, enum meanings and streams; accept only named additive entries per wire conventions sections 1.2-1.3 and 5.3, including TTL GraphChangeOperation tags 19-22 and temporal tag 23 / enum value 10 in their landing slices. Snapshot every new symbol/tag, reserved interval and enum number; permit only the band's first tag to be carved from a shared-message fence for a named addition; reject deletion, reuse or reassignment. List every service and enforce RPC short-name uniqueness. |
| Auth negatives | Missing/expired/revoked key, wrong lineage, missing graph_read/write, collection wildcard without graph scopes, unbound existing and nonexistent graph (same denial), read-only mutation, non-admin stats, viewer/editor/admin. |
| Tenant negatives | Logical vs physical names; cross-tenant graph/pivot/evidence/attached collection; filtered list; suspend/delete tenant; revocation while queued. No existence probe, raw-name label or denied evidence payload. |
| Atomic writes | Each attach/detach/vector mutation and attached-node deletion denied without affected collection write; multi-collection quota rejection rolls back everything; Expected conflict returns ABORTED; schema conflict FAILED_PRECONDITION; no partial outcomes on failure. |
| Every default and boundary | Every L field omitted/default/ceiling/ceiling+1/0/u64::MAX; node and edge page limits including implicit zero pages; G/Key/labels/type/Props/S/B boundaries, UTF-8 byte rather than character lengths. Schema budget, pivot k/exact-scan, fusion candidates/rrf/weights, evidence names/items/excerpts and query-language bounds at default/ceiling/+1/zero where meaningful. Record semantic rejection vs supported zero. |
| Consistency/pages | Same view across pivot/attachments/expansion/evidence; graph or attached-record mutation invalidates cursor; wrong query/page/projection/graph cursor rejected, wrong credential lineage denied; corruption/uppercase/odd hex; no automatic restart. Exhausted != no-path; found path tie fixture. No unsupported cross-file cursor guarantee. |
| Deadline/cancel | Before admission, while queued, before work, mid-read at a check, during pivot/schema scan, and after write starts. Graph budget partial vs transport deadline error; smaller graph/client deadline; sub-ms remainder; connected server-cancel partial (if a signal is wired); preserve permits/audit until work ends. Client expiry asserts CANCELLED without `vlite-error-code` or DEADLINE_EXCEEDED, never OK or a partial result; pivot/schema scans never return a prefix. |
| Size caps | Every service sets both caps and has an oversize-request test. Request frame 8 MiB-1/equal/+1 (+1: OUT_OF_RANGE, no `vlite-error-code`); whole decoded read request `encoded_len()` 1 MiB-1/equal/+1 (+1: INVALID_ARGUMENT / invalid_json per proposed decision 18); response 16 MiB-1/equal/+1 across each response family, binary/UTF-8/map overhead (+1: RESOURCE_EXHAUSTED / response_too_large with preflight, never a truncated success). Assert no cursor advance or invention, smaller explicit read request succeeds, failed write response never triggers automatic replay. |
| Version skew | Old generated VLite/feed clients against new server; new Graph stubs against old server -> UNIMPLEMENTED, no fallback. Unknown fields tolerated, unknown request enum/operation rejected, unknown response enum surfaced. Feature-off builds and health registration remain coherent. |
| References/docs | release_docs.py, content_budget.py, agent_docs.py, api_reference.py, relative-link resolution and diff whitespace. Proto/Graph reference inventory agrees exactly; no claim of shipped support before delivery. |

### Independent Python oracle seed

This is a proposed executable oracle seed, not a claim that generated Graph
bindings exist today. Generate `graph_contract_pb2` from section 3 and the
unchanged imported proto in a temporary directory outside the checkout.
These literal wire bytes are independent of the Rust encoder and catch
accidentally self-consistent field/tag changes. Extend this seed to the full
field/arm matrix above, and feed the same bytes through Rust decoding.

```python
import graph_contract_pb2 as g
from vlite.v1 import vlite_pb2 as v

fixtures = [
    (g.GraphSelector(id=2**64 - 1), "08ffffffffffffffffff01"),
    (g.GraphSelector(key="a"), "120161"),
    (g.GraphLimits(max_hops=0), "0800"),
    (g.GraphProperties(values={"n": v.MetadataValue(integer_value=42)}),
     "0a070a016e1202102a"),
    (g.GraphUpsertNode(key="a", expected=v.Expected(revision=0)),
     "0a016122021800"),
    (g.GraphFindNodes(cursor="00ab"), "220430306162"),
    (g.GraphReadMeta(
        graph="g", view_sequence=42, complete=False,
        truncation=[g.GraphTruncation(kind=g.GRAPH_TRUNCATION_DEADLINE)],
        stats=g.GraphReadStats()), "0a0167102a220208052a00"),
]
for message, golden_hex in fixtures:
    wire = bytes.fromhex(golden_hex)
    assert message.SerializeToString(deterministic=True) == wire
    decoded = type(message).FromString(wire)
    assert decoded == message
assert fixtures[0][0].WhichOneof("kind") == "id"
assert fixtures[2][0].HasField("max_hops")
assert not fixtures[2][0].HasField("max_visited_nodes")
assert fixtures[3][0].values["n"].WhichOneof("kind") == "integer_value"
assert fixtures[4][0].expected.WhichOneof("kind") == "revision"
assert fixtures[5][0].cursor == "00ab"
assert not fixtures[6][0].complete and fixtures[6][0].truncation
```

Mutation list: swap tags; narrow uint64; turn optional zero into absence; merge
float/integer kinds; omit one operation/outcome; erase metadata-wrapper
presence; clamp ceiling+1; accept zero page; certify exhausted as no-path;
omit view_sequence; reuse cursor after commit; decode a cursor as an offset;
skip projection in cursor identity; authorize after lookup; let collection
wildcard grant graph access; requalify stored collections; skip attached-node
delete coverage; leak denied evidence or unredacted evidence_items; expose
stats to graph_read; ignore pivot deadline; free permit on disconnect; return
oversized success; auto-retry a write; silently fall back on UNIMPLEMENTED.
Each mutation must fail a named assertion above, not merely change coverage.

## 9. Delivery slices and independent acceptance

Sequencing follows the parent (`docs/projects/roadmap/serving-contract.md:253-255`):
schemas are approved first, then Gap 8's provider registry and its REST/gRPC
wiring, then graph unary parity. gap12-proto starts after Gap 8's proto/gRPC
wiring is accepted, because both edit `vlite.proto` and `grpc.rs`; later slices
depend on gap12 slices as the table lists. No slice here implements providers.
Shared files forcing serialization with
other serving slices: **proto/vlite/v1/vlite.proto, src/server/grpc.rs,
src/server/graph.rs, docs/GRPC.md**. Land one owner at a time; rebasing must
rerun descriptor/reference checks, not just resolve textual conflicts.
New test/helper filenames below are proposed, not existing-code claims.

| Slice | Exclusive files owned when active | Dependencies and required tests | Independent acceptance checklist |
|---|---|---|---|
| gap12-proto | proto/vlite/v1/vlite.proto; proposed tests/graph_grpc_schema.py and descriptor/golden fixtures | Gap 8 proto/gRPC wiring accepted + exact-schema review; compile via existing build.rs path; independent Python oracle and descriptor stability | 17 unary Graph methods at landing; all field tables match descriptors; old descriptors/feeds unchanged at this slice; sibling-owned fences under approved wire decision 1 and the named core hold under decision 20B; named additions and unique RPC short names enforced; no adapter needed to verify schema |
| gap12-server | src/server/grpc.rs; src/server/graph.rs; src/property_graph/read.rs, retrieve.rs and engine read/schema execution only if section 10 approves a public cancellation API; proposed tests/graph_grpc.py | gap12-proto; all parity/auth/tenant/atomicity/limits/deadline/cap checks; cargo test with graph+grpc and feature-off builds | Shared admission/auth/audit/metrics; Graph registration, health and both caps per section 3; explicit `response_too_large` and `deadline_exceeded` mapper arms only if wire conventions decisions 3 and 4 are approved; no HTTP loopback; exact errors; complete metadata; pivot/schema deadline handled as section 10 decides, never a pivot prefix; no new policy/default; no source name guessed from this draft |
| gap12-clients | clients/README.md; clients/python/README.md; clients/javascript/README.md; optional stub examples/notes and tests under clients/python, clients/javascript, clients/dotnet | gap12-proto; runnable endpoints from gap12-server for live checks; Python oracle, JS full-width ID checks, .NET optional generation and skew tests | REST dependency defaults unchanged; explicit gRPC selection; an example/type per method; status/trailers/absence preserved; no silent fallback/retry |
| gap12-parity-docs | docs/GRPC.md; site/api.html; docs/GRAPH.md; docs/SECURITY.md; final parity fixtures in tests/graph_grpc.py coordinated after server ownership ends | All prior slices; full acceptance matrix, four documentation checks plus api_reference.py and links | Every method documented once; shared REST/MCP fixtures pass; mutation assertions proven; generated-client instructions reproducible; no claim of release/qualification based only on docs |

## 10. Owner decisions and remaining open items

Recommendations below are exact proposed resolutions, not blanket approvals or
changes to existing gates. The shared sibling-band and core-reservation choices
are already settled by wire decisions 1 and 20; they do not approve this exact
manifest or lift its descriptor/oracle review prerequisite.

| Decision | Options | Recommendation / acceptance consequence |
|---|---|---|
| Exact wire manifest | Approve these typed wrappers/tags; or revise names/presence before code | Approve only after descriptor/oracle review, especially GraphProperties absent/empty, schema observations, 13 write arms and eight outcomes. Proto remains frozen until then. |
| Deadline partial vs transport error | Always return partial; always fail on any budget; or distinguish graph budget from transport deadline. For client expiry: accept tonic's CANCELLED (today) or add a header-derived adapter timer | Graph-budget partial while transport permits. For client expiry accept tonic's CANCELLED with no `vlite-error-code` (no new code); add the DEADLINE_EXCEEDED timer and `deadline_exceeded` only if an owner needs that trailer. `graph_cancelled` stays an audit/metrics label. Do not change engine defaults. Evidence: tonic `src/transport/service/grpc_timeout.rs:41-93`; section 4 table. Wire conventions, decision 4 (proposed): explicit 504 deadline_exceeded for an engine/adapter stop before commit with no partial allowed; tonic expiry is unchanged. |
| 16 MiB response | Truncate and mark; `encoded_len()` preflight error (RESOURCE_EXHAUSTED + `response_too_large`); or accept tonic's OUT_OF_RANGE (today) | Explicit error, never truncation. Prefer the preflight (new adapter code, new trailer name); OUT_OF_RANGE is the zero-code fallback. No cap increase. Evidence: tonic `src/codec/encode.rs:188-192`, `docs/GRPC.md:157-160`. Wire conventions, decision 3 (proposed): REST retains no response cap; the preflight code is gRPC-only. |
| Read-body admission accounting | Transport 8 MiB only (today); 1,048,576 re-encoded protobuf bytes; or 1 MiB of a canonical REST JSON encoding | Protobuf unit (section 4): deterministic, one counter, no JSON encoder to specify. REST counts the JSON bytes received (`src/server/graph.rs:79-80`), so exact REST equivalence is not claimed. Add boundary fixtures. Wire conventions, decision 18 (proposed): option B, encoded_len() preflight with invalid_json. |
| Pivot k=0 documentation mismatch | Tighten runtime to minimum 1; or document current zero behavior | Preserve zero runtime behavior here. Correct OpenAPI in its separately owned approved slice, or explicitly approve a future compatibility change; do not silently invent a minimum. |
| Sibling tag allocation; `reserved 14 to 31;` | Shared choices already resolved; exact operation schemas remain separately reviewed | **Owner-approved 2026-11-02:** wire decision 1 option A, sibling-owned bands for atomic mixed writes; decision 20 option B, KEEP tags 14-31 for GR12 future core atomic graph-batch arms. Only a separate exact-schema review naming an operation may release its tag. The original recommendation to drop this hold is superseded; use [wire conventions section 1.3](serving-wire-conventions.md#13-field-number-allocation-c1). |
| Packaged clients | Mandatory new runtimes; optional packages; or generated-stub recipes first | Generated-stub recipes/optional examples first, no dependency change to REST packages. Separately approve packaging and supported runtime versions. |
| Pivot/schema cancellation (public Rust API) | (a) Adapter-only: stop waiting at the client deadline, retain permits, drop the result; (b) honor `ReadOptions.cancel` or a deadline inside pivot and schema code; (c) add a public method or parameter | (a) for gap12-server; (b)/(c) need explicit approval and no new `Error` variant. Evidence: `src/property_graph/read.rs:61-69`, `src/property_graph/retrieve.rs:12-18`, `src/property_graph/retrieve.rs:301`, `src/property_graph/mod.rs:189`, `src/property_graph/engine/schema.rs:1130`. |
| Sibling RPCs, method count and sequencing | This document alone defines the `Graph` service; or later documents append methods | **Withdrawn:** an absolute post-ALG method total, because earlier slices append RPCs too; see [wire conventions sections 1.1 and 5.1](serving-wire-conventions.md#51-order). Graph lands with 17 unary RPCs at step 3; later slices append to what is present in landing order (TTL step 5, temporal step 6, five ALG RPCs step 8), adding named descriptor and parity entries. No slice promises an absolute total. Gap 8's reservations (section 2) stay untouched. |

Open implementation details: fix a concrete descriptor baseline after Gap 8
lands; select reproducible generated JavaScript/.NET runtime versions without
changing REST dependencies; assign reviewers for each slice. Schema review must
resolve the decisions above or explicitly record deferral before code. No
performance, size-reduction, release or readiness claim follows from this
documentation.
