# Temporal graph serving contract

Status: proposed exact schema for Gap 4 slice E; review required before code.
This document follows the [serving wire conventions](serving-wire-conventions.md), whose shared owner decisions were selected on 2026-11-02; items still unchecked for it in that note's section 7 remain conformance work.
Nothing in this document activates a file, changes a default, or ships an API.
The owner's 2026-11-02 authorization requires exact schemas to be reviewed first
([next-major](next-major.md), lines 49-55); qualification remains separate.
Scope: REST, `vlite.v1.Graph`, MCP, graph CLI, HTTP clients and correction feeds.
Prerequisites: qualified Rust slice C and the applicable
[Gap 12 graph service](serving-contract.md). The
[temporal design](gap4-temporal-graph.md) defines engine semantics.
All wire names below are proposed. **NEW** items do not exist in this checkout;
[section 9](#9-owner-decisions-and-open-items) lists each owner decision they need.
Cross-document wire vocabulary follows that proposal, pending owner ratification.

Non-goals: temporal nodes, transaction-time snapshots, bitemporality, inferred
contradictions, historical vectors/evidence, TTL replication, new query grammar,
an online converter, dashboard temporal editing, performance or storage claims.

## 1. Verified current coverage

These citations describe this checkout, not the older audit baseline of sibling
designs. A shared-open refusal is not a separate temporal-aware implementation.

| Surface or invariant | Current coverage and evidence |
|---|---|
| Public Rust writes | `TemporalSegment`, `TemporalOp::{CreateEdge,PromoteEdge,AppendSegment,ReplaceSegment,CloseSegment,RemoveSegment}` exist at `src/property_graph/temporal.rs:27-103`; `GraphOp::Temporal` at `src/property_graph/mod.rs:327-328`. |
| Rust reads, not public | `GraphRef::as_of`, `GraphRef::temporal_read`, `TemporalReadView::{get_edge,get_edge_by_key,get_node,get_node_by_id,find_nodes,neighbors,timeline,query,explain,query_text,explain_text,traverse,shortest_path,retrieve}` are crate-private at `src/property_graph/temporal/read.rs:91-98,200-416`. Do not advertise them as released public APIs. |
| Identity and bounds | One ID/key, fixed endpoints/type, no segment ID; half-open signed epoch-ms intervals, open ends, no merging, 256 maximum: `src/property_graph/engine/temporal.rs:1-24,72-88`; adapter bounds are `Option<i64>`: `src/property_graph/temporal.rs:31-35`. |
| Stored presence | Bounds use one presence byte, bits 0/1, followed by present little-endian i64 values; tags 65-69 do not alter legacy edge payloads: `src/property_graph/engine/codec/temporal_entries.rs:1-20`. This is storage encoding, not protobuf encoding. |
| Activation/open gate | `FLAG_TEMPORAL=64`: `src/format.rs:70-75`; header refusal before frame loading: `src/format.rs:1440-1466`; non-test support is false: `src/property_graph/temporal.rs:265-275`. The standalone engine is also test-gated: `src/property_graph/engine/temporal.rs:25-34`. |
| Writes without capability | Refuse before planning: `src/property_graph/mod.rs:899-906`; no mutation enables a file. Copy-out is crate-private, gate-checked, frame-preserving and new-lineage: `src/property_graph/temporal/activate.rs:1-17,36-86`. |
| Server and dashboard | Server opens through `Database::open`/`open_encrypted`: `src/bin/vlite-server.rs:515-518`, reaching the header gate above. The dashboard fetches REST, not database files: `src/dashboard/graph.js:571-587`; it has no independent `UnsupportedFeature(64)` open site. Static assets being available does not mean a refused database is served. |
| CLI and MCP open | Graph CLI write/read opens: `src/bin/vlite-graph-cli.rs:274-317`; MCP plain/encrypted/read-only opens: `src/bin/vlite-mcp.rs:205-211`. Both reach the same header gate. CLI maps the error to `unsupported_feature`: `src/bin/tool_support/graph_json.rs:45`. |
| Embedded bindings | C plain opens: `bindings/c/src/lib.rs:818-820`; encrypted/read-only/byte opens: `bindings/c/src/features.rs:388-403`; Python delegates there: `bindings/python/vlite_embedded/__init__.py:139-168`; Node delegates there: `bindings/node/index.js:112-117` and `bindings/node/addon.cc:230`; WASM byte open: `bindings/wasm/src/lib.rs:55`. All inherit header refusal, not temporal support. |
| Feeds | `Database::temporal_changes_after` is public but gated: `src/property_graph/temporal.rs:232-263`. `GraphChange::Temporal` and five `TemporalChange` variants: `src/property_graph/change.rs:70-122`. Ordinary feeds refuse instead of dropping entries: `src/property_graph/feed.rs:278-286`; REST and webhooks call ordinary `changes_after`: `src/server/features.rs:119`, `src/server/webhooks.rs:420`. JSON `unsupported_temporal` and gRPC `Unspecified` are unreachable markers, not wire support: `src/changes.rs:575`, `src/server/grpc.rs:770-772`. |
| Existing REST graph routes | Graph lifecycle, batch, node/edge get, find, neighbors, traverse, shortest-path, explain, schema, retrieve and query routes: `src/server/graph.rs:123-185`. Slice E adds timeline beside `/edges/get` and extends existing requests; it does not duplicate traversal routes. |
| Existing MCP/CLI | Graph tool names and shared JSON dispatch: `src/bin/tool_support/graph_mcp.rs:34-52,119-136,182-192`; CLI commands, JSON stdin and writable flag: `src/bin/vlite-graph-cli.rs:33-82,192-207`. |
| Existing gRPC | `ChangesRequest`, `WatchChangesRequest`, graph enums/operations/event tags: `proto/vlite/v1/vlite.proto:455-530`. Gap 12's proposed `Graph` service is in `docs/projects/roadmap/serving-contract.md:140-162,199-208`, not an implemented service in that proto. |
| Existing clients | HTTP feed methods: `clients/python/vlite_client/__init__.py:911-917`, `clients/javascript/index.js:376-379`, `clients/dotnet/VLite.Client/Client.cs:100-105`. No HTTP client has a graph accessor, so every proposed client method below is **NEW**. |

## 2. Numbered acceptance requirements

1. **TE-001, activation:** refuse unsupported temporal files before publishing
   any database handle, route data or feed event. Only an explicit, qualified
   per-file copy-out may add capability 64; never convert on open or first write.
2. **TE-002, lossless time:** preserve every signed i64 millisecond, including
   extremes, negative times and open ends, identically across all surfaces.
3. **TE-003, atomic corrections:** preserve whole-series identity, exact old
   bounds, batch-start expected revisions, schema validation and rollback.
4. **TE-004, fixed view:** capture one validity time T, TTL clock C and sequence;
   apply them to every edge and endpoint and every page, without historical
   node-property or evidence claims.
5. **TE-005, certified reads:** expose no silently truncated answer. Preserve
   completeness, reasons and work accounting; shortest-path exhaustion cannot
   certify either a path or absence.
6. **TE-006, parity:** REST, gRPC and MCP enforce identical graph/tenant grants
   before lookup; local CLI/bindings confer only the file access they already own.
7. **TE-007, feeds:** opt in to correction vocabulary before receiving it;
   preserve durable commit order and whole-commit cursors; emit no event when
   wall time merely crosses a validity boundary.
8. **TE-008, compatibility:** no legacy field/tag/enum redefinition, silent
   downgrade, automatic activation, or changes to old-file/default behavior.
9. **TE-009, publication:** schema approval, implementation acceptance and
   activation qualification are three distinct gates; document every supported
   public surface and explicitly refuse the rest.

## 3. Shared JSON and engine semantics

All JSON objects reject unknown request fields. `graph` is an unqualified
tenant-local name in REST paths and a required string in MCP/CLI requests.
IDs, revisions and sequences keep the existing decimal-string output and
decimal-string-or-exact-integer input (`src/server/graph.rs:4-8`).
New temporal times are **canonical decimal strings on output** (signed i64
epoch milliseconds UTC); input accepts those strings or exact JSON integers.
Reject floats, exponents, whitespace, `+`, leading zeros (except `0`), `-0`,
out-of-range values and integers beyond ±9,007,199,254,740,991 (2^53-1), which a
JavaScript parser may already have rounded: send those as strings; they fail as
REST 422 `invalid_json`. Never parse a timestamp through floating point. Prose
`Time`/`TimeInput` abbreviate `GraphTemporalMillis` (output) and `GraphTemporalMillisInput`;
`ID`/`U64String` a decimal u64 string (input `GraphTemporalU64Input`);
`Bounds`/`Segment` the `GraphTemporal*` schemas. TTL clock output is unsigned u64 ms.

`Segment = {valid_from_ms: Time|null, valid_to_ms: Time|null, properties: Metadata}`.
Input bounds may be absent or null: both mean unbounded on that side. Output
always includes both, null for negative/positive infinity respectively.
`properties` defaults to `{}`; null is invalid. No integer is an infinity
sentinel. An all-time segment is
`{"valid_from_ms":null,"valid_to_ms":null,"properties":{}}`.
For correction selectors, the `old_bounds` object is required; its missing or
null members select the corresponding infinite end, never a wildcard.
Close's `at_ms` and read's explicit `as_of_ms` must be finite; null is invalid.

Apply `[from,to)` exactly: inclusive start, exclusive end; finite `from < to`.
At most 256 sorted, nonoverlapping segments; touching/gaps allowed, never merged.
These copy engine rules (`src/property_graph/engine/temporal.rs:1-24,72-88`).
An empty series is retained identity, not an ordinary edge or a deleted edge.
Selected edge properties are only the matching segment's properties. `validity`
on an edge is null for a legacy edge, otherwise
`{valid_from_ms:Time|null,valid_to_ms:Time|null}`; open/open is not legacy
(private distinction: `src/property_graph/temporal/read.rs:41-47`).

Metadata retains the graph typed `{type,value}` representation, not arbitrary
untyped JSON (`src/bin/tool_support/graph_json.rs:727-729`).
Node/edge/property/string/key/type/model caps remain those of
`src/property_graph/engine/limits.rs:10-37`: 256 UTF-8 bytes/key, 63/type,
64 properties, 65,536 encoded property bytes, 4,096 bytes/string, 64 conditions.
Every temporal mutation counts toward 4,096 user operations and the full encoded
16,777,216-byte body; a complete series must fit one body. Engine evidence:
`src/property_graph/engine/limits.rs:31-35`,
`src/property_graph/engine/temporal.rs:51`,
`src/property_graph/engine/codec/temporal_entries.rs:246-257` (series bytes),
`src/property_graph/mod.rs:889-891` (library `BatchTooLarge`). REST already
refuses more than 4,096 operations as 422 `invalid_input`
(`src/server/graph.rs:110,717-720`). Transport caps (8 MiB bodies,
[wire caps](#errors-and-resource-rules)) are below the 16,777,216-byte body, so
the transport can refuse a series the engine accepts.

### Writes: one atomic batch, six variants

Use existing `POST /v1/graphs/{graph}/batch`, operationId `GraphBatch`,
scope `graph_write`; status 200, including create/retry/no-op, never 201.
No new one-operation REST endpoints. Each row is an exact JSON operation shape;
`E` is exactly one of `{"id":"7"}` / `{"key":"employment-7"}`,
`R` is required `{"revision":"42"}`, `S` is a Segment, `B` is old bounds.

| `op` and required fields | Meaning / Rust mapping |
|---|---|
| `temporal_create_edge`, `source:"a"`, `destination:"b"`, `relationship:"EMPLOYED"`, `segments:[S]`; optional `key` string/null, default null | `TemporalOp::CreateEdge`; `segments` required, 0-256. Endpoints are node **keys**, not selectors. |
| `temporal_promote_edge`, `edge:E`, `expected:R` | `TemporalOp::PromoteEdge`; retain identity and copy ordinary properties to one all-time segment. |
| `temporal_append_segment`, `edge:E`, `segment:S`, `expected:R` | `TemporalOp::AppendSegment`; ordered insertion, including backfill. |
| `temporal_replace_segment`, `edge:E`, `old_bounds:B`, `segment:S`, `expected:R` | `TemporalOp::ReplaceSegment`; replace exact old segment, including its properties and potentially bounds. |
| `temporal_close_segment`, `edge:E`, `old_bounds:B`, `at_ms:"200"`, `expected:R` | `TemporalOp::CloseSegment`; strictly inside old bounds, or exact finite-end no-op after revision/segment validation. |
| `temporal_remove_segment`, `edge:E`, `old_bounds:B`, `expected:R` | `TemporalOp::RemoveSegment`; remove that segment, retaining a possibly empty series. |

Names and mappings are verified at `src/property_graph/temporal.rs:56-102`;
revision/no-op handling at `src/property_graph/engine/mutate/temporal.rs:12-64,139-155`.
`expected:any`, `expected:absent`, missing revisions and null are invalid for
the five existing-series operations. Create has no expected field: identical
keyed retry returns `created:false`; another live edge at the key conflicts.
Corrections compare the revision before the batch, even for close+append in the
same batch (`src/property_graph/temporal.rs:40-46,57-69`).
Do not correct a newly created series in the same batch; create its complete
series instead. New wire acceptance must test this against the planner.
Ordinary edge patch cannot alter a temporal series; ordinary delete removes
the whole series. No automatic close, merge or cross-series exclusivity.

```json
{"ops":[
  {"op":"temporal_close_segment","edge":{"key":"employment-7"},"old_bounds":{"valid_from_ms":"100","valid_to_ms":null},"at_ms":"200","expected":{"revision":"42"}},
  {"op":"temporal_append_segment","edge":{"key":"employment-7"},"segment":{"valid_from_ms":"200","valid_to_ms":null,"properties":{"role":{"type":"string","value":"lead"}}},"expected":{"revision":"42"}}
]}
```

Response (values illustrative; golden tests fix them): `{"graph":"g","revision":"43","outcomes":[
{"edge":{"id":"7","created":false,"revision":"43"}},
{"edge":{"id":"7","created":false,"revision":"43"}}]}`.
Each operation produces its existing edge outcome; a wholly no-op batch has
top-level `revision:null`, while outcomes retain the existing series revision.
The batch envelope and edge outcomes already exist
(`src/bin/tool_support/graph_json.rs:1016-1060`,
`src/property_graph/mod.rs:999-1025`); their new wire inputs need golden tests.

### Reads: extend existing requests; add bounded timeline

Optional `as_of_ms: TimeInput` on all structural POST reads (node/edge get,
find, neighbors, traverse, shortest-path, explain, retrieve, query).
Absent means T=C captured once per operation, checked for i64 overflow.
No client TTL-clock override: C is server/local epoch ms, returned but never
accepted. Expiry is destructive retention, not history: at C equal to an edge or
endpoint expiry the edge is invisible, even when T is earlier
(`src/property_graph/temporal/read.rs:68-79,108-149,285`).
No new text grammar; `as_of_ms` is outside the query string.

| REST POST suffix; operationId | Request fields, beyond `as_of_ms`, `limits`, `projection` (timeline accepts none of the three) | Response-specific fields |
|---|---|---|
| `/edges/get`; `GraphGetEdge` | `edge:E` required | `edge: Edge|null`, with selected `validity` |
| `/neighbors`; `GraphNeighbors` | `node:E` required; `direction:"out"` default (`out,in,both`); `types` omitted/null = all, nonempty array selects types, `[]` invalid; `edge_conditions:[]`; optional `limit`, `cursor` | `nodes:[Node]`, `edges:[Edge]`, each selected edge has `validity` |
| `/traverse`; `GraphTraverse` | `start:[E]` required; direction/types as above; `expand_filter:{}`, `return_filter:{}`, `edge_filter:[]`, `include_paths:false` | `nodes:[{depth:uint,node:Node}]`, `edges:[Edge]`, `paths:[{nodes:[ID],edges:[ID]}]` |
| `/shortest-path`; `GraphShortestPath` | `source:E`, `destination:E` required; direction/types as above | `shortest_path:{outcome:"found",path:Path,nodes:[Node],edges:[Edge]}` or `{outcome:"no_path_within_hops"}` or `{outcome:"exhausted",truncation:[Reason]}` |
| `/edges/timeline`; **`GraphEdgeTimeline`** | `edge:E` required; `start_index:uint=0`, `end_index:uint=256`; `0 <= start <= end <= 256`; no filters or cursor; no read options | `timeline:null` or `{edge_id:ID,revision:ID,start_index:uint,end_index:uint,total_segments:uint,segments:[Segment]}` |

The base typed request fields above are verified at
`src/bin/tool_support/graph_json.rs:520-606`; output shapes at
`src/bin/tool_support/graph_json.rs:733-752,765-767,950-1010`.
Filters are `{labels:[string]=[],conditions:[Condition]=[]}`; edge filters are
typed conditions. Projection remains `"all"` (default), `"none"`, or
`{"only":[string]}` (`src/bin/tool_support/graph_json.rs:493-506,836-842`).
Neighbor `limit` defaults to effective `limits.max_output_edges` (200 by
default), ceiling 2,000; page size must be at least one, including when derived
from an output cap. Preserve these existing rules
(`src/property_graph/engine/query.rs:480-486,511-528,740-744`).
Work/output limits still apply independently.

On a temporal file every successful read is HTTP 200, keeps its existing
response shape and adds top-level `as_of_ms:Time` and
`ttl_clock_ms:U64String` (`src/property_graph/temporal/read.rs:19-39`);
non-temporal responses are unchanged. Typed reads share
`{graph,view_sequence,complete,truncation,stats,cursor,...}`
(`src/bin/tool_support/graph_json.rs:950-1010`); explain keeps
`{graph,explain:{...}}` (`graph_json.rs:938-948`); retrieve keeps its own
`stats` and has no cursor (`graph_json.rs:1542-1594`).
Edge and node field names remain those cited above; a missing/hidden get or
timeline is null, not 404. Timeline ignores validity, not TTL, and does no
budgeted work: `complete:true`, empty `truncation`, zero `stats`, null `cursor`.
A legacy-edge timeline is 422 `invalid_input` category `not_temporal`. Indices
are requested half-open segment indices, sliced at actual length; report them
unchanged with the total count before slicing and ascending retained segments.
Timeline absence and empty-series timeline are distinct
(`src/property_graph/temporal/read.rs:264-315`). Timeline completeness certifies
the requested range, not all history.

`Stats` preserves `nodes_visited,nodes_scanned,edges_examined,nodes_returned,
edges_returned,paths_returned,elapsed_ms,indexes`; reasons preserve strings
`visited_limit,edge_limit,output_limit,deadline,cancelled` or
`{"degree_limit":{"node_ids":[ID]}}`
(`src/bin/tool_support/graph_json.rs:776-791,816-827`).
Neighbor pages may be explicitly incomplete with a continuation cursor.
Traversal may return explicitly incomplete discovered data; it is not a
certified complete reachability result. A shortest-path budget interruption
returns **exhausted with no path**, not a provisional path or certified absence;
preserve the engine certification rule at
`src/property_graph/engine/traverse.rs:242-246`.
Do not turn engine incompleteness into `complete:true` at serialization.

Example: `POST /v1/graphs/g/neighbors`
`{"node":{"key":"a"},"as_of_ms":"150","limit":1}` returns
`{"graph":"g","view_sequence":"43","as_of_ms":"150","ttl_clock_ms":"300",
"complete":true,"truncation":[],"stats":Stats,"cursor":null,
"nodes":[Node],"edges":[{...Edge,"validity":{"valid_from_ms":"100","valid_to_ms":"200"}}]}`.
`Node`, `Edge` and `Stats` stand for the shapes above, not literal JSON tokens.
Timeline request `{"edge":{"id":"7"},"start_index":0,"end_index":1}` returns that
envelope with `timeline` instead of `nodes`/`edges`.

Paging binds resolved T and C, normalized query/projection, graph and sequence;
omitting T on continuation reuses cursor T, not now. Conflicting explicit T is
422 `invalid_input`; a changed graph is 409 `graph_cursor_stale`. Never refresh
C between pages. Private implementation evidence:
`src/property_graph/temporal/read.rs:108-149,225-231`.
Reject malformed cursors and cross-graph reuse.
No cursor for traverse, shortest-path or timeline; no stateless timeline
continuation guarantee across revisions. Repeat ranges with the same returned
revision/sequence, or restart if either changes.

### Errors and resource rules

Codes not marked **NEW** exist today (`src/server.rs:1380-1662`); proposed
mapping additions follow [wire conventions section 2](serving-wire-conventions.md#2-error-registry-c2-c9).

| Condition | REST / wire code | gRPC status |
|---|---|---|
| Missing authentication | 401 `unauthorized` | UNAUTHENTICATED |
| Wrong grant/tenant | 403 `forbidden_scope`; write through read-only principal: 403 `read_only` | PERMISSION_DENIED |
| Temporal unavailable, file not opted in, or feed not opted in | **NEW** scoped 409 `unsupported_feature`, `details:{feature:64,category:"capability"}`; legacy routes keep 500 `internal_error` | FAILED_PRECONDITION |
| Missing graph/entity for a mutation | 409 `graph_not_found`, `graph_edge_not_found`, `graph_node_not_found` | FAILED_PRECONDITION |
| Expected revision mismatch | 412 `precondition_failed` (`src/server.rs:1523-1527`) | ABORTED (`src/server/grpc.rs:181-189`) |
| Keyed create conflicts with another live edge | 409 `graph_key_conflict` | FAILED_PRECONDITION |
| Malformed or oversize JSON; wrong type, unknown field or non-canonical/unsafe time | 400 (syntax) / 413 (oversize) / 422 (shape) `invalid_json` (`src/server/graph.rs:344-365`) | INVALID_ARGUMENT for adapter validation; transport rejection per wire conventions section 2.2 |
| Invalid temporal operation or read; more than 4,096 operations | 422 `invalid_input`, `details.category` below for temporal reasons | INVALID_ARGUMENT |
| Above graph ceiling | 422 `graph_limit_exceeds_ceiling` | INVALID_ARGUMENT |
| Admission saturation | 503 `busy` | UNAVAILABLE |
| Poisoned database / indeterminate commit | 503 `poisoned` / 503 `commit_indeterminate` | INTERNAL / UNKNOWN |
| Stale read / wrong-lineage feed / below history floor | 409 `graph_cursor_stale` / `cursor_compacted` / `cursor_before_history` | FAILED_PRECONDITION |
| **NEW** feed cursor bound to another feature set | 409 `cursor_mode_mismatch` | FAILED_PRECONDITION |
| **NEW** whole response/commit cannot fit wire cap | 413 `response_too_large`; no cursor advance | RESOURCE_EXHAUSTED |
| **NEW** engine or adapter deadline before commit, no partial allowed | 504 `deadline_exceeded` | DEADLINE_EXCEEDED |

Proposed D8/D9 replacements follow [wire conventions decisions 4, 7 and 21](serving-wire-conventions.md#6-owner-decisions-23)
and [section 2.2](serving-wire-conventions.md#22-one-reachable-family-oversize-deadline-cancel-busy).
**Withdrawn:** separate request-size, transport-deadline and cancellation codes,
and per-draft error-body members/trailer; the shared registry and context proposal replace them.
For each new code, the temporal handler/adapter builds an `ApiError`:
`unsupported_feature` from the existing `Error::UnsupportedFeature` or explicit
temporal/feed negotiation refusal; `cursor_mode_mismatch` from the feed feature-set
check; `response_too_large` from response preflight; `deadline_exceeded` from the
engine stop reason before commit. No new public `Error` variant or global
`UnsupportedFeature` conversion arm is proposed. Legacy routes retain their
existing mapping (`src/server.rs:1624`).
Error context uses only `details`, per [wire conventions section 3](serving-wire-conventions.md#3-encoding-rules-c12);
the REST `error.details` object remains proposed under decision 21, while gRPC
uses the existing details trailer. Temporal `details.category` is the reason prefix before `:` of the engine InvalidQuery
message: `bounds,order,overlap,segment_limit,no_segment,close,not_temporal,
temporal_edge,series_bytes,expected,already_temporal,capability`
(`src/property_graph/engine/temporal.rs:37-56`; `capability` is never an
`invalid_input` category:
a served file reports `unsupported_feature` with `details.category`
`capability`). Two timeline errors carry no prefix
(`src/property_graph/temporal/read.rs:273,291`) and map by exact message:
`timeline requires a temporal edge` to `not_temporal`,
`timeline bounds must satisfy start <= end <= 256` to `bounds`. Any other 422
`invalid_input` has no `details.category`; schema/key failures keep existing codes.
`invalid_request` exists only in CLI/MCP/proxy (`src/bin/vlite-graph-cli.rs:101`,
`src/bin/tool_support/graph_mcp.rs:61`, `src/bin/tool_support/graph_proxy.rs:190`)
for failures REST reports as `invalid_json`. No broad `internal_error` fallback
for validation or the scoped temporal unsupported-feature failures.

**NEW** refusal JSON (D8; engine text today is
`database header declares unsupported feature flags 0x0040`, `src/lib.rs:634-637`):
`{"error":{"code":"unsupported_feature","message":"temporal capability 64 is unavailable for this operation","details":{"feature":64,"category":"capability"}}}`.
gRPC Status message is that message, code FAILED_PRECONDITION; existing trailers
`vlite-error-code: unsupported_feature` and `vlite-error-details-base64` carry
the code and the base64-encoded JSON details respectively, per wire conventions section 3.
MCP tool failure has `isError:true`, text
`unsupported_feature: temporal capability 64 is unavailable for this operation`,
and `structuredContent:{"error":"unsupported_feature: temporal capability 64 is unavailable for this operation"}`
per [wire conventions section 2.3](serving-wire-conventions.md#23-partial-versus-error).
CLI prints the refusal JSON to
stderr and exits 1 with empty stdout (today's shape lacks `details`,
`src/bin/vlite-graph-cli.rs:337-353`). A failed local open prints this
diagnostic before accepting commands/tools; it is not a successful MCP response.
Bindings keep their language error result, code `unsupported_feature`, feature 64
preserved. All of this is new normalization.

Retain graph limits, defaults/ceilings respectively:
hops 2/4, visited 1,000/10,000, examined 10,000/100,000, degree 1,000/10,000,
deadline 250/5,000 ms, output nodes 100/1,000, edges 200/2,000, paths 20/100.
Reject above ceilings, never clamp; zero hops/output valid, zero work/deadline
invalid (`src/property_graph/engine/limits.rs:63-69,87-108,125-157`).
Charge invalid-at-T adjacency entries too. Use elapsed deadline time, never T.
Use min(client remaining deadline where the transport supplies one, graph
deadline, configured lower ceiling), including queueing/serialization;
propagate disconnect/cancellation to engine checks (**NEW** for REST, which
passes `cancel: None`: `src/server/graph.rs:458,497,609`).
A graph-budget deadline may return a certified incomplete envelope
while transport time remains; transport expiry and cancellation follow
[wire conventions section 2.2](serving-wire-conventions.md#22-one-reachable-family-oversize-deadline-cancel-busy), not a new domain wire code.
No successful partial serialization, dropped segments or truncated commits.

Wire caps today: REST graph reads 1 MiB (`src/server/graph.rs:79-80,184`), other
REST bodies 8 MiB (`src/server.rs:60,742`), gRPC decode 8 MiB and encode 16 MiB
(`src/server/grpc.rs:23,25,98-99`); responses are uncapped elsewhere, and the
legacy CLI accepts 64 MiB of input (`src/bin/vlite-graph-cli.rs:72`).
**NEW** (D10): temporal requests and files get 1 MiB read, 8 MiB batch and 16 MiB
response caps, including envelopes, across REST/gRPC/MCP/CLI/client methods;
non-temporal behaviour is unchanged. Proposed [wire conventions decision 3](serving-wire-conventions.md#6-owner-decisions-23)
would keep REST without a response cap and make `response_too_large` gRPC-only;
see [sections 1.5 and 2.2](serving-wire-conventions.md#15-size-and-count-caps-c14).
Reject oversize before mutation;
preflight the bounded batch response before committing. Timeline cannot
silently omit oversized properties: caller requests a smaller index range.

## 4. Capability, activation and authorization

A release build currently refuses at header open even an empty flag-64 file
(section 1). New serving support may accept one only after engine C, correction
feed decoding, auth parity and serving qualification all pass. Qualification
does not set any file flag. An accepted temporal file applies fixed T/C even
when the client omits T; old requests cannot see empty temporal properties.
On an old file, ordinary operations remain unchanged; explicit `as_of_ms`,
timeline and temporal writes refuse `UnsupportedFeature(64)`. A version-aware
feed may read an old file unchanged. Unsupported surfaces never open in a
"legacy-only" mode or drop temporal content. An older server may answer a new
route with 404 or a new RPC with UNIMPLEMENTED; clients surface that failure,
never retry as a non-temporal operation.

Proposed operator workflow, only after separate activation qualification:
stop/close the source; run **NEW** `vlite-maintain enable-temporal SOURCE
DESTINATION` (positional like `enable-graph`, which passes no key,
`src/bin/vlite-maintain.rs:73-80`); validate the result; explicitly configure the
server to open DESTINATION; seed new replicas/backups from it. The proposed
public `maintenance::enable_temporal(source,destination,key) -> Result<Inspection>`
(`gap4-temporal-graph.md:606-611`) does **not** exist (crate-private,
`src/property_graph/temporal/activate.rs:37-41`); adding it and encrypted-source
key handling are D7. No REST/MCP/SDK activation operation.
The engine's existing crate-private copy-out preserves every plaintext frame,
sequence, revision and absolute deadline under a new lineage, resealing encrypted
frames under the same key with fresh salt; source is untouched, destination must
not exist (`src/property_graph/temporal/activate.rs:1-17,27-86`).
Do not compact/reclaim as part of activation or implement it as a header edit.
Open source, incomplete tail, wrong/missing key, no graph, already temporal,
existing destination and indeterminate publication remain explicit failures
(`src/property_graph/temporal/activate.rs:27-34,44-55`).
Keep secrets out of command arguments, audit and responses.

Every read route/RPC/tool, including timeline, requires `graph_read` plus graph
binding; batch requires `graph_write`. Any mixed vector/attachment mutation also
requires affected collection `write` under the same write guard. Retrieve also
requires pivot collection `read`, redacting unauthorized evidence. No new scope,
no validity-time exemption, no collection-wildcard implication.
These preserve existing rules (`src/server/graph.rs:10-54,85-108`).
Qualify tenant-local graph names before lookup, reject physical-name bypass,
unqualify responses/feed names and filter lists. Authorize before looking up
existence; recheck mutable grants before queued execution. Apply identical policy
in gRPC interceptors and proxied MCP; local stdio MCP/CLI is file-authorized,
not an unauthenticated remote service.

**NEW** audit fields (D12; today's audit and metrics are in `src/server.rs:1203-1300`,
`src/server/audit.rs`, `src/server/observability.rs`): `request_id,transport,operation,principal_id,tenant_id,
graph,lineage,scope,outcome,error_code,feature,as_of_ms,ttl_clock_ms,
view_sequence,expected_revision,committed_revision,complete,truncation,
nodes_visited,edges_examined,request_bytes,response_bytes,elapsed_ms,cancelled`.
Unresolved fields are null, not fabricated; record failed authorization without
looking up a forbidden graph. Never record keys, bounds lists, properties,
query text, evidence, credentials or encryption material.
**NEW** metrics use bounded labels `transport,operation,outcome,error_code,feature,
truncation_reason`; counts for refusal/activation attempts, latency/work/bytes
histograms, cancellation and limit exhaustion. No tenant/graph/time/key labels.
No cross-tenant result cache; audit follows existing retention/access policy.

## 5. MCP, CLI and language clients

All tool schemas use JSON Schema object, `additionalProperties:false`.
Existing tool names remain: `vlite_graph_write` takes `{graph,ops:[Operation]}`;
`vlite_graph_get` (exactly one node/edge), `vlite_graph_find_nodes`,
`vlite_graph_neighbors`, `vlite_graph_traverse`, `vlite_graph_shortest_path`,
`vlite_graph_query`, `vlite_graph_retrieve` gain optional `as_of_ms`.
Their input schema is the corresponding REST body schema plus required
`graph:string`; existing `explain:boolean=false` stays on typed reads.
`vlite_graph_edge_timeline` is **NEW**, schema
`{graph:string!,edge:E!,start_index:integer[0,256]=0,
end_index:integer[0,256]=256}` (no `as_of_ms` or `limits`, as in section 3).
Here `!` means required and `?` optional, not nullable; cross-field start/end
validation is mandatory. Responses are exactly REST JSON in structured content
and equivalent JSON text. Writes are listed/callable only with `--writable`;
read-only calls refuse before mutation. No activation tool.

CLI retains `vlite-graph-cli --db PATH [--writable] [--explain] COMMAND REQUEST`,
with `-` for stdin. `batch` uses the six JSON operations; existing get/read
commands gain JSON `as_of_ms`. Add **NEW** `edge-timeline` with the MCP timeline shape.
No separate timestamp flags, implicit Date parsing or conversion commands here.
Timeline disallows `--explain`; typed temporal reads allow it and return resolved
T/C/sequence in explanation metadata. Reads reject `--writable`; writes require
it. Success exits 0 with one JSON object. No-op is success, refusal exits 1.

Proposed **HTTP client** surface (every method **NEW**, D13), same request/result
types, no engine fallback:

| Operation | Python `client.graph(name)` | JavaScript `client.graph(name)` | .NET `client.Graph(name)` |
|---|---|---|---|
| Six writes, atomic mixed batch | `write_batch(ops)` | `writeBatch(ops, {signal}?)` | `WriteBatchAsync(ops, cancellationToken)` |
| Edge get | `get_edge(edge, *, as_of_ms=None)` | `getEdge({edge,asOfMs?}, {signal}?)` | `GetEdgeAsync(edge, asOfMs, cancellationToken)` |
| Neighbors | `neighbors(request, *, as_of_ms=None)` | `neighbors(request, {signal}?)` | `NeighborsAsync(request, cancellationToken)` |
| Traverse | `traverse(request, *, as_of_ms=None)` | `traverse(request, {signal}?)` | `TraverseAsync(request, cancellationToken)` |
| Shortest path | `shortest_path(request, *, as_of_ms=None)` | `shortestPath(request, {signal}?)` | `ShortestPathAsync(request, cancellationToken)` |
| Timeline | `edge_timeline(edge, *, start_index=0, end_index=256)` | `edgeTimeline({edge,startIndex?,endIndex?}, {signal}?)` | `EdgeTimelineAsync(request, cancellationToken)` |
| Opt-in feed | `changes(cursor=None, limit=1000, graph_events=["temporal"])` | `changes({cursor?,limit?,graphEvents:["temporal"],signal?})` | `ChangesWithGraphEventsAsync(cursor,limit,graphEvents:["temporal"],cancellationToken)` |

Other read methods in section 3 also forward the optional selector.
Python timestamps are range-checked `int` (bool rejected), bounds `None`;
omit `as_of_ms` for Python None, never serialize it as null. JavaScript uses
`bigint` or canonical decimal string, bounds null/undefined; rejects unsafe
Number inputs; maps camelCase to the snake_case wire. .NET uses `long`/`long?`
for validity and nullable segment bounds, `ulong` for IDs/TTL clocks and
`CancellationToken`. Clients decode to those lossless types; no Date/datetime
conversion or retry after unknown write outcome. Typed operation constructors
mirror all six discriminants, including required old bounds/revision.
Expose `details.feature` and `details.category` in typed errors, plus transport code.
Older-server errors remain explicit. Embedded C/Python/Node/WASM temporal
exposure is a separate qualification: until then refuse the file at open;
do not make HTTP client work silently depend on binding support.

## 6. Versioned correction feed

Proposed D2 negotiation follows [wire conventions section 4 and decision 8](serving-wire-conventions.md#4-graph-events-in-the-change-feed-c11).
Use existing REST `GET /v1/changes?graph_events=temporal&cursor=TOKEN&limit=1000`;
the temporal slice consumes the shared `graph_events` set and response echo,
not a separate temporal boolean or version discriminator.
Old servers: REST `ChangeQuery` is `deny_unknown_fields`, so `graph_events` is 400
`invalid_query` (`src/server/features.rs:90-103`); gRPC (proto3) skips unknown
request fields. A client that asked for temporal must fail the scoped
`unsupported_feature` negotiation check when the response echo does not contain
`temporal` (D2; producer in section 3).
**NEW** (D13): MCP read-only `vlite_temporal_changes`, schema
`{cursor:string?,limit:integer[1,10000]=1000}` with no graph field; authorization
filters every event's tenant graph and collection. **NEW** CLI `temporal-changes`
with that same request, no `--writable`/`--explain`; output is the REST batch.
Limit default 1000, maximum 10,000 are the current feed constants
(`src/changes.rs:22-23`; REST answers 422 outside that range,
`src/server/features.rs:108-110`). A whole temporal commit may exceed the event
limit; never divide a temporal commit to manufacture a resumable cursor.
TTL reclaim pagination remains governed by wire conventions section 4.

Opted-in REST/MCP batch:
`{changes:[Event],next_cursor:string,has_more:bool,head:U64String,
graph_events:["temporal"]}` (**NEW** feature-set echo, D2; echoes the requested set).
Existing event shape remains
`{sequence:U64String,ordinal:uint,collection:string,committed_at_ms:uint|null,
change:{type:"graph_batch",graph:string,revision:U64String,operations:[Change]}}`;
graph operations stay in original frame order. Existing JSON envelope evidence:
`src/server/features.rs:83-89,119-127` (`committed_at_ms` is a JSON number
today, `features.rs:87`); temporal contents are new.

Each temporal `Change` has `op` below, `id:ID`, `key:string|null`, plus exactly
these fields (bounds are explicit null or canonical Time strings):

| `op` | Additional fields |
|---|---|
| `temporal_edge_put` | `source:ID,source_key:string,relationship:string,destination:ID,destination_key:string,segments:[Segment],created:bool` |
| `temporal_segment_appended` | `segment:Segment` |
| `temporal_segment_replaced` | `old_bounds:Bounds,segment:Segment` |
| `temporal_segment_closed` | `old_bounds:Bounds,at_ms:Time` |
| `temporal_segment_removed` | `old_bounds:Bounds` |

`created:false` on edge put denotes promotion; ordinary delete is still ordinary
edge deletion. Corrections contain old **bounds**, not guessed old properties.
These map exactly to `TemporalChange` fields at
`src/property_graph/change.rs:81-122` and `src/property_graph/feed.rs:298-364`.
Commit order is sequence then ordinal, not validity order; append/backfill may
mention earlier validity time. No-op close emits nothing. No event at from/to.

**NEW** (D11) webhook subscription create/status field
`graph_events` is an immutable per-subscription feature set, default `[]`,
per [wire conventions section 4](serving-wire-conventions.md#4-graph-events-in-the-change-feed-c11); the admin-only
`/v1/admin/webhooks` routes are retained (`src/server/webhooks.rs:150-163`).
Delivery for opted-in subscriptions is
`{subscription_id:string,graph_events:["temporal"],changes:[Event]}` using
the exact REST event/correction shapes, signed over the complete bytes as today
(`src/server/webhooks.rs:473-490`). The dedup key stays
`(sequence,ordinal,collection)` per subscription (`docs/WEBHOOKS.md:39-40`); the
payload carries no lineage, so a lineage-bearing key needs a new envelope field
(D11).
Legacy subscriptions on a temporal file enter explicit blocked status
`last_error:"unsupported_feature:64"` (`src/server/webhooks.rs:26-31`), deliver
nothing and do not advance their cursor. The dispatcher consumes the shared
feed cursor and persists progress per feature set, per wire conventions section 4;
the empty-set `webhooks.cursor` path and format remain unchanged
(`src/server/webhooks.rs:42,405-436`; `docs/WEBHOOKS.md:45,52`).
Persisted state is `deny_unknown_fields` and a failed parse fails `Dispatcher::configure`
(`src/server/webhooks.rs:14,26,33,70-73`), so new fields need `#[serde(default)]`
and a downgrade after migration fails (D11). Never silently upgrade a
subscription; an operator must opt it in.

Additive vocabulary is safe only with negotiation: non-opted-in REST/gRPC feeds
on flag-64 files refuse **before any event**, including when empty or filtered.
This **strengthens** today's refusal, which fires only on meeting a temporal
entry (`src/property_graph/feed.rs:278-286`; D2).
Do not send unknown temporal enum values to legacy consumers or omit correction
operations from their commits. Empty-set requests on non-temporal files retain
legacy response shapes; opted-in responses echo the requested feature set.

Use the shared `FeedCursor` owned by `src/changes.rs` under
[wire conventions section 4](serving-wire-conventions.md#4-graph-events-in-the-change-feed-c11):
the TTL slice lands `vlc2` and temporal consumes it; `vlc1` remains the empty-set
token. Non-empty-set cursors bind lineage, sequence, ordinal and the feature set;
a different set yields `cursor_mode_mismatch`. Temporal commits stay whole under
the proposed decision 9; there is no validity-time or segment-index cursor.
Advance across fully processed/unauthorized commits,
never past a failed or partially delivered one; a response cap that cannot fit
the next whole commit errors without advancement. Checkpoint snapshots restate
state, not correction history: below floor is `cursor_before_history`, never
synthetic change replay (`src/changes.rs:266-277`). Same-lineage checkpoints
preserve cursor lineage; new-lineage compaction/upgrade/restore and temporal
activation invalidate old cursors with `cursor_compacted` (explicit reseed, not
automatic reset) and revision expectations must be reacquired. Consumers
bootstrap from a separately authorized consistent snapshot and its cursor; this
slice adds no snapshot endpoint. This slice adds no TTL deadline/reclaim
semantics; that vocabulary belongs to the preceding TTL slice and the requested
feature set under wire conventions section 4, not temporal opt-in alone.
Temporal correction output is not replication or historical-retention reconstruction.

## 7. Exact OpenAPI and proto proposal

`src/openapi.json` and `proto/vlite/v1/vlite.proto` stay frozen until the owner
approves **this** document. Existing graph paths merge from
`src/openapi_graph.json` (`src/server/graph.rs:1761-1786`), which inserts only
`paths` lines; approved REST work updates that fragment plus the root's shared
`components` (`src/openapi.json:121`), not duplicate routes.

This is an insertion manifest, not replacement of existing path objects:
preserve their security, parameters, responses and operationIds. Apply
`GraphTemporalReadSelector`'s property directly inside each existing closed request
object (not `allOf` outside `additionalProperties:false`) and extend responses
with resolved selectors and edge validity. The JSON types and conditional
required fields of sections 3 and 6 are normative.

Every non-200 response below carries
`content: {application/json: {schema: {$ref: '#/components/schemas/Error'}}}`
(elided); responses marked NEW follow the proposed shared registry cited in section 3.

```yaml
paths:
  /v1/graphs/{graph}/edges/timeline:
    parameters:
      - {name: graph, in: path, required: true, schema: {type: string, pattern: '^[a-z][a-z0-9_-]{0,62}$'}}
    post:
      operationId: GraphEdgeTimeline
      security: [{BearerAuth: []}]
      requestBody:
        required: true
        content:
          application/json:
            schema: {$ref: '#/components/schemas/GraphTemporalTimelineRequest'}
      responses:
        '200':
          description: Retained TTL-visible timeline range, or null
          content:
            application/json:
              schema: {$ref: '#/components/schemas/GraphTemporalTimelineResponse'}
        '400': {description: invalid_json}
        '401': {description: unauthorized}
        '403': {description: forbidden_scope}
        '409': {description: NEW unsupported_feature (D8); graph_not_found}
        '413': {description: invalid_json (oversize request); NEW response_too_large (D10)}
        '422': {description: invalid_json (shape), invalid_input}
        '503': {description: busy, poisoned}
        '504': {description: NEW deadline_exceeded (D9)}
components:
  schemas:
    GraphTemporalMillis:
      type: string
      pattern: '^(0|-[1-9][0-9]*|[1-9][0-9]*)$'
      description: Signed i64 epoch milliseconds; enforce -9223372036854775808..9223372036854775807
    GraphTemporalMillisInput:
      oneOf:
        - {$ref: '#/components/schemas/GraphTemporalMillis'}
        - {type: integer, minimum: -9007199254740991, maximum: 9007199254740991, description: Larger magnitudes must be sent as strings}
    GraphTemporalBoundsInput:
      type: object
      additionalProperties: false
      properties:
        valid_from_ms: {anyOf: [{$ref: '#/components/schemas/GraphTemporalMillisInput'}, {type: 'null'}]}
        valid_to_ms: {anyOf: [{$ref: '#/components/schemas/GraphTemporalMillisInput'}, {type: 'null'}]}
    GraphTemporalBounds:
      type: object
      additionalProperties: false
      required: [valid_from_ms, valid_to_ms]
      properties:
        valid_from_ms: {anyOf: [{$ref: '#/components/schemas/GraphTemporalMillis'}, {type: 'null'}]}
        valid_to_ms: {anyOf: [{$ref: '#/components/schemas/GraphTemporalMillis'}, {type: 'null'}]}
    GraphTemporalSegmentInput:
      type: object
      additionalProperties: false
      properties:
        valid_from_ms: {anyOf: [{$ref: '#/components/schemas/GraphTemporalMillisInput'}, {type: 'null'}]}
        valid_to_ms: {anyOf: [{$ref: '#/components/schemas/GraphTemporalMillisInput'}, {type: 'null'}]}
        properties: {type: object, maxProperties: 64, additionalProperties: {$ref: '#/components/schemas/MetadataValue'}, default: {}}
    GraphTemporalSegment:
      type: object
      additionalProperties: false
      required: [valid_from_ms, valid_to_ms, properties]
      properties:
        valid_from_ms: {anyOf: [{$ref: '#/components/schemas/GraphTemporalMillis'}, {type: 'null'}]}
        valid_to_ms: {anyOf: [{$ref: '#/components/schemas/GraphTemporalMillis'}, {type: 'null'}]}
        properties: {type: object, maxProperties: 64, additionalProperties: {$ref: '#/components/schemas/MetadataValue'}}
    GraphTemporalReadSelector:
      type: object
      properties:
        as_of_ms: {$ref: '#/components/schemas/GraphTemporalMillisInput'}
    GraphTemporalTimelineRequest:
      type: object
      additionalProperties: false
      required: [edge]
      properties:
        edge: {$ref: '#/components/schemas/GraphTemporalEntitySelector'}
        start_index: {type: integer, minimum: 0, maximum: 256, default: 0}
        end_index: {type: integer, minimum: 0, maximum: 256, default: 256}
    GraphTemporalEntitySelector:
      oneOf:
        - {type: object, additionalProperties: false, required: [id], properties: {id: {$ref: '#/components/schemas/GraphTemporalU64Input'}}}
        - {type: object, additionalProperties: false, required: [key], properties: {key: {type: string, minLength: 1, maxLength: 256}}}
    GraphTemporalU64Input:
      oneOf:
        - {type: string, pattern: '^(0|[1-9][0-9]*)$', description: Enforce u64 range}
        - {type: integer, minimum: 0, maximum: 18446744073709551615}
    GraphTemporalExpectedRevision:
      type: object
      additionalProperties: false
      required: [revision]
      properties:
        revision: {$ref: '#/components/schemas/GraphTemporalU64Input'}
```

Remaining insertions, as construction rules (existing graph schemas are not copied):

| Target | Exact edit |
|---|---|
| Existing read POST operationIds `GraphGetNode,GraphGetEdge,GraphFindNodes,GraphNeighbors,GraphTraverse,GraphShortestPath,GraphExplain,GraphRetrieve,GraphQueryText` | Add request `as_of_ms` above (existing inline `limits` objects unchanged), temporal errors, and response `as_of_ms:GraphTemporalMillis` and `ttl_clock_ms:decimal u64 string`, both required when the file is temporal. Add required `validity:GraphTemporalBounds|null` to edge objects in temporal responses. Non-temporal responses unchanged. |
| `GraphBatch` | **Withdrawn:** closed `oneOf` alternatives on `ops.items` would restructure the shared schema. Proposed [wire conventions section 5.2, decision 23](serving-wire-conventions.md#52-per-file-owner-and-sequence): retain the single open `ops.items` object and `maxItems:4096`; add the six write-table literals to its `op` enum and one temporal `allOf` `if`/`then` branch that `$ref`s `GraphTemporalWriteOperation`. No existing variant changes. |
| `GraphTemporalWriteOperation` component | One object using the input schemas above; `op` enum contains all six write-table discriminants. Six internal `allOf` `if`/`then` branches on the `op` const `$ref` the named schemas `GraphTemporalCreateEdge,GraphTemporalPromoteEdge,GraphTemporalAppendSegment,GraphTemporalReplaceSegment,GraphTemporalCloseSegment,GraphTemporalRemoveSegment`; each is a closed object with the exact properties/required/defaults of its write row, not a `oneOf` union. Source/destination: key strings (1-256 UTF-8 bytes); relationship 1-63 ASCII identifier bytes; key nullable, default null; segments maxItems 256. |
| `GraphTemporalTimelineResponse` | Closed object with every shared read field required (`cursor` null; sequence/TTL decimal u64 strings; canonical `as_of_ms`; `complete`, `truncation`, `stats` as existing reads) plus required nullable `timeline`: a closed object with the six section 3 fields required, segments maxItems 256, response segment bounds required. |
| Error schema | Proposed D8 context follows [wire conventions section 3, decision 21](serving-wire-conventions.md#3-encoding-rules-c12): only optional `error.details`, with `feature` and `category` inside it; no per-draft body members. Scoped temporal unsupported response fixes `details.feature` to 64 and `details.category` to `capability`, with message/code as section 3. |
| `/v1/changes` GET, existing operationId preserved | Consume the shared `graph_events` query set and response echo under wire conventions section 4; append five temporal operation alternatives only to the temporal-opted-in schema (D2). |
| `/v1/admin/webhooks` POST/GET, `/v1/admin/webhooks/{id}` GET | Add immutable subscription `graph_events` feature set, default `[]`; use the opted-in delivery envelope and per-set progress from section 6; existing operationIds/auth unchanged (D11). |

`BearerAuth`, `MetadataValue` and `Error` are existing root definitions
(`src/openapi.json:123,195,198`); add no second metadata/error type. Use OpenAPI
3.1 null unions, not 3.0 `nullable` (root version, `src/openapi.json:2`).
UTF-8 byte lengths, i64/u64 string ranges, start<=end, nonoverlap and conditional
required response fields need runtime validation beyond OpenAPI's structural
checks; goldens must test them.

Proto additions below are exact proposed declarations. Reuse existing
`MetadataValue`/`Expected` (`proto/vlite/v1/vlite.proto:100-123`).
Messages are additive; domain-message reserves and shared-message extensions
follow [wire conventions sections 1.3 and 1.4](serving-wire-conventions.md#13-field-number-allocation-c1).
Never reserve or reuse an occupied field.
Existing `GraphChangeOperation` fields 1-18 and enum values 0-9 stay untouched.

```proto
message GraphTemporalBounds {
  optional int64 valid_from_ms = 1;
  optional int64 valid_to_ms = 2;
  reserved 100 to 199;
}
message GraphTemporalSegment {
  GraphTemporalBounds bounds = 1;
  map<string, MetadataValue> properties = 2;
  reserved 100 to 199;
}
message GraphTemporalSelector {
  optional int64 as_of_ms = 1;
  reserved 100 to 199;
}
message GraphTemporalContext {
  int64 as_of_ms = 1;
  uint64 ttl_clock_ms = 2;
  reserved 100 to 199;
}
message GraphTemporalEntitySelector {
  oneof kind { uint64 id = 1; string key = 2; }
  reserved 100 to 199;
}
enum GraphTemporalWriteKind {
  TEMPORAL_WRITE_UNSPECIFIED = 0;
  TEMPORAL_WRITE_CREATE_EDGE = 1;
  TEMPORAL_WRITE_PROMOTE_EDGE = 2;
  TEMPORAL_WRITE_APPEND_SEGMENT = 3;
  TEMPORAL_WRITE_REPLACE_SEGMENT = 4;
  TEMPORAL_WRITE_CLOSE_SEGMENT = 5;
  TEMPORAL_WRITE_REMOVE_SEGMENT = 6;
  reserved 7 to 19;
}
message GraphTemporalWriteOperation {
  GraphTemporalWriteKind kind = 1;
  GraphTemporalEntitySelector edge = 2;
  Expected expected = 3;
  optional string key = 4;
  string source = 5;
  string destination = 6;
  string relationship = 7;
  repeated GraphTemporalSegment segments = 8;
  GraphTemporalBounds old_bounds = 9;
  GraphTemporalSegment segment = 10;
  optional int64 at_ms = 11;
  reserved 100 to 199;
}
message GraphTemporalTimelineRequest {
  string graph = 1;
  GraphTemporalEntitySelector edge = 2;
  optional uint32 start_index = 3;
  optional uint32 end_index = 4;
  reserved 100 to 199;
}
message GraphTemporalTimeline {
  uint64 edge_id = 1;
  uint64 revision = 2;
  uint32 start_index = 3;
  uint32 end_index = 4;
  uint32 total_segments = 5;
  repeated GraphTemporalSegment segments = 6;
  reserved 100 to 199;
}
message GraphTemporalTimelineResponse {
  string graph = 1;
  GraphReadMeta meta = 2;
  GraphTemporalTimeline timeline = 3;
  reserved 100 to 199;
}
enum GraphTemporalChangeKind {
  TEMPORAL_CHANGE_UNSPECIFIED = 0;
  TEMPORAL_CHANGE_EDGE_PUT = 1;
  TEMPORAL_CHANGE_SEGMENT_APPENDED = 2;
  TEMPORAL_CHANGE_SEGMENT_REPLACED = 3;
  TEMPORAL_CHANGE_SEGMENT_CLOSED = 4;
  TEMPORAL_CHANGE_SEGMENT_REMOVED = 5;
  reserved 6 to 19;
}
message GraphTemporalChangeOperation {
  GraphTemporalChangeKind kind = 1;
  uint64 id = 2;
  optional string key = 3;
  optional uint64 source = 4;
  string source_key = 5;
  string relationship = 6;
  optional uint64 destination = 7;
  string destination_key = 8;
  repeated GraphTemporalSegment segments = 9;
  optional bool created = 10;
  GraphTemporalBounds old_bounds = 11;
  GraphTemporalSegment segment = 12;
  optional int64 at_ms = 13;
  reserved 100 to 199;
}
```

Exact integration into the proposed Gap 12 service/messages and existing feeds:

```proto
// Append inside service vlite.v1.Graph; retain all methods already landed.
rpc EdgeTimeline(GraphTemporalTimelineRequest) returns (GraphTemporalTimelineResponse);
// Append to each Gap 12 request named in the read operation table:
// GetNodeRequest, GetEdgeRequest, FindNodesRequest, NeighborsRequest,
// TraverseRequest, ShortestPathRequest, ExplainRequest, RetrieveRequest,
// QueryTextRequest:
GraphTemporalSelector temporal = 200;
// Append once to shared GraphReadMeta (covers those read responses):
GraphTemporalContext temporal = 200;
// Append to shared Gap 12 GraphEdge; absent = legacy, present empty = all-time:
GraphTemporalBounds validity = 200;
// Append as a new member of GraphWriteOperation's operation oneof:
GraphTemporalWriteOperation temporal = 200;
// Append to existing GraphChangeOp:
GRAPH_CHANGE_OP_TEMPORAL = 10;
// Append to existing GraphChangeOperation, not a second event stream:
GraphTemporalChangeOperation temporal = 23;
// Shared GraphEventFeature enum and feed fields land in the preceding TTL slice.
// Consume that enum's TTL and TEMPORAL values; do not redeclare it here.
// ChangesRequest:
repeated GraphEventFeature graph_events = 3;
// WatchChangesRequest (tag 3 remains poll_interval_ms):
repeated GraphEventFeature graph_events = 4;
// ChangesResponse (echo):
repeated GraphEventFeature graph_events = 5;
```

Proposed D3 allocations follow [wire conventions section 1.3, decision 1](serving-wire-conventions.md#13-field-number-allocation-c1):
carve tag 200 out of the temporal fence in each shared message above, leaving
`reserved 201 to 299`; retain the other sibling fences.
Gap 12 must fix the shared GraphEdge/GraphReadMeta and operation-oneof names
before implementation and publish one combined descriptor. The descriptor test
adds named entries for each tag-200 extension, `GraphChangeOperation.temporal`
tag 23 and `GRAPH_CHANGE_OP_TEMPORAL` value 10; the TTL slice owns the shared feed
fields and change tags 19-22. These additive changes amend
`docs/projects/roadmap/serving-contract.md:206` ("keep GraphChangeOperation and
existing stream messages unchanged"): owner decision D3.
Proto generated JSON mapping uses int64 decimal strings; binary wire uses
ordinary protobuf int64 presence, never the storage presence byte.
JSON `Segment` is flat (`valid_from_ms`, `valid_to_ms`, `properties`), while
proto `GraphTemporalSegment` nests `bounds`; the proto binding converts. A timeline
request has no read options; absent indices mean 0 and 256, and `graph` and
`edge` are required. Section 3's zero-work envelope (complete, empty truncation,
zero stats) travels in `meta` (field manifest: D3).
Required selectors/messages and kind-dependent fields follow the JSON write/feed
tables; missing edge/expected/old_bounds/segment/at, kind 0 and inapplicable
nondefault fields are INVALID_ARGUMENT. Empty GraphTemporalBounds means open/open,
not unspecified old bounds; every GraphTemporalSegment carries its bounds message
(empty for all-time). Empty segments is legal on create/put. Zero ID, time,
index and false-created values retain presence where optional. Timeline absent
means JSON null.

Every new public Rust API needs its own `site/api.html` row and matching
`data-api` key; do not publish proposed private names prematurely.
The scanner's public-visibility and server/bin exclusions are verified at
`tests/api_reference.py:9-28`. Include REST/gRPC/MCP and new client/binding
reference updates in their implementation slices as applicable.

## 8. Independent acceptance and delivery

Extend the independent
[interval oracle](../../../tests/temporal_interval_oracle.py) and
[read oracle](../../../tests/temporal_read_oracle.py), not a copy of the Rust
implementation, with wire-normalization, whole-series revision and feed-order
models. Retain the existing
[interval goldens](../../../tests/fixtures/temporal_intervals/golden.json) and
[read cases](../../../tests/fixtures/temporal_intervals/read-cases.json);
add hand-authored request/response and binary-proto goldens at implementation.
Their source-of-truth role is logical semantics, not proof of persistence/auth.

| Matrix | Required independent assertions |
|---|---|
| Times/intervals | from-1/from/to-1/to, both infinities, all-time versus legacy, negatives/i64 extremes, null versus absent, rejected float/unsafe JS number/out-of-range, gaps/touching/overlap, 256 accepted/257 atomic refusal. |
| Writes | All six variants; promotion; identical keyed retry/conflict; exact old bounds; stale expected; close no-op/no event; empty retained series; full-series replacement; same-batch supersession; mixed vector/TTL final-op failure leaves everything unchanged. |
| Reads | REST/gRPC/MCP/CLI/client equality for get/neighbors/traverse/shortest-path/timeline; hidden endpoints; TTL C before/equal/after independent of T; post-purge nonresurrection; current node metadata/evidence limitations. |
| Certification | Invalid-at-T edges charged; each work/output cap; deadline/cancel at admission/mid-execution/serialization; shortest-path never provisional; no transport truncation masquerading as complete. |
| Paging/timeline | Captured default T/C, conflicting override, stale/cross-graph cursor, gap null get, empty timeline, legacy refusal, 0/0 and 256/256, start>end and 257, range past length, changed revision between ranges. |
| Feed | Five correction payloads, promotion false, ordered mixed commit, backfill, no boundary event, no-op absent, feature-set echo/old-server refusal, `vlc1`/`vlc2` and wrong-set `cursor_mode_mismatch`, old subscription blocked, signature exact bytes, per-set dispatcher progress, whole temporal commits and no advancement on oversized/failed commit. |
| Persistence | Plain/encrypted reopen, directory checkpoint, compact image, activation copy; checkpoint floor versus lineage mismatch; revisions reacquired; no synthetic history; temporal-only feed's TTL omission explicitly visible in capability docs. |
| Authorization | No auth, revoked/expired/read-only, graph binding, collection wildcard without graph scopes, tenant physical-name bypass, forbidden existence, mixed collection writes/pivot/evidence grants; REST/gRPC/proxied MCP parity. |
| Compatibility/refusal | Old generated client/new server and reverse; old file exact legacy goldens; no automatic capability on write/open; empty flag-64 file refused; ordinary/versioned feed negotiation; unsupported binding/CLI/dashboard path; unknown enums handled without success fallback. |
| Schemas/errors | OpenAPI validation including the six `op` enum literals and single temporal `allOf` branch, generated-client descriptor/golden checks for shared tag 200, change tag 23/enum 10 and feed tags 3/4/5, every error code/details trailer and scoped feature/category, transport expiry/cancel per wire conventions, every default/ceiling/ceiling+1, 1/8/16 MiB boundaries, all relative links/API-reference keys. |

Mutation list: flip inclusive start/exclusive end; map null to zero; round i64
through f64; collapse open/open into legacy; select latest properties in a gap;
resample C on continuation; remove endpoint TTL; stop charging filtered entries;
check current instead of batch-start revision; silently merge touching segments;
accept segment 257; serialize exhausted as no-path; drop an unknown feed op;
emit at a validity boundary; advance half a commit; retain old cursor across
lineage; let collection wildcard grant graph scope. Each mutation must make an
independent named case fail before the corresponding slice is accepted.

The following are proposed file ownership boundaries, not files changed here.
Every slice waits for qualified public Rust C reads; gRPC-related work also waits
for the reviewed Gap 12 shared service/types. Landing order: `gap4e-rest`, then
`gap4e-grpc` and `gap4e-mcp-cli`, then `gap4e-clients`. Integration must not
bypass gates. Activation tooling (D7) belongs to none of these slices.

| Slice | Files owned | Required tests and independent acceptance checklist |
|---|---|---|
| `gap4e-rest` | `src/server/graph.rs`, `src/server/features.rs`, `src/server/webhooks.rs`, `src/bin/tool_support/graph_json.rs` (shared by REST, `vlite-mcp` and `vlite-graph-cli`: `src/server/graph.rs:75-77`, `src/bin/vlite-mcp.rs:1`, `src/bin/vlite-graph-cli.rs:17`), `ApiError`/auth/error-mapping glue in `src/server.rs`, approved `src/openapi_graph.json`/`src/openapi.json`, relevant server tests/docs | At wire-conventions step 6 after TTL: oracle/wire goldens; all routes/auth/limits; shared `graph_events`/`vlc2` consumption and per-set webhook progress; scoped handler-built errors; single temporal `GraphBatch` branch with root components; refusal at empty flag-64 file; descriptor-free REST schema review; source unchanged by activation; old-file goldens. Accept only with independently reproduced negative cases and no gate changes. |
| `gap4e-grpc` | `proto/vlite/v1/vlite.proto`, `src/server/grpc.rs`, graph service implementation/tests, generated descriptor fixtures, `site/api.html` rows | After Gap 12 and TTL: named descriptor additions for shared tag 200 and change tag 23/enum 10; consume feed tags 3/4/5; all unary parity, stream feature-set echo/error details, no unknown-op downgrade, old descriptor compatibility, transport cancellation/whole-commit cap per wire conventions. Accept only with independent binary goldens and REST-equivalent authorization. |
| `gap4e-mcp-cli` | `src/bin/tool_support/graph_mcp.rs`, `graph_proxy.rs`, `src/bin/vlite-mcp.rs`, `src/bin/vlite-graph-cli.rs`, command/tool tests and reference docs; consumes `graph_json.rs` from `gap4e-rest` | Stdin exact int parsing, tool-schema validation, writable/read-only listing/calls, local/proxy parity, stderr/exit/open refusal, timeline and temporal feed, cancellation. Accept only after independent malformed-input and no-silent-fallback cases. |
| `gap4e-clients` | `clients/python/vlite_client/__init__.py`, `clients/javascript/index.js`/`index.d.ts`, `clients/dotnet/VLite.Client/Client.cs` and model files/tests/docs; embedded binding files only in a separately qualified sub-slice | Lossless language types, all six operation builders, HTTP/gRPC fixture parity, safe timestamps, cancellation/error fields, old-server behavior; run binding gate tests unchanged until explicit support. Accept only with independent wire captures and API-reference completeness. |

Each slice also runs the documentation checks listed below and relevant existing
test runners; fixture/reference acceptance is not production activation.
Dashboard stays without temporal controls in this slice. Once a qualified server
can open a temporal file, dashboard ordinary graph reads must inherit fixed-T/C
visibility and show explicit unsupported errors rather than attempt conversion.

## 9. Owner decisions and open items

These are schema/qualification decisions, not requests to change already chosen
copy-out activation, explicit opt-in, caps or validity semantics.
Original decision options and recommendations are retained below; text marked
"Formerly" is historical, not the current proposal. Appended wire-conventions
annotations propose replacements and remain open for owner ratification.

| Decision | Options | Recommendation / blocking consequence |
|---|---|---|
| D1: signed JSON time and ids | Decimal strings with exact integer input; integers only; formatted dates | Approve: times take strings or integers within ±(2^53-1), larger integers refused; ids/revisions keep today's string-or-integer input (`src/server/graph.rs:4-8`), clients send strings. Blocks serializer/client goldens. |
| D2: feed negotiation | Explicit existing-route opt-in plus per-subscription version; new feed endpoints; implicit new vocabulary | Formerly: Approve `temporal=true`/`temporal_v1`. Legacy feeds refuse temporal files before any event, and a client that asked for temporal fails `unsupported_feature` without `feed_version` (cites in section 6). Blocks feed and activation acceptance. **Withdrawn:** separate temporal negotiation; the shared feature set and echo replace it. Proposed replacement: section 6 `graph_events=temporal`. [Wire conventions, decision 8 (owner-selected)](serving-wire-conventions.md#6-owner-decisions-23). |
| D3: Gap 12 tag manifest | Formerly: Shared tag-100 extension points; allocate different reviewed tags; JSON-only RPC payloads; new stream messages | Formerly: Approve these typed extensions after resolving Gap 12 shared names and the GraphReadMeta field manifest. One combined descriptor review is required; never guess occupied tags. Tag/enum 100 on the existing change messages amends `serving-contract.md:206`; approve that explicitly. **Withdrawn:** tag-100 temporal allocation conflicts with the ALG band; propose section 7's shared tag 200, change tag 23/enum 10 and shared feed fields. [Wire conventions, decision 1 (owner-selected)](serving-wire-conventions.md#6-owner-decisions-23). |
| D4: remote TTL clock | Server-owned C only; explicit admin clock override | Keep server-owned C and return it, never allow a remote caller to resurrect retained-but-expired data. Blocks read request schema approval. |
| D5: activation readiness | Qualify all serving/feed surfaces before acceptance; permit explicitly unsupported bindings to remain closed | Require complete server/feed support before server acceptance; keep individual bindings closed until separately qualified. No per-request weakening and no gate/default changes here. |
| D6: wire failure/deadline | Explicit transport errors plus bounded incomplete read envelopes; always discard budget results | Approve section 3 precedence/caps/error codes; graph budget can report incomplete, transport cancellation/deadline cannot report success. gRPC deadlines come from `grpc-timeout`; REST has no client deadline and passes `cancel: None` (section 3), so REST propagation is NEW. Blocks parity/error goldens. [Wire conventions, decision 4 (owner-selected)](serving-wire-conventions.md#6-owner-decisions-23); transport expiry/cancel follows its section 2.2. |
| D7: public activation API and CLI | Keep `enable_temporal` crate-private; public `maintenance::enable_temporal` plus `vlite-maintain enable-temporal`; library only | After activation qualification, add both. Encrypted sources: `--key-env NAME` (never a key argument) or plaintext only. A public Rust API needs its own `site/api.html` row and is not authorized here (`gap4-temporal-graph.md:500`; section 4 cites the rest). |
| D8: `UnsupportedFeature` REST mapping | Flag 64 only; every flag; keep 500 | Formerly: Flag 64 only: 409 `unsupported_feature` with NEW `ApiError` members (`feature` body field, `vlite-feature` trailer); other flags keep today's 500 (`src/server.rs:1624`). No new public `Error` variant. Blocks refusal goldens. **Withdrawn:** per-draft members/trailer would fork error context; propose scoped handler-built `ApiError` with `details.feature` and `details.category` under section 3, preserving legacy routes. [Wire conventions, decision 7 (owner-selected)](serving-wire-conventions.md#6-owner-decisions-23); Wire conventions, decision 21 (owner-selected). |
| D9: new wire codes | Formerly: `request_too_large`, `response_too_large`, `graph_timeout`, `graph_cancelled`; reuse 413 `invalid_json`, 503 `busy`; none | Formerly: Provisional. Today 504 maps to INTERNAL and 408/413 to INVALID_ARGUMENT (`src/server/grpc.rs:181-197`); the vocabulary is settled in the cross-document wire review before any REST/gRPC golden. **Withdrawn:** the domain-specific request/deadline/cancel vocabulary conflicts with the shared registry. Proposed replacement: section 3 and [wire conventions section 2](serving-wire-conventions.md#2-error-registry-c2-c9), including `response_too_large` to RESOURCE_EXHAUSTED and explicit `deadline_exceeded` to DEADLINE_EXCEEDED, with no wire cancellation code. Wire conventions, decision 4 (owner-selected). |
| D10: wire caps | Temporal-only 1/8/16 MiB; uniform for all graph requests; none | Temporal-only. REST has no response cap today and an 8-16 MiB write the engine accepts is refused by transport (cites in section 3). [Wire conventions, decision 3 (owner-selected)](serving-wire-conventions.md#6-owner-decisions-23). |
| D11: webhook migration | Formerly: Shared cursor and legacy key; per-subscription progress with `feed_version`; separate temporal dispatcher | Formerly: Per-subscription progress; dedup key unchanged, a lineage key needs a new payload field. Contradicts `docs/WEBHOOKS.md:45,52`; `deny_unknown_fields` blocks downgrade (cites in section 6). Needs its own qualification before activation serving. **Withdrawn:** separate versioned progress duplicates cursor ownership; propose section 6's immutable feature sets and per-set dispatcher progress, preserving the empty-set path/format. [Wire conventions, decision 8 (owner-selected)](serving-wire-conventions.md#6-owner-decisions-23). |
| D12: audit and metrics | Section 4 fields and labels; existing fields only; none | Approve the NEW fields and bounded labels (cites in section 4); never record keys, bounds, properties or secrets. |
| D13: NEW tools, CLI and client methods | All NEW; REST/gRPC only | Approve MCP `vlite_graph_edge_timeline`/`vlite_temporal_changes`, CLI `edge-timeline`/`temporal-changes` and the HTTP graph accessors (none exist today, section 1); each is documented in its slice. |

Open implementation items: review schema references against the approved Gap 12
descriptor; verify batch no-op outcomes, full-series byte accounting, response
preflight and per-transport cancellation; qualify webhook migration (D11);
publish a support/refusal matrix before release. Nothing here implies production
readiness. Documentation checks: `python tests/release_docs.py`,
`python tests/content_budget.py`, `python tests/agent_docs.py`,
`git diff --check`.
