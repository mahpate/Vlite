# Gap 5 S5: bounded graph algorithm serving contract

Status: candidate for independent acceptance, based on `00babff460c58b0d80f179de79cfcc3e50c0ce57`.
Design only; no serving implementation or schema activation is authorized by this file.
S1-S4 are embedded, reviewed and unreleased [A1]. S5 needs separate acceptance.
The [owner's wire decision](next-major.md#decided-2026-11-02) requires exact schemas
and review before code. This candidate supplies those schemas, not a release gate waiver.
Open means not decided or not verified. All unqualified requirements below are proposed.
This document is conformed to the [serving wire conventions proposal](serving-wire-conventions.md), whose owner decisions remain open.

## 1. Scope and requirements

Expose exactly five read-only algorithms over one explicit local projection:
degree, weighted shortest path, PageRank, label propagation and sampled betweenness.
Preserve the accepted [Gap 5 semantics](gap5-graph-algorithms.md), including its
implemented corrections, and the [Gap 12 serving contract](serving-contract.md#gap-12-graph-rpc-parity).
The [graph guide](../../GRAPH.md#bounded-local-algorithms-rust-unreleased) remains the embedded usage reference.

Non-goals: new algorithms, whole-graph/offline jobs, query-text syntax, persisted
scores, graph writes, vector or attachment retrieval, historical/as-of reads,
streaming, continuation cursors, bindings, provider calls, storage changes,
benchmark claims, changed defaults, policies or release criteria.
`src/openapi.json`, `src/openapi_graph.json` and the proto remain frozen in this slice.

1. **S5-R1:** Every operation in section 3 maps once to `GraphRef::algorithm`;
   the adapter must not reimplement computation, weights, sampling or projection.
2. **S5-R2:** All transports preserve section 4's budgets, validation precedence,
   nullable outputs, first stopping reason and last-full-step provenance.
3. **S5-R3:** REST, gRPC and proxied MCP authorize `graph_read`, tenant and graph
   binding before lookup. The CLI opens read-only; MCP uses the session's existing
   handle (read-only by default), and the new tools never mutate it.
4. **S5-R4:** A response uses one graph borrow, sequence and TTL read clock;
   no adapter retry may combine results from different views.
5. **S5-R5:** Schemas reject unknown JSON fields, ambiguous selectors, wrong
   algorithm parameters, invalid numbers and over-ceiling limits; never clamp.
6. **S5-R6:** NoPath is certified only after complete extraction/computation.
   Partial analytics are never computed over an extracted prefix.
7. **S5-R7:** Five REST operations, five unary RPCs, five MCP tools and five CLI
   commands have identical normalized JSON results, apart from elapsed time.
8. **S5-R8:** Independent oracle, mutation, auth and cancellation cases in section 8
   must pass before each implementation slice is independently accepted.

## 2. Current coverage and evidence

Evidence refers to this checkout, not to line numbers copied from older audits.
Absence means absent from the cited dispatcher/service, not from every possible client.

| Capability | Embedded Rust | REST | gRPC | MCP | CLI |
|---|---|---|---|---|---|
| Degree | `GraphAlgorithm::Degree` [A2] | Not dispatched [W1] | No graph algorithm RPC [W3] | No tool [W4] | No command [W5] |
| Weighted path | `WeightedShortestPath` [A2] | Only ordinary shortest path [W1] | No RPC [W3] | Only ordinary shortest path [W4] | Only ordinary shortest path [W5] |
| PageRank | `PageRank` [A2] | No route [W1] | No RPC [W3] | No tool [W4] | No command [W5] |
| Label propagation | `LabelPropagation` [A2] | No route [W1] | No RPC [W3] | No tool [W4] | No command [W5] |
| Sampled betweenness | `Betweenness` [A2] | No route [W1] | No RPC [W3] | No tool [W4] | No command [W5] |
| Limits/results/errors | Typed kernel/adapter [A3-A6] | Existing read JSON, not algorithm JSON [W2] | Existing error mapping [E2] | Existing compact JSON/error wrapper [W4, E3] | Existing file/JSON convention [W5] |

- **A1:** `docs/GRAPH.md:129-136` records accepted embedded-only, unreleased coverage.
- **A2:** `src/property_graph/algorithms.rs:106-157` defines all five variants;
  `src/property_graph/algorithms.rs:374-400` is the public entry point.
- **A3:** `src/property_graph/algorithms.rs:187-260` defines rows, path outcomes,
  outputs and truncation; `src/property_graph/algorithms.rs:264-317` defines stats and envelope.
- **A4:** `src/property_graph/algorithms.rs:325-361` defines existing algorithm errors and display text.
- **A5:** `src/property_graph/engine/limits.rs:10-40,65-161` defines model/read limits;
  `src/property_graph/engine/algorithm.rs:76-179` defines algorithm constants/caps and validation.
- **A6:** `src/property_graph/algorithms.rs:7-46` specifies visibility, metered
  preparation, partial steps/sources, output ordering and no cursors.
- **W1:** `src/server/graph.rs:123-184` lists the REST routes and read permission classifier.
- **W2:** `src/openapi_graph.json:863-955` describes an existing typed read;
  `src/bin/tool_support/graph_json.rs:464-495,775-790,852-889` defines limits,
  truncation, selectors and conditions. `src/server/graph.rs:1757-1787`
  merges fragment paths only, not fragment components.
- **W3:** `proto/vlite/v1/vlite.proto:1-60` defines transport caps and the existing VLite service;
  `proto/vlite/v1/vlite.proto:100-108,222-237` defines reusable MetadataValue and Condition.
- **W4:** `src/bin/tool_support/graph_mcp.rs:1-12,34-76,119-136,152-282`
  defines existing tool dispatch and local execution; its ordinary reads currently pass `cancel: None`.
- **W5:** `src/bin/vlite-graph-cli.rs:1-12,33-82,188-207` defines read-only file
  execution, decimal IDs, commands and rejection of `--writable` for reads.
- **W6:** `src/bin/tool_support/graph_proxy.rs:1-15,77-127,164-203` forwards
  graph tools to REST with bearer identity and no local file; its dispatcher
  needs an explicit algorithm branch, not the ordinary read-route fallback.
- **E1:** `src/server.rs:1476-1480,1528-1535,1595-1655` maps existing engine errors.
  `src/property_graph/convert.rs:147-177` converts invalid graph queries to `Error::InvalidQuery`.
- **E2:** `src/server/grpc.rs:181-215` maps HTTP status/code to gRPC status and trailers.
- **E3:** `src/bin/vlite-mcp.rs:669-681` defines JSON-RPC errors and MCP result wrappers.
- **T1:** `src/server/graph.rs:10-56,317-334` defines route, graph-binding and tenant
  authorization; `src/server/credentials.rs:141-183` defines qualification/ownership.
- **T2:** `src/server/tenants.rs:132-149,170-175,462-479` checks tenant lifecycle
  and qualifies/unqualifies names.
- **O1:** `src/server.rs:1203-1255` records requests, denial audits and sanitized
  metrics; `src/server/graph.rs:217-265` records guard holds and ordinary read truncations.
- **D1:** `tests/api_reference.py:2-24` requires exact `site/api.html` coverage
  for public APIs; `src/openapi.json:195-201` defines shared typed metadata,
  Condition, Error and Unauthorized schemas.

## 3. Exact operation and JSON contract

All five routes are `POST /v1/graphs/{graph}/algorithms/<suffix>`, use `graph_read`,
and return HTTP 200 for a valid execution, including exhaustion.
Like existing graph reads, they do not enforce Content-Type: the body is a UTF-8
JSON object; empty/all-whitespace bodies decode as `{}`, whose missing required
scope yields 422 `invalid_json` (`src/server/graph.rs:344-366`). No 415 is added.
Other statuses are exactly section 5.
There is no GET alias, batch algorithm endpoint or mutation method.
Each new read request is capped at 1,048,576 encoded bytes: UTF-8 JSON for
REST/CLI, `encoded_len()` of the decoded protobuf request for gRPC (prost drops
unknown fields, so they are not counted). MCP size is the UTF-8 byte length of
compact re-serialized `arguments`, excluding the JSON-RPC envelope, measured
after parsing (`src/bin/tool_support/graph_mcp.rs:152-159`).
REST retains the existing graph-read cap (`src/server/graph.rs:79-80,184`).
New CLI commands use this cap, leaving existing commands' caps unchanged.
gRPC retains its service-wide 8 MiB request / 16 MiB response caps [W3];
do not lower Graph's service cap, which also admits batches.
Algorithm responses on all surfaces have a 16,777,216 encoded-byte ceiling:
overflow fails explicitly as `invalid_input` rather than cutting serialized JSON.
This is an output safety limit, not a measurement.

| REST suffix / operationId | `vlite.v1.Graph` unary method | MCP tool | CLI command |
|---|---|---|---|
| `degree` / `GraphAlgorithmDegree` | `AlgorithmDegree` | `vlite_graph_degree` | `degree` |
| `weighted-shortest-path` / `GraphAlgorithmWeightedShortestPath` | `AlgorithmWeightedShortestPath` | `vlite_graph_weighted_shortest_path` | `weighted-shortest-path` |
| `pagerank` / `GraphAlgorithmPageRank` | `AlgorithmPageRank` | `vlite_graph_pagerank` | `pagerank` |
| `label-propagation` / `GraphAlgorithmLabelPropagation` | `AlgorithmLabelPropagation` | `vlite_graph_label_propagation` | `label-propagation` |
| `betweenness` / `GraphAlgorithmBetweenness` | `AlgorithmBetweenness` | `vlite_graph_betweenness` | `betweenness` |

### 3.1 Shared input

All JSON objects are closed; duplicate keys fail. Required common field: `scope`.
Optional `limits` and `algorithm_limits` are objects whose absent fields take section 4 defaults.
Omission, not explicit null, selects an input default (except `weight_property`).
No `cursor`, `projection`, `explain`, `as_of`, collection or vector field is accepted.

```json
{
  "scope": {
    "seeds": [{"key":"a"}],
    "direction": "out",
    "relationship_types": {"only":["LINK"]},
    "node_labels": {"any":true},
    "edge_conditions": []
  },
  "limits": {"max_hops":2},
  "algorithm_limits": {"max_vertex_operations":100000}
}
```

`scope.seeds`, `direction`, `relationship_types` and `node_labels` are required.
`edge_conditions` defaults to `[]`. Direction is exactly `out|in|both` and has no default.
Relationship scope is exactly `{"any":true}` or `{"only":[type,...]}`;
label scope is exactly `{"any":true}` or `{"all":[label,...]}`.
False, both branches, empty Only/All or omitted filters are invalid.
Lists have the raw lengths in section 4 before deduplication.

A selector is exactly `{"id":U64Input}` or `{"key":string}`. Keys are 1..256 UTF-8
bytes; type/label names match `[A-Za-z0-9_]{1,63}`; graph names match
`[a-z][a-z0-9_-]{0,62}` [A5]. `U64Input` is a canonical unsigned decimal string
(`0|[1-9][0-9]*`, <=18446744073709551615) or an exactly parsed JSON integer in
that range, never a float. ID zero is valid. Clients must not round via binary64.
Every output ID, sequence, TTL timestamp and sampling seed is a decimal string.
Bounded counts, milliseconds, degrees, iterations, caps and stats are JSON integers;
algorithm_version is integer 1. Scores, residual, mass and cost are finite binary64.

Conditions are the existing typed Condition [W2, D1], at most 64:
`{"op":"eq","field":string,"value":MetadataValue}`,
`{"op":"range","field":string,"min":MetadataValue|null,"max":MetadataValue|null}`,
or `{"op":"georadius","field":string,"latitude":number,"longitude":number,"radius_meters":number}`.
Range ends may be omitted; at least one must be non-null, both numeric ends must
have the same type and min<=max (inclusive). Field names are 1..256 UTF-8 bytes.
MetadataValue is `{"type":"string|integer|float|boolean|geopoint","value":...}`;
integer values are canonical signed i64 strings, strings <=4096 UTF-8 bytes,
floats finite, geopoints `{latitude,longitude}` in [-90,90]/[-180,180].
Radius is finite and nonnegative. Preserve existing filter semantics, not raw JSON
number inference (`src/metadata.rs:15-105`; [D1]).

### 3.2 Algorithm-specific request and output

Merge the following fields into the common input object, not into `scope`.
No extra algorithm field is accepted. Degree and label propagation accept no
extra fields. Each response has the envelope in 3.3 and exactly its output kind.

| Algorithm | Additional input fields | Exact non-null `output` JSON shape |
|---|---|---|
| Degree | None | `{"kind":"degree","rows":[{"node_id":"1","in_degree":0,"out_degree":1,"incident_degree":1}]}` |
| Weighted shortest path | Required `source:Selector`, `destination:Selector`; optional `weight_property:string|null`, default null | `{"kind":"weighted_shortest_path","outcome":"found","cost":2.0,"node_ids":["1","2"],"edge_ids":["7"]}` or `{"kind":"weighted_shortest_path","outcome":"no_path_within_scope_and_hops"}` |
| PageRank | Optional `damping:number` default 0.85, `tolerance:number` default 1e-8, `personalization:[{"node":Selector,"mass":number}]`, `weight_property:string|null` default null | `{"kind":"pagerank","scores":[{"node_id":"1","score":0.5}],"iterations":1,"residual":0.0,"converged":true}` |
| Label propagation | None | `{"kind":"label_propagation","assignments":[{"node_id":"1","community_id":"1"}],"sweeps":1,"converged":true}` |
| Betweenness | Required `sampling_seed:U64Input`; no implicit random seed | `{"kind":"betweenness","scores":[{"node_id":"1","score":0.0}],"selected_sources":["1"],"completed_sources":["1"],"completed_source_count":1,"estimated":false}` |

These are shapes, not a common fixture with fabricated counts.
`weight_property` is 1..256 UTF-8 bytes. Damping and tolerance are finite and
strictly between 0 and 1. Personalization, when present, has 1..10,000 entries,
positive finite masses and distinct resolved nodes; each entry's resolved node
must be a scope seed, but entries need not cover every seed
(`src/property_graph/engine/algorithm/iterative.rs:69-78`). An empty list
does not mean uniform. Omitted personalization means uniform. Do not accept
weights for degree, label propagation or betweenness [A2, A4].

Semantics are algorithm version 1 [A2-A6]:

- **Degree:** retained physical edges count separately; loops count once in
  each of in/out/incident. In reverses endpoints; Both keeps stored orientation.
- **Path:** source must be a seed. Use at most H edges. Compare exact sums of
  binary64 weights, then fewer hops, then lexicographic numeric edge IDs.
  Round the winning exact sum once (nearest, ties-even), not at every addition.
  Unit weights when no property is named; otherwise validate every retained edge:
  missing, negative, nonfinite, nonnumeric or integer >2^53 fails; normalize -0.
  Source=destination gives cost 0. A visible destination outside the projection
  is NoPath; missing/due endpoints are errors. No path is emitted on exhaustion.
- **PageRank:** start from normalized teleport masses, redistribute dangling
  mass through teleport, use synchronous ordered binary64 steps and L1 stopping.
  Parallel arcs contribute weights, self-loops count; zero-out-weight nodes dangle.
  The edge budget covers every pass. Only full steps yield scores.
- **Label propagation:** after directional extraction, symmetrize retained edges,
  collapse parallel neighbor pairs, ignore loops. Visit nodes ascending, update
  in place, keep a tied current label, otherwise choose the smallest winning label.
  Canonical community IDs are minimum member IDs. Only full sweeps yield assignments.
- **Betweenness:** unweighted Brandes on directed arcs (Both symmetric), collapse
  parallel arcs, ignore loops and exclude endpoints. Rank sources by
  `(SplitMix64-v1(sampling_seed XOR node_id),node_id)`; use the first min(samples,n).
  For n>2 score is `(n/k)*sum_dependency/((n-1)*(n-2))`; do not halve Both.
  For n<=2 scores are zero. `estimated` iff completed k<n, even on a complete run.
  `selected_sources` is in sampling order; `completed_sources` is its completed
  prefix. Never include a partial source or claim a confidence interval.

Rows always ascend by numeric node ID, not score or decimal string order.
Path arrays stay in traversal order. Scope normalization sorts/deduplicates IDs,
types and labels; PageRank echo uses effective damping/tolerance and ascending
personalization IDs with original, not normalized, masses [A3, A6].
Fixed reduction order is preserved; floats are not promised cross-platform bitwise
equal. Deadline-dependent step counts and elapsed time are not deterministic.

### 3.3 Response, partial work and non-pagination

```json
{
  "graph":"g",
  "output":{"kind":"degree","rows":[]},
  "complete":false,
  "truncation":["output_limit"],
  "scope_complete":true,
  "computation_complete":true,
  "view_sequence":"42",
  "ttl_clock_ms":"1793592000000",
  "algorithm_version":1,
  "request":{
    "scope":{"seeds":[{"id":"1"}],"direction":"out","relationship_types":{"only":["LINK"]},"node_labels":{"any":true},"edge_conditions":[]},
    "algorithm":{"kind":"degree"}
  },
  "max_hops":2,
  "stats":{"nodes_extracted":1,"edges_extracted":0,"arcs":0,"edges_examined":0,"vertex_operations":12,"scratch_bytes":512,"elapsed_ms":0}
}
```

Illustration: `max_output_nodes:0`; stats here are shape examples, not measured units.
All envelope fields shown are required. `output` and `request` are nullable;
there is no `truncated` boolean or cursor. Echo is `{scope,algorithm}`, with the
algorithm kind plus all effective parameters from 3.2 (including null weight and
omitted uniform personalization); selectors in an echo contain ID strings only.
Limits are not part of this embedded request echo; `max_hops` is reported separately [A3].

`complete` iff `truncation` is empty; it has length 0 or 1. Reasons are
`visited_limit|edge_limit|vertex_limit|memory_limit|output_limit|deadline|cancelled|iteration_limit`
or `{"degree_limit":{"node_ids":[U64String]}}` (one stopping node for this kernel).
`scope_complete` certifies extraction, not global coverage. `computation_complete`
certifies the whole computation, including convergence for iterative algorithms
or all selected sources for betweenness, before output caps.

Extraction interruption always returns null output and null echo. Degree/path
interruption also withholds uncertified output; a path beyond an output cap is
withheld even though computation finished. Iterative/sampled interruption after
a full step/source retains its last full output and echo, with computation_complete=false;
before the first it returns neither. A final deadline/cancellation check may
retain an already committed iterative/sample output but makes it incomplete.
Echo may be present with null path output after OutputLimit: do not infer echo
presence solely from output presence. Preserve the adapter's actual flags [A3, A6].

Rows are cut to max_output_nodes only after computation; betweenness source
lists are never cut. A found path must fit node, edge and path caps; NoPath
needs no output path slot. If computation already stopped, its first reason
wins even when rows were also cut. No second request can resume this view:
there are no algorithm pages/cursors, and retry recomputes on a new view.

### 3.4 MCP and CLI binding

For each tool in section 3, `inputSchema` is exactly its section 7 request schema
with required `graph:string` added (the same graph-name validation); references
are expanded into a self-contained tool schema, including Condition/MetadataValue.
Do not declare `outputSchema`: errors retain `structuredContent:{"error":message}`
[E3], and existing protocol negotiation includes older versions
(`src/bin/vlite-mcp.rs:21-22`). Success still returns the envelope as
structuredContent plus text; the oracle validates it against section 7.
Annotations: `readOnlyHint:true, destructiveHint:false, idempotentHint:true`.
Idempotence means no writes, not identical results after graph/TTL changes.
Tool invocation is `tools/call {name:<tool>,arguments:{graph:"g",...request}}`.
Only graph-enabled local files advertise the tools, as in existing dispatch [W4].
Proxy mode forwards the authorized REST request, without local reexecution [W6].

CLI invocation for every row is
`vlite-graph-cli --db PATH <command> '<JSON with graph and request fields>'`;
`-` reads the same JSON from stdin. Open with `Database::open_read_only`;
reject `--writable` and `--explain` for these commands. Successful execution
prints exactly one compact envelope on stdout, exit 0 even when incomplete;
errors print `{"error":{"code":CODE,"message":MESSAGE}}` on stderr, exit 1.
Argument/usage failures use `usage`; request decoding failures use `invalid_request`.
Rejecting `--explain`/tool `explain` is new for algorithms: existing typed reads
support explain (`src/bin/tool_support/graph_proxy.rs:164-183`). The proxy's
algorithm branch must reject it rather than forward it to `/explain`.
There is no new HTTP CLI mode. Local file authority and server tenancy differ
explicitly in section 6; result parity does not imply a local security boundary.

## 4. Bounds and cancellation

Defaults/ceilings are inherited, not enlarged [A5]. All five algorithms share
one edge/vertex/scratch meter across preparation, extraction, rescans, validation,
computation and result staging. Repeated/rejected edge visits are charged again.
There is no independent retained-edge allowance: retained physical edges are
bounded by the shared examined-edge budget; Both may create two arcs per edge.

| Field | Default / ceiling | Applicability and exact stop |
|---|---|---|
| `limits.max_hops` | 2 / 4 | All projection radii; path length too. Scope bound, not a truncation. Zero valid. |
| `limits.max_visited_nodes` | 1,000 / 10,000 | All, seeds included; `visited_limit` |
| `limits.max_edges_examined` | 10,000 / 100,000 | All passes/arcs; `edge_limit` |
| `limits.max_edges_per_node` | 1,000 / 10,000 | Extraction adjacency; `degree_limit` with stopping node ID |
| `limits.deadline_ms` | 250 / 5,000 | All, monotonic execution clock; `deadline` |
| `limits.max_output_nodes` | 100 / 1,000 | Degree/scores/assignments and found path nodes; `output_limit` |
| `limits.max_output_edges` | 200 / 2,000 | Found path only; `output_limit` |
| `limits.max_output_paths` | 20 / 100 | Found path only (one maximum result); `output_limit` |
| `algorithm_limits.max_vertex_operations` | 100,000 / 1,000,000 | All; `vertex_limit` |
| `algorithm_limits.max_scratch_bytes` | 67,108,864 / 268,435,456 | All cumulative reservations including output staging; `memory_limit` |
| `algorithm_limits.max_iterations` | 20 / 100 | PageRank/label propagation; `iteration_limit` unless final step converged |
| `algorithm_limits.max_sampled_sources` | 16 / 64 | Betweenness selects min(cap,n); completing sample is not truncation |
| Raw seed selectors | Explicit / 10,000 | All; after dedup distinct seeds <=max_visited_nodes; no implicit seed |
| Relationship types / labels | Explicit / 256 Only entries, 8 All entries | All; Any explicit; raw duplicates count toward caps |
| Edge conditions | 0 / 64 | All; existing filter validation |
| Personalization entries | Uniform / 10,000 | PageRank only, nonempty when specified |

Above-ceiling limits, raw seeds/types and personalization yield
`graph_limit_exceeds_ceiling`. Zero work/algorithm caps, no seeds, too many
distinct seeds for max_visited_nodes, empty Only/All, >8 raw labels and invalid
algorithm parameters yield `invalid_input`. Output caps and H may be zero;
all other caps must be >=1, even if a particular algorithm ignores one [A5].
More than 64 conditions or invalid condition contents use existing InvalidFilter
and `invalid_input`. List-shape and UTF-8/model validation remain metered exactly
as in the embedded API; a wire type error is rejected before that API.
Schema min/max constraints document the valid domain; adapters must not run a
generic schema validator that replaces embedded limit/shape errors or their
precedence with `invalid_json`. Decode types first, then call the typed validator.

Preserve order: transport authentication and bounded decoding; read limits,
algorithm caps, condition shape, request shape; then metered condition conversion,
type/label validation, seed resolution and endpoints/personalization [A6].
After shape checks a pre-set cancellation/exhaustion wins over an item error
that has not yet been reached. Do not resolve selectors in an unmetered adapter.
The engine checks before work, at most every 64 charged operations and before
return. Deadlines are cooperative, never a hard real-time guarantee [A6].

Keep the embedded relative execution deadline, which starts at its meter, distinct
from an end-to-end client transport deadline. A client disconnect/cancellation or
earlier client deadline sets the same cancellation flag; do not extend either
deadline while waiting for a guard. A transport that has already expired cannot
deliver a partial result. If the engine's own deadline expires while the transport
is live, deliver HTTP 200 / gRPC OK / MCP isError=false with `deadline`.
If gRPC itself expires/cancels first, the call ends CANCELLED without an envelope
or trailer, per [wire conventions section 2.2](serving-wire-conventions.md#22-one-reachable-family-oversize-deadline-cancel-busy);
set the flag, stop work cooperatively and release the guard.
Today no served graph read supplies a cancellation flag (`src/server/graph.rs:458,497,609`;
`src/bin/tool_support/graph_mcp.rs:218,271`; `src/bin/vlite-graph-cli.rs:322`).
REST/gRPC slices must add a drop-guard flag around spawn_blocking work, surviving
handler/future drop until the worker observes it. MCP currently ignores
notifications/cancelled and reads stdio sequentially (`src/bin/vlite-mcp.rs:733,798-818`);
its slice must add a reader thread and request-id-to-flag map to receive cancellation
during a call, for local and proxied execution. A live channel returns the incomplete
envelope normally. The CLI installs no signal handler: Ctrl+C ends the process,
without a result guarantee; uninterrupted runs use deadline_ms (ceiling 5,000 ms [A5]).
Add no dependency, unsafe/FFI, cancellation route or job registry.

## 5. Error contract

No new public `Error` variant. Adapt existing `GraphAlgorithmError` directly:
Read delegates to existing Error mappings [E1]; MissingWeight/InvalidWeight/
NumericOverflow retain their existing variants and display messages [A4].
The adapter builds an `ApiError` for each of `graph_missing_weight`,
`graph_invalid_weight` and `graph_numeric_overflow` from the corresponding
existing `GraphAlgorithmError` variant, per
[wire conventions section 7](serving-wire-conventions.md#7-per-document-conformance-checklist);
these are not new engine variants.
Graph-enabled checks and graph binding must precede node/edge information exposure.

| Failure / exact REST code | HTTP | gRPC | MCP/CLI code |
|---|---:|---|---|
| Syntax / shape: `invalid_json` | 400 / 422 | INVALID_ARGUMENT (`invalid_input` for typed-message shape failures) | `invalid_request` for JSON decode/shape |
| Read body exceeds 1 MiB: `invalid_json` | 413 | INVALID_ARGUMENT (decoded request encoded_len cap; `invalid_json` trailer) | `invalid_request` |
| Missing/invalid bearer: `unauthorized` | 401 | UNAUTHENTICATED | Proxy preserves `unauthorized` |
| Missing operation/graph/lineage: `forbidden_scope` | 403 | PERMISSION_DENIED | Proxy preserves `forbidden_scope` |
| Inactive tenant: `tenant_suspended` / `tenant_unavailable` | 403 | PERMISSION_DENIED | Proxy preserves code |
| `GraphsNotEnabled`: `graphs_not_enabled` | 409 | FAILED_PRECONDITION | Same code |
| `GraphNotFound`: `graph_not_found` | 409 | FAILED_PRECONDITION | Same code |
| `GraphNodeNotFound`: `graph_node_not_found` | 409 | FAILED_PRECONDITION | Same code |
| `InvalidGraphName`: `invalid_graph_name` | 422 | INVALID_ARGUMENT | Same code |
| `GraphLimitExceedsCeiling`: `graph_limit_exceeds_ceiling` | 422 | INVALID_ARGUMENT | Same code |
| `InvalidQuery`, `InvalidFilter`, encoded response overflow: `invalid_input` | 422 | INVALID_ARGUMENT | Same code |
| `MissingWeight {edge_id}`: `graph_missing_weight` | 422 | INVALID_ARGUMENT | Same code |
| `InvalidWeight {edge_id,reason}`: `graph_invalid_weight` | 422 | INVALID_ARGUMENT | Same code |
| `NumericOverflow`: `graph_numeric_overflow` | 422 | INVALID_ARGUMENT | Same code |
| Existing admission: `rate_limited` | 429 | RESOURCE_EXHAUSTED | Proxy preserves code |
| Existing admission: `busy` / `unavailable` | 503 | UNAVAILABLE | Proxy preserves code |
| Unexpected internal failure: `internal_error` | 500 | INTERNAL | Same sanitized code |

Shape errors differ only at the transport decoding layer; valid typed requests
must have identical engine error codes across all four surfaces. Do not
reclassify InvalidQuery as graph_invalid_options or missing nodes as HTTP 404.
Reuse existing operational state-error mappings for other Read(Error) failures,
not a blanket catch that hides them [E1, E2].

REST errors use `{"error":{"code":CODE,"message":MESSAGE}}` [D1].
gRPC message is the same sanitized MESSAGE and `vlite-error-code` trailer is CODE,
as today [E2]. The decoded graph-read size check uses the existing `invalid_json`
mapping, with no new status-mapping arm, as proposed in
[wire conventions decision 18](serving-wire-conventions.md#6-owner-decisions-23).
Frames above service decode/encode caps
fail natively with OUT_OF_RANGE and no VLite trailer (locked tonic 0.14.6;
`Cargo.lock:2946-2950`; dependency source `src/codec/decode.rs:189-196` and
`src/codec/encode.rs:186-191`, verified in the cached crate). These are distinct
from the decoded per-method check.
Native deadline/cancellation may also lack application trailers; none are
algorithm truncations.
Retain request IDs and Retry-After on existing admission errors, with no automatic retry.

MCP tool failure preserves the existing wrapper [E3], not a new error object:

```json
{"content":[{"type":"text","text":"graph_missing_weight: graph edge 7 has no weight property"}],"structuredContent":{"error":"graph_missing_weight: graph edge 7 has no weight property"},"isError":true}
```

MCP success wraps the envelope as `structuredContent`, with compact envelope JSON
in `content[0].text` and `isError:false`, including incomplete executions.
Malformed JSON-RPC still uses protocol -32700/-32600/-32602, not an engine error.
CLI error shape is 3.4; semantic errors use this table even though older nonalgorithm
commands may have different local names. Errors contain no weight value, key,
condition contents, payload or secrets. Edge IDs may appear only after authorization.

## 6. Authorization, tenancy and observation

All five POST routes must be added to both `route_op` and `read_post` [W1].
Admin and configured read-only tokens may read; a scoped key must have graph_read,
matching database lineage and a bound graph. `collections:["*"]` never grants
graph_read; graph_read grants no collection read. Tenant keys and signed-in scoped
accounts address local `g`; qualify once to `t{tenant}__g` before graph lookup,
validate ownership/binding, and return local `g` [T1, T2]. A physical name supplied
by a tenant is not an escape into that physical graph. Admin/untenanted tokens
retain stored-name access. Reject inactive tenants before executing.

Authorize before lookup, including missing graphs and every seed/endpoint; do not
probe other graphs for an ID/key. Graph IDs are interpreted only inside the
authorized graph. Scope labels constrain data, never substitute for authorization.
No new graph scope, role or credential default; no result cache, attachment follow,
vector fetch, property projection or cross-tenant/cross-graph seed resolution.
Run under one read guard/GraphRef and sample TTL once; due nodes hide incident edges
at equality. Do not expose a caller-selected TTL clock.

REST and gRPC use the same permission/scope helpers; proxied MCP forwards identity,
request cancellation and errors, never retries via local file access. Local MCP
and CLI have no server credential context [W4, W5]: file access is full authority,
not tenant isolation. The CLI opens read-only; MCP uses the session's existing
handle (read-only by default), including writable sessions, and the new tools
never mutate it (`src/bin/vlite-mcp.rs:95-108,722`). Neither claims graph scopes
are enforced. Do not add a `tenant` input claiming to secure local execution. Test
server authorization parity through proxied MCP and test local non-mutation
separately. Adding remote CLI auth or local access policy is outside S5.

Reuse shared request observation/denial auditing [O1] for REST, gRPC and proxied
MCP, recording each execution once. Attribute proxied calls without double counting.
Extend existing admin graph guard statistics to algorithm reasons (vertex,
memory, iteration included), not a public metrics endpoint. Record operation,
bounded outcome/reason, elapsed/guard duration and charged work; no seed IDs,
properties, personalization, graph-name labels of unbounded cardinality, credentials
or result arrays in logs/metric labels. Local CLI/MCP have no server audit store;
their explicit envelope/stderr is not a fabricated server audit event.

## 7. Exact future OpenAPI and proto additions

These fenced snippets are proposed insertions, not applied changes. Names beginning
GraphAlgorithm are owned by this contract, avoiding unresolved Gap 12 shared-message
tag choices. Keep all existing v1 RPCs, fields, numbers and enums untouched.
Append five RPCs (section 7.2) to whatever `Graph` holds at step 8, after memory,
per the [wire conventions manifest](serving-wire-conventions.md#11-services-all-package-vlitev1-appended-to-protovlitev1vliteproto)
and [delivery order](serving-wire-conventions.md#51-order).
This slice owns only its five RPCs and matching parity/reference rows; it preserves
all earlier additions and promises no absolute method count.
No second Graph service or algorithm service.

### 7.1 OpenAPI overlay

YAML below is a compact spelling of JSON schema objects. Expand anchors/merges
before inserting: `paths` goes only into `src/openapi_graph.json`; `components`
goes only into `src/openapi.json` because the current merger copies paths only [W2].
Reuse existing Condition, Error and BearerAuth [D1]. The overlay defines every new
schema; byte/range/cross-field invariants in sections 3-5 are normative runtime
checks in addition to JSON Schema. `U64Input` integer branches require exact parsing.
For each operation the output kind must match its route, even though the reusable
response schema admits all five kinds. No other base OpenAPI edits are proposed.

```yaml
paths:
  /v1/graphs/{graph}/algorithms/degree:
    parameters: &graphPath
      - {name: graph, in: path, required: true, schema: {type: string, pattern: '^[a-z][a-z0-9_-]{0,62}$'}}
    post:
      operationId: GraphAlgorithmDegree
      security: [{BearerAuth: []}]
      description: Read-only; requires graph_read and graph binding; bounded local scope only.
      requestBody: {required: true, content: {application/json: {schema: {$ref: '#/components/schemas/GraphAlgorithmScopeRequest'}}}}
      responses: &algorithmResponses
        '200': {description: Complete or explicitly incomplete algorithm read, content: {application/json: {schema: {$ref: '#/components/schemas/GraphAlgorithmResponse'}}}}
        '400': {description: invalid_json, content: &errorContent {application/json: {schema: {$ref: '#/components/schemas/Error'}}}}
        '401': {description: unauthorized, headers: {WWW-Authenticate: {schema: {type: string, const: Bearer}}}, content: *errorContent}
        '403': {description: forbidden_scope or tenant_suspended or tenant_unavailable, content: *errorContent}
        '409': {description: graphs_not_enabled or graph_not_found or graph_node_not_found, content: *errorContent}
        '413': {description: invalid_json - request exceeds 1048576 bytes, content: *errorContent}
        '422': {description: invalid_json or invalid_input or invalid_graph_name or graph_limit_exceeds_ceiling or graph_missing_weight or graph_invalid_weight or graph_numeric_overflow, content: *errorContent}
        '429': {description: rate_limited, headers: {Retry-After: {schema: {type: integer, const: 1}}}, content: *errorContent}
        '503': {description: busy or unavailable, content: *errorContent}
        '500': {description: internal_error, content: *errorContent}
  /v1/graphs/{graph}/algorithms/weighted-shortest-path:
    parameters: *graphPath
    post:
      operationId: GraphAlgorithmWeightedShortestPath
      security: [{BearerAuth: []}]
      description: Read-only; graph_read plus graph binding; exact hop-bounded weighted path.
      requestBody: {required: true, content: {application/json: {schema: {$ref: '#/components/schemas/GraphAlgorithmPathRequest'}}}}
      responses: *algorithmResponses
  /v1/graphs/{graph}/algorithms/pagerank:
    parameters: *graphPath
    post:
      operationId: GraphAlgorithmPageRank
      security: [{BearerAuth: []}]
      description: Read-only; graph_read plus graph binding; last full step on interruption.
      requestBody: {required: true, content: {application/json: {schema: {$ref: '#/components/schemas/GraphAlgorithmRankRequest'}}}}
      responses: *algorithmResponses
  /v1/graphs/{graph}/algorithms/label-propagation:
    parameters: *graphPath
    post:
      operationId: GraphAlgorithmLabelPropagation
      security: [{BearerAuth: []}]
      description: Read-only; graph_read plus graph binding; last full sweep on interruption.
      requestBody: {required: true, content: {application/json: {schema: {$ref: '#/components/schemas/GraphAlgorithmScopeRequest'}}}}
      responses: *algorithmResponses
  /v1/graphs/{graph}/algorithms/betweenness:
    parameters: *graphPath
    post:
      operationId: GraphAlgorithmBetweenness
      security: [{BearerAuth: []}]
      description: Read-only; graph_read plus graph binding; deterministic seeded sample.
      requestBody: {required: true, content: {application/json: {schema: {$ref: '#/components/schemas/GraphAlgorithmBetweennessRequest'}}}}
      responses: *algorithmResponses
components:
  schemas:
    GraphAlgorithmU64:
      type: string
      pattern: '^(0|[1-9][0-9]*)$'
      description: Canonical u64 decimal; maximum 18446744073709551615.
    GraphAlgorithmU64Input:
      oneOf:
        - {$ref: '#/components/schemas/GraphAlgorithmU64'}
        - {type: integer, minimum: 0, maximum: 18446744073709551615}
    GraphAlgorithmSelector:
      oneOf:
        - {type: object, additionalProperties: false, required: [id], properties: {id: {$ref: '#/components/schemas/GraphAlgorithmU64Input'}}}
        - {type: object, additionalProperties: false, required: [key], properties: {key: {type: string, minLength: 1, description: At most 256 UTF-8 bytes}}}
    GraphAlgorithmIdSelector:
      type: object
      additionalProperties: false
      required: [id]
      properties:
        id: {$ref: '#/components/schemas/GraphAlgorithmU64'}
    GraphAlgorithmRelationshipScope:
      oneOf:
        - {type: object, additionalProperties: false, required: [any], properties: {any: {const: true}}}
        - {type: object, additionalProperties: false, required: [only], properties: {only: {type: array, minItems: 1, maxItems: 256, items: {type: string, pattern: '^[A-Za-z0-9_]{1,63}$'}}}}
    GraphAlgorithmLabelScope:
      oneOf:
        - {type: object, additionalProperties: false, required: [any], properties: {any: {const: true}}}
        - {type: object, additionalProperties: false, required: [all], properties: {all: {type: array, minItems: 1, maxItems: 8, items: {type: string, pattern: '^[A-Za-z0-9_]{1,63}$'}}}}
    GraphAlgorithmScope:
      type: object
      additionalProperties: false
      required: [seeds, direction, relationship_types, node_labels]
      properties:
        seeds: {type: array, minItems: 1, maxItems: 10000, items: {$ref: '#/components/schemas/GraphAlgorithmSelector'}}
        direction: {type: string, enum: [out, in, both]}
        relationship_types: {$ref: '#/components/schemas/GraphAlgorithmRelationshipScope'}
        node_labels: {$ref: '#/components/schemas/GraphAlgorithmLabelScope'}
        edge_conditions: {type: array, maxItems: 64, default: [], items: {$ref: '#/components/schemas/Condition'}}
    GraphAlgorithmLimits:
      type: object
      additionalProperties: false
      properties:
        max_hops: {type: integer, minimum: 0, maximum: 4, default: 2}
        max_visited_nodes: {type: integer, minimum: 1, maximum: 10000, default: 1000}
        max_edges_examined: {type: integer, minimum: 1, maximum: 100000, default: 10000}
        max_edges_per_node: {type: integer, minimum: 1, maximum: 10000, default: 1000}
        deadline_ms: {type: integer, minimum: 1, maximum: 5000, default: 250}
        max_output_nodes: {type: integer, minimum: 0, maximum: 1000, default: 100}
        max_output_edges: {type: integer, minimum: 0, maximum: 2000, default: 200}
        max_output_paths: {type: integer, minimum: 0, maximum: 100, default: 20}
    GraphAlgorithmCaps:
      type: object
      additionalProperties: false
      properties:
        max_vertex_operations: {type: integer, minimum: 1, maximum: 1000000, default: 100000}
        max_scratch_bytes: {type: integer, minimum: 1, maximum: 268435456, default: 67108864}
        max_iterations: {type: integer, minimum: 1, maximum: 100, default: 20}
        max_sampled_sources: {type: integer, minimum: 1, maximum: 64, default: 16}
    GraphAlgorithmScopeRequest:
      type: object
      additionalProperties: false
      required: [scope]
      properties: &commonInput
        scope: {$ref: '#/components/schemas/GraphAlgorithmScope'}
        limits: {$ref: '#/components/schemas/GraphAlgorithmLimits'}
        algorithm_limits: {$ref: '#/components/schemas/GraphAlgorithmCaps'}
    GraphAlgorithmPathRequest:
      type: object
      additionalProperties: false
      required: [scope, source, destination]
      properties:
        <<: *commonInput
        source: {$ref: '#/components/schemas/GraphAlgorithmSelector'}
        destination: {$ref: '#/components/schemas/GraphAlgorithmSelector'}
        weight_property: &weight {type: [string, 'null'], minLength: 1, description: At most 256 UTF-8 bytes; null means unit weights}
    GraphAlgorithmPersonalization:
      type: array
      minItems: 1
      maxItems: 10000
      items:
        type: object
        additionalProperties: false
        required: [node, mass]
        properties:
          node: {$ref: '#/components/schemas/GraphAlgorithmSelector'}
          mass: {type: number, format: double, exclusiveMinimum: 0}
    GraphAlgorithmRankRequest:
      type: object
      additionalProperties: false
      required: [scope]
      properties:
        <<: *commonInput
        damping: &damping {type: number, format: double, exclusiveMinimum: 0, exclusiveMaximum: 1, default: 0.85}
        tolerance: &tolerance {type: number, format: double, exclusiveMinimum: 0, exclusiveMaximum: 1, default: 0.00000001}
        personalization: {$ref: '#/components/schemas/GraphAlgorithmPersonalization'}
        weight_property: *weight
    GraphAlgorithmBetweennessRequest:
      type: object
      additionalProperties: false
      required: [scope, sampling_seed]
      properties:
        <<: *commonInput
        sampling_seed: {$ref: '#/components/schemas/GraphAlgorithmU64Input'}
    GraphAlgorithmNormalizedScope:
      type: object
      additionalProperties: false
      required: [seeds, direction, relationship_types, node_labels, edge_conditions]
      properties:
        seeds: {type: array, minItems: 1, maxItems: 10000, uniqueItems: true, items: {$ref: '#/components/schemas/GraphAlgorithmIdSelector'}}
        direction: {type: string, enum: [out, in, both]}
        relationship_types: {$ref: '#/components/schemas/GraphAlgorithmRelationshipScope'}
        node_labels: {$ref: '#/components/schemas/GraphAlgorithmLabelScope'}
        edge_conditions: {type: array, maxItems: 64, items: {$ref: '#/components/schemas/Condition'}}
    GraphAlgorithmNormalizedPersonalization:
      type: array
      minItems: 1
      maxItems: 10000
      items:
        type: object
        additionalProperties: false
        required: [node, mass]
        properties:
          node: {$ref: '#/components/schemas/GraphAlgorithmIdSelector'}
          mass: {type: number, format: double, exclusiveMinimum: 0}
    GraphAlgorithmEcho:
      type: object
      additionalProperties: false
      required: [scope, algorithm]
      properties:
        scope: {$ref: '#/components/schemas/GraphAlgorithmNormalizedScope'}
        algorithm:
          oneOf:
            - {type: object, additionalProperties: false, required: [kind], properties: {kind: {const: degree}}}
            - type: object
              additionalProperties: false
              required: [kind, source, destination, weight_property]
              properties:
                kind: {const: weighted_shortest_path}
                source: {$ref: '#/components/schemas/GraphAlgorithmIdSelector'}
                destination: {$ref: '#/components/schemas/GraphAlgorithmIdSelector'}
                weight_property: *weight
            - type: object
              additionalProperties: false
              required: [kind, damping, tolerance, weight_property]
              properties:
                kind: {const: pagerank}
                damping: *damping
                tolerance: *tolerance
                personalization: {$ref: '#/components/schemas/GraphAlgorithmNormalizedPersonalization'}
                weight_property: *weight
            - {type: object, additionalProperties: false, required: [kind], properties: {kind: {const: label_propagation}}}
            - {type: object, additionalProperties: false, required: [kind, sampling_seed], properties: {kind: {const: betweenness}, sampling_seed: {$ref: '#/components/schemas/GraphAlgorithmU64'}}}
    GraphAlgorithmDegreeOutput:
      type: object
      additionalProperties: false
      required: [kind, rows]
      properties:
        kind: {const: degree}
        rows:
          type: array
          maxItems: 1000
          items:
            type: object
            additionalProperties: false
            required: [node_id, in_degree, out_degree, incident_degree]
            properties:
              node_id: {$ref: '#/components/schemas/GraphAlgorithmU64'}
              in_degree: &count {type: integer, minimum: 0, maximum: 1000000}
              out_degree: *count
              incident_degree: *count
    GraphAlgorithmPathOutput:
      oneOf:
        - {type: object, additionalProperties: false, required: [kind, outcome], properties: {kind: {const: weighted_shortest_path}, outcome: {const: no_path_within_scope_and_hops}}}
        - type: object
          additionalProperties: false
          required: [kind, outcome, cost, node_ids, edge_ids]
          properties:
            kind: {const: weighted_shortest_path}
            outcome: {const: found}
            cost: {type: number, format: double, minimum: 0}
            node_ids: {type: array, minItems: 1, maxItems: 5, items: {$ref: '#/components/schemas/GraphAlgorithmU64'}}
            edge_ids: {type: array, maxItems: 4, items: {$ref: '#/components/schemas/GraphAlgorithmU64'}}
    GraphAlgorithmScores:
      type: array
      maxItems: 1000
      items:
        type: object
        additionalProperties: false
        required: [node_id, score]
        properties:
          node_id: {$ref: '#/components/schemas/GraphAlgorithmU64'}
          score: {type: number, format: double, minimum: 0}
    GraphAlgorithmRankOutput:
      type: object
      additionalProperties: false
      required: [kind, scores, iterations, residual, converged]
      properties:
        kind: {const: pagerank}
        scores: {$ref: '#/components/schemas/GraphAlgorithmScores'}
        iterations: {type: integer, minimum: 1, maximum: 100}
        residual: {type: number, format: double, minimum: 0}
        converged: {type: boolean}
    GraphAlgorithmLabelOutput:
      type: object
      additionalProperties: false
      required: [kind, assignments, sweeps, converged]
      properties:
        kind: {const: label_propagation}
        sweeps: {type: integer, minimum: 1, maximum: 100}
        converged: {type: boolean}
        assignments:
          type: array
          maxItems: 1000
          items:
            type: object
            additionalProperties: false
            required: [node_id, community_id]
            properties:
              node_id: {$ref: '#/components/schemas/GraphAlgorithmU64'}
              community_id: {$ref: '#/components/schemas/GraphAlgorithmU64'}
    GraphAlgorithmBetweennessOutput:
      type: object
      additionalProperties: false
      required: [kind, scores, selected_sources, completed_sources, completed_source_count, estimated]
      properties:
        kind: {const: betweenness}
        scores: {$ref: '#/components/schemas/GraphAlgorithmScores'}
        selected_sources: &sources {type: array, minItems: 1, maxItems: 64, items: {$ref: '#/components/schemas/GraphAlgorithmU64'}}
        completed_sources: *sources
        completed_source_count: {type: integer, minimum: 1, maximum: 64}
        estimated: {type: boolean}
    GraphAlgorithmTruncation:
      type: array
      maxItems: 1
      items:
        oneOf:
          - {type: string, enum: [visited_limit, edge_limit, vertex_limit, memory_limit, output_limit, deadline, cancelled, iteration_limit]}
          - type: object
            additionalProperties: false
            required: [degree_limit]
            properties:
              degree_limit:
                type: object
                additionalProperties: false
                required: [node_ids]
                properties:
                  node_ids: {type: array, minItems: 1, maxItems: 1, items: {$ref: '#/components/schemas/GraphAlgorithmU64'}}
    GraphAlgorithmStats:
      type: object
      additionalProperties: false
      required: [nodes_extracted, edges_extracted, arcs, edges_examined, vertex_operations, scratch_bytes, elapsed_ms]
      properties:
        nodes_extracted: {type: integer, minimum: 0, maximum: 10000}
        edges_extracted: {type: integer, minimum: 0, maximum: 100000}
        arcs: {type: integer, minimum: 0, maximum: 200000}
        edges_examined: {type: integer, minimum: 0, maximum: 100000}
        vertex_operations: *count
        scratch_bytes: {type: integer, minimum: 0, maximum: 268435456}
        elapsed_ms: {type: integer, minimum: 0}
    GraphAlgorithmResponse:
      type: object
      additionalProperties: false
      required: [graph, output, complete, truncation, scope_complete, computation_complete, view_sequence, ttl_clock_ms, algorithm_version, request, max_hops, stats]
      properties:
        graph: {type: string}
        output:
          oneOf:
            - {type: 'null'}
            - {$ref: '#/components/schemas/GraphAlgorithmDegreeOutput'}
            - {$ref: '#/components/schemas/GraphAlgorithmPathOutput'}
            - {$ref: '#/components/schemas/GraphAlgorithmRankOutput'}
            - {$ref: '#/components/schemas/GraphAlgorithmLabelOutput'}
            - {$ref: '#/components/schemas/GraphAlgorithmBetweennessOutput'}
        complete: {type: boolean}
        truncation: {$ref: '#/components/schemas/GraphAlgorithmTruncation'}
        scope_complete: {type: boolean}
        computation_complete: {type: boolean}
        view_sequence: {$ref: '#/components/schemas/GraphAlgorithmU64'}
        ttl_clock_ms: {$ref: '#/components/schemas/GraphAlgorithmU64'}
        algorithm_version: {type: integer, const: 1}
        request: {anyOf: [{$ref: '#/components/schemas/GraphAlgorithmEcho'}, {type: 'null'}]}
        max_hops: {type: integer, minimum: 0, maximum: 4}
        stats: {$ref: '#/components/schemas/GraphAlgorithmStats'}
```

Normalized echo schemas require ID-only strings and explicit edge_conditions.
Canonical ordering and deduplicated type/label lists are additional runtime
output invariants; implementation acceptance must assert them.

### 7.2 Proto insertion

Insert these declarations inside the future Gap 12 `service Graph`, not VLite:

```proto
rpc AlgorithmDegree(GraphAlgorithmDegreeRequest) returns (GraphAlgorithmResponse);
rpc AlgorithmWeightedShortestPath(GraphAlgorithmPathRequest) returns (GraphAlgorithmResponse);
rpc AlgorithmPageRank(GraphAlgorithmRankRequest) returns (GraphAlgorithmResponse);
rpc AlgorithmLabelPropagation(GraphAlgorithmLabelRequest) returns (GraphAlgorithmResponse);
rpc AlgorithmBetweenness(GraphAlgorithmBetweennessRequest) returns (GraphAlgorithmResponse);
```

Append these new top-level messages in package `vlite.v1`. All input scalar options
have presence; unset takes defaults, explicitly zero is validated. An unset
required message/oneof or unspecified direction is invalid_input. A false `any`
is invalid, not a silent empty scope. Condition/MetadataValue reuse existing tags [W3].
Numbers in the snippets are final candidate field numbers. Reserve 100..199 in
each new message; additions may use unassigned 1..99 or >=200 after separate review.
Never reuse deleted field numbers/names; reserve them when removing a field.
Unknown future enum values are rejected on input; output clients must tolerate them.

```proto
message GraphAlgorithmSelector {
  oneof selector { uint64 id = 1; string key = 2; }
  reserved 100 to 199;
}
message GraphAlgorithmStrings {
  repeated string values = 1;
  reserved 100 to 199;
}
message GraphAlgorithmRelationshipScope {
  oneof selection { bool any = 1; GraphAlgorithmStrings only = 2; }
  reserved 100 to 199;
}
message GraphAlgorithmLabelScope {
  oneof selection { bool any = 1; GraphAlgorithmStrings all = 2; }
  reserved 100 to 199;
}
message GraphAlgorithmScope {
  enum Direction { DIRECTION_UNSPECIFIED = 0; OUT = 1; IN = 2; BOTH = 3; }
  repeated GraphAlgorithmSelector seeds = 1;
  Direction direction = 2;
  GraphAlgorithmRelationshipScope relationship_types = 3;
  GraphAlgorithmLabelScope node_labels = 4;
  repeated Condition edge_conditions = 5;
  reserved 100 to 199;
}
message GraphAlgorithmLimits {
  optional uint64 max_hops = 1;
  optional uint64 max_visited_nodes = 2;
  optional uint64 max_edges_examined = 3;
  optional uint64 max_edges_per_node = 4;
  optional uint64 deadline_ms = 5;
  optional uint64 max_output_nodes = 6;
  optional uint64 max_output_edges = 7;
  optional uint64 max_output_paths = 8;
  reserved 100 to 199;
}
message GraphAlgorithmCaps {
  optional uint64 max_vertex_operations = 1;
  optional uint64 max_scratch_bytes = 2;
  optional uint64 max_iterations = 3;
  optional uint64 max_sampled_sources = 4;
  reserved 100 to 199;
}
message GraphAlgorithmDegreeRequest {
  string graph = 1;
  GraphAlgorithmScope scope = 2;
  GraphAlgorithmLimits limits = 3;
  GraphAlgorithmCaps algorithm_limits = 4;
  reserved 100 to 199;
}
message GraphAlgorithmPathParameters {
  GraphAlgorithmSelector source = 1;
  GraphAlgorithmSelector destination = 2;
  optional string weight_property = 3;
  reserved 100 to 199;
}
message GraphAlgorithmPathRequest {
  string graph = 1;
  GraphAlgorithmScope scope = 2;
  GraphAlgorithmLimits limits = 3;
  GraphAlgorithmCaps algorithm_limits = 4;
  GraphAlgorithmPathParameters parameters = 5;
  reserved 100 to 199;
}
message GraphAlgorithmPersonalization {
  GraphAlgorithmSelector node = 1;
  double mass = 2;
  reserved 100 to 199;
}
message GraphAlgorithmPersonalizationList {
  repeated GraphAlgorithmPersonalization entries = 1;
  reserved 100 to 199;
}
message GraphAlgorithmRankParameters {
  optional double damping = 1;
  optional double tolerance = 2;
  GraphAlgorithmPersonalizationList personalization = 3;
  optional string weight_property = 4;
  reserved 100 to 199;
}
message GraphAlgorithmRankRequest {
  string graph = 1;
  GraphAlgorithmScope scope = 2;
  GraphAlgorithmLimits limits = 3;
  GraphAlgorithmCaps algorithm_limits = 4;
  GraphAlgorithmRankParameters parameters = 5;
  reserved 100 to 199;
}
message GraphAlgorithmLabelRequest {
  string graph = 1;
  GraphAlgorithmScope scope = 2;
  GraphAlgorithmLimits limits = 3;
  GraphAlgorithmCaps algorithm_limits = 4;
  reserved 100 to 199;
}
message GraphAlgorithmBetweennessParameters {
  optional uint64 sampling_seed = 1;
  reserved 100 to 199;
}
message GraphAlgorithmBetweennessRequest {
  string graph = 1;
  GraphAlgorithmScope scope = 2;
  GraphAlgorithmLimits limits = 3;
  GraphAlgorithmCaps algorithm_limits = 4;
  GraphAlgorithmBetweennessParameters parameters = 5;
  reserved 100 to 199;
}
message GraphAlgorithmMarker { reserved 100 to 199; }
message GraphAlgorithmEcho {
  GraphAlgorithmScope scope = 1;
  oneof algorithm {
    GraphAlgorithmMarker degree = 2;
    GraphAlgorithmPathParameters weighted_shortest_path = 3;
    GraphAlgorithmRankParameters pagerank = 4;
    GraphAlgorithmMarker label_propagation = 5;
    GraphAlgorithmBetweennessParameters betweenness = 6;
  }
  reserved 100 to 199;
}
message GraphAlgorithmDegreeRow {
  uint64 node_id = 1;
  uint64 in_degree = 2;
  uint64 out_degree = 3;
  uint64 incident_degree = 4;
  reserved 100 to 199;
}
message GraphAlgorithmDegreeOutput {
  repeated GraphAlgorithmDegreeRow rows = 1;
  reserved 100 to 199;
}
message GraphAlgorithmPath {
  double cost = 1;
  repeated uint64 node_ids = 2;
  repeated uint64 edge_ids = 3;
  reserved 100 to 199;
}
message GraphAlgorithmPathOutput {
  oneof outcome {
    GraphAlgorithmPath found = 1;
    GraphAlgorithmMarker no_path_within_scope_and_hops = 2;
  }
  reserved 100 to 199;
}
message GraphAlgorithmScore {
  uint64 node_id = 1;
  double score = 2;
  reserved 100 to 199;
}
message GraphAlgorithmRankOutput {
  repeated GraphAlgorithmScore scores = 1;
  uint64 iterations = 2;
  double residual = 3;
  bool converged = 4;
  reserved 100 to 199;
}
message GraphAlgorithmAssignment {
  uint64 node_id = 1;
  uint64 community_id = 2;
  reserved 100 to 199;
}
message GraphAlgorithmLabelOutput {
  repeated GraphAlgorithmAssignment assignments = 1;
  uint64 sweeps = 2;
  bool converged = 3;
  reserved 100 to 199;
}
message GraphAlgorithmBetweennessOutput {
  repeated GraphAlgorithmScore scores = 1;
  repeated uint64 selected_sources = 2;
  repeated uint64 completed_sources = 3;
  uint64 completed_source_count = 4;
  bool estimated = 5;
  reserved 100 to 199;
}
message GraphAlgorithmOutput {
  oneof output {
    GraphAlgorithmDegreeOutput degree = 1;
    GraphAlgorithmPathOutput weighted_shortest_path = 2;
    GraphAlgorithmRankOutput pagerank = 3;
    GraphAlgorithmLabelOutput label_propagation = 4;
    GraphAlgorithmBetweennessOutput betweenness = 5;
  }
  reserved 100 to 199;
}
message GraphAlgorithmTruncation {
  enum Reason {
    REASON_UNSPECIFIED = 0;
    VISITED_LIMIT = 1;
    EDGE_LIMIT = 2;
    DEGREE_LIMIT = 3;
    VERTEX_LIMIT = 4;
    MEMORY_LIMIT = 5;
    OUTPUT_LIMIT = 6;
    DEADLINE = 7;
    CANCELLED = 8;
    ITERATION_LIMIT = 9;
  }
  Reason reason = 1;
  repeated uint64 node_ids = 2;
  reserved 100 to 199;
}
message GraphAlgorithmStats {
  uint64 nodes_extracted = 1;
  uint64 edges_extracted = 2;
  uint64 arcs = 3;
  uint64 edges_examined = 4;
  uint64 vertex_operations = 5;
  uint64 scratch_bytes = 6;
  uint64 elapsed_ms = 7;
  reserved 100 to 199;
}
message GraphAlgorithmResponse {
  string graph = 1;
  GraphAlgorithmOutput output = 2;
  bool complete = 3;
  repeated GraphAlgorithmTruncation truncation = 4;
  bool scope_complete = 5;
  bool computation_complete = 6;
  uint64 view_sequence = 7;
  uint64 ttl_clock_ms = 8;
  uint32 algorithm_version = 9;
  GraphAlgorithmEcho request = 10;
  uint64 max_hops = 11;
  GraphAlgorithmStats stats = 12;
  reserved 100 to 199;
}
```

Required graph strings cannot be empty (unlike the vector service's legacy
default-collection convention [W3]). Path/betweenness parameters are required;
rank parameters may be absent, meaning all defaults. An explicitly present
empty personalization list is invalid; absence means uniform. Empty weight
strings are invalid, absent means unit weight (JSON null is equivalent).
No JSON blob replaces typed scope/condition/output fields.

`output`/`request` message absence maps to JSON null, not empty output/echo;
present output with unset oneof is invalid server output. Truncation reasons
other than DEGREE_LIMIT have no node_ids; DEGREE_LIMIT has exactly one.
Response oneofs map to JSON `kind` and path `outcome`, not ProtoJSON field
names. gRPC binary uint64 values normalize to section 3 decimal JSON strings
for parity; bounded counts normalize to integers, not ProtoJSON uint64 strings.
Echo uses ID selectors, explicit effective rank options and sample seed.
No existing GraphReadMeta or GraphLimits message is extended or repurposed.

### 7.3 Public reference inventory

When code lands, add `site/api.html` rows with matching keys [D1], not during S5 design:
`rest:POST /v1/graphs/{graph}/algorithms/<suffix>` for all five literal suffixes,
`grpc:AlgorithmDegree`, `grpc:AlgorithmWeightedShortestPath`, `grpc:AlgorithmPageRank`,
`grpc:AlgorithmLabelPropagation`, `grpc:AlgorithmBetweenness`, and
`mcp:vlite_graph_degree`, `mcp:vlite_graph_weighted_shortest_path`,
`mcp:vlite_graph_pagerank`, `mcp:vlite_graph_label_propagation`,
`mcp:vlite_graph_betweenness`. Describe CLI commands in the graph guide; the current
API-reference key inventory has no CLI prefix [D1]. Add any genuinely new public
Rust helper key only if implementation makes it public; keep adapters private.

## 8. Independent oracle and acceptance matrix

The oracle must calculate results from fixture nodes/edges, not consume a Rust
answer and reserialize it. Proposed Python transport consumer checks identical
fixtures through all four surfaces, validates exact keys/types and verifies
normalized echo, numeric ID ordering, flags and first reason independently.
Reuse the fixture semantics of `tests/graph_algorithm_oracle.py` and
`tests/graph_iteration_oracle.py` described in
[Gap 5 coverage](gap5-graph-algorithms.md#test-oracle-and-delivery); do not infer
iteration counts from stationary PageRank. No new oracle is delivered here.

| Independent Python calculation | Golden/result assertions | Mutations that must fail |
|---|---|---|
| Degree: induced-scope BFS/rescan, physical edge enumeration | Loops + parallel edges, Out/In/Both, isolates; exact degrees and ID order including 2 vs 10 and IDs >2^53 | Count loop twice in incident; collapse parallels; lexicographic ID sort |
| Path: exhaustive <=H walks, rational sums of binary64 inputs | Weighted diamond, hop-state counterexample, zero cycle, source=destination, missing/due endpoint, exact-sum ties and unequal exact sums rounding alike; exact path/outcome and once-rounded cost | Round each addition; single state per node; cost-only tie; return NoPath on interruption |
| PageRank: independent ordered synchronous recurrence plus stationary rational goldens | Reciprocal pair, dangling chain, zero weights, personalized isolates; every full step, residual/iterations/converged; scores within 1e-14 per-step, stationary within 1e-8 at tolerance 1e-10/100 steps; mass within 1e-12 | Lose dangling mass; update in place; normalize echo masses; publish partial step |
| Label propagation: independent ascending in-place sweeps | Two pairs + isolate; tied labels, reverse ID insertion; exact per-sweep assignment, smallest member community ID | Synchronous sweep; random ties; loop votes; label rather than minimum member as community ID |
| Betweenness: BFS path counts/dependencies and Python wrapping SplitMix64-v1 | A->B->C full sample B=0.5, diamond multipaths, Both, parallels, isolates, n<=2; exact selected/completed IDs and counts; scores within 1e-12 relative to max(1, \|expected\|), zero scores exactly | Halve Both; seed+ID instead of XOR; count parallel arcs; emit interrupted source; wrong k scaling |

| Cross-surface area | Required cases / exact assertions |
|---|---|
| Wire schema | Every required/optional/null field, both selector forms, ID 0/u64 max, number vs decimal string, duplicate/unknown fields, unknown enum, malformed JSON, NaN/Inf, noncanonical strings, invalid oneofs; validate both OpenAPI and tool schemas |
| MCP SDK compatibility | SDK-style callTool validation on success structuredContent and isError structuredContent; no outputSchema advertised; success envelope matches section 7, error wrapper remains accepted |
| Body/size parity | Missing or non-JSON Content-Type still parses JSON; empty/whitespace body gives 422 invalid_json, never 415; compact re-serialized MCP arguments cap; decoded graph-read gRPC encoded_len cap gives INVALID_ARGUMENT/invalid_json per wire conventions decision 18 (proposed); unknown fields count only toward service frame cap, whose overflow gives native OUT_OF_RANGE without VLite trailer |
| Defaults/ceilings | Each field omitted/default/ceiling/ceiling+1; zero H/output valid, zero work/caps invalid; inactive-algorithm caps still validated; raw list counts before dedup; UTF-8 boundary bytes |
| Execution limits | Exact needed edge/vertex/scratch budget and one below; extraction vs compute vs serialization stops; per-node degree including rejected/repeated edges; zero output; path withheld vs certified NoPath; first reason alone |
| Iteration/sample limits | Cap just before convergence and exactly on convergence; stop within first and later full steps/sources; output-prefix cuts never change computation or source lists; estimated complete sample vs partial estimate |
| Cancellation | Pre-set flag; before/after 64-operation boundary; edge-free preparation/sort/fill loops; final check; REST/gRPC disconnect/drop guard; MCP notification received during local/proxy call through reader thread and request-id flag map; fake monotonic clock, not sleeps |
| Transport deadline | Engine deadline while live channel gives incomplete OK; earlier client gRPC deadline or cancellation gives CANCELLED without trailer per wire conventions section 2.2; no response after expired channel, flag observed and read guard released |
| Errors | Each section 5 row, existing display text, smallest bad edge ID; no new public Error variants, no leaking weight/key/property contents; old read errors unchanged |
| Authorization | Missing/expired/revoked key, wrong lineage, read-only allowed, graph_write without graph_read denied, collection wildcard denied; out-of-binding nonexistent/existing graph indistinguishable; admin vs two tenant keys |
| Tenant/lifecycle | Equal local graph/key/ID names in two tenants; physical names; suspended/deleted tenant; no cross-tenant seeds or existence leak; no vector/attachment reads; proxy must not fall back locally |
| Consistency/non-mutation | TTL equality and node-hidden edges, one clock/view under attempted writes, no caller clock; database bytes/state unchanged; CLI read-only shared lock and --writable/--explain rejected; MCP existing handle (read-only default or writable) never mutated; proxy algorithm explain rejected |
| Parity/compatibility | Compare REST, gRPC normalized output, local/proxied MCP and CLI except elapsed_ms; status/trailer/wrapper/exit mapping; old clients unaffected; new RPCs on older servers UNIMPLEMENTED, no silent fallback |
| Observation/docs | One request record per execution/proxy call, denial audit and reason counters, no sensitive log values; every new data-api key and relative link valid; no stale current-facing docs |

Goldens pin IDs and algorithm_version, not wall-clock elapsed time. Inject a fixed
TTL clock only in tests, never as wire input. Test float portability within stated
tolerances; do not claim portable bitwise equality. Mutation tests are temporary
test runs, not committed production changes.

## 9. Delivery ledger and independent acceptance

`gap5-s5-serving` is this single-document candidate. Acceptance authorizes only the
contract; subsequent code slices need their own review. No slice may widen another's
file ownership implicitly. File lists below are proposed future ownership, not
permission to edit them during the document slice.
Algorithm wire slices land at step 8, after memory, per the
[shared delivery order](serving-wire-conventions.md#51-order); each rebases shared
files onto preceding slices and owns only its five operations and matching rows.

| Ledger name, dependency order | Future files owned | Independent-acceptance checklist |
|---|---|---|
| `gap5-s5-serving` after accepted S1-S4 | This document only | Evidence checked at required base; all five exact schemas/bounds/errors; decisions enumerated; four requested doc checks pass; links resolve |
| `gap5-s5-serving-json` after contract decisions | `src/bin/tool_support/graph_json.rs`, new focused serializer/adapter tests | Strict request decoding; ID/float fidelity; canonical envelope and nulls; existing errors adapted without new Error variants; all five oracle schemas and mutations |
| `gap5-s5-serving-rest` after JSON | `src/server/graph.rs`, `src/openapi_graph.json`, `src/openapi.json`, new REST algorithm tests, REST rows in `site/api.html` | Five POST read classifications; graph/tenant authorization before lookup; one guard/clock; spawn_blocking drop-guard cancellation flag; exact overlay/error/limit parity |
| `gap5-s5-serving-grpc` at step 8 after memory, REST and independently accepted Gap 12 Graph service/auth/typed-read slices | `proto/vlite/v1/vlite.proto`, `src/server/grpc.rs`, new graph-algorithm RPC adapter/tests, gRPC rows in `site/api.html` | Append five RPCs (section 7.2) to whatever Graph holds at step 8 per wire conventions section 1.1; own only those RPCs and matching parity/reference rows; compile descriptors/generated clients; freeze tags/reservations; decoded graph-read encoded_len cap/existing INVALID_ARGUMENT/invalid_json mapping per decision 18 (proposed) vs native OUT_OF_RANGE; spawn_blocking drop-guard flag; deadline/trailer parity; old/new compatibility |
| `gap5-s5-serving-agents` after REST and JSON | `src/bin/tool_support/graph_mcp.rs`, `src/bin/tool_support/graph_proxy.rs`, reader thread/request-id flag map and cancellation dispatch in `src/bin/vlite-mcp.rs`, `src/bin/vlite-graph-cli.rs`, new MCP/CLI tests, MCP rows in `site/api.html` | Five tools and commands; explicit proxy routing, rejecting explain (no ordinary-read fallback); MCP reader-thread cancellation during calls; CLI no signal handler, deadline-bounded; MCP existing read-only/writable handle never mutated; proxy identity/no fallback; compact arguments byte cap; no outputSchema, SDK success/error validation; wrappers, exits, all five oracle results |
| `gap5-s5-serving-parity` after all transports | New `tests/graph_algorithm_serving_oracle.py`, transport fixtures/tests, `docs/GRAPH.md`, matching existing README/site sections only when implementation is published | Python oracle + goldens + mutations; full cross-surface matrix; API-reference/doc checks; independent review; coordinator alone integrates |

Serialize overlapping ownership of `site/api.html` and `src/bin/vlite-mcp.rs`.
The gRPC slice waits for actual Gap 12 acceptance, not merely this design's
service-name agreement. The coordinator assigns the Gap 12 ledger identifiers;
this document does not invent or rename their ledger entries. No policy/default
activation, release criterion, storage qualification or benchmark gate is changed.

## 10. Owner decisions and open items

These decisions concern the candidate, not an assertion of prior approval.
Accept recommendations explicitly during independent acceptance before code.

| Decision | Options and recommendation | Blocking scope |
|---|---|---|
| S5-D1: operation granularity | **Recommend five named read operations** as specified; alternative one tagged algorithm endpoint/tool/RPC. Five avoids algorithm-dependent permission/schema ambiguity; no extra algorithms implied. | All wire schemas |
| S5-D2: specialized weight/numeric errors | **Recommend the three 422 adapter codes** in section 5 over existing GraphAlgorithmError; alternative collapse to invalid_input and lose machine distinction. No public Error variant required; any proposal for one needs separate owner approval. | Error adapter |
| S5-D3: engine vs transport deadline | **Recommend live-channel partial OK and expired-channel native failure**, with cancellation flag and no partial-result guarantee after expiry. Alternative discard partial results at every deadline; that would diverge from embedded semantics. Do not reinterpret the embedded relative deadline as an end-to-end SLA. | REST/gRPC/MCP/CLI cancellation |
| S5-D4: local authority | **Recommend existing local-file authority**, explicit non-tenanted local CLI/MCP, with server parity through proxied MCP. Alternative new remote CLI/credential policy is a separately scoped design, not an S5 shortcut. | Agent-surface acceptance |
| S5-D5: new wire size bounds | **Recommend 1 MiB input / 16 MiB output** for these operations only, sharing graph-read input and gRPC output bounds; alternative smaller output cap needs explicit approval and overflow cases. Existing nonalgorithm defaults stay unchanged. **Withdrawn (ALG-only input scope):** superseded by the graph-read-wide input proposal in [wire conventions, decision 18](serving-wire-conventions.md#6-owner-decisions-23); recommend option B there, not an ALG-only rule. The output proposal is unchanged. Wire conventions, decision 18 (proposed) | Wire adapters |
| S5-D6: proto ownership | **Recommend prefixed algorithm-owned messages**, tags/reservations in 7.2, appended methods to Gap 12 Graph; alternative reuse future shared graph types only after their exact schemas are accepted and this contract is amended. | gRPC slice |

Open implementation items: independently accept Gap 12's exact Graph service
messages and ledger IDs; approve the new adapter wire codes and cancellation
plumbing; prove disconnects stop charged work and release guards on each runtime;
validate expanded OpenAPI/MCP schemas against negative fixtures; prove generated
proto presence and u64 fidelity. Cross-platform floating reproducibility beyond
the oracle tolerances remains open in Gap 5. Offline analytics and new public
engine Error variants remain out of scope, not implicit follow-up approvals.

Candidate checks: `python tests/release_docs.py`, `python tests/content_budget.py`,
`python tests/agent_docs.py`, `git diff --check`, and resolution of every relative
link. No generated schema, code, test, site or README file changes accompany this
contract. Integration/linking from `next-major.md` belongs to the coordinator.
