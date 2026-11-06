STATUS: DRAFT, paused 2026-11-06; unreviewed; NOT ratifiable; not done: (1) independent second review of this revision; (2) final consistency read of sections 5, 7 and 8 after the last edits (sections 3, 4, 6 and 9-12 were re-read); (3) a full read of every cited line range (all 305 unique citations were checked for existence, in-bounds line numbers and the first cited line; interior lines of long ranges were not all re-read); (4) tool validation of the section 7 OpenAPI/proto snippets; (5) OPEN DECISIONS B1b, B6 and B7, which stay open; (6) the coordinator's confirmation of the ruling E wording "0.7.1 ignores the barrier", which conflicts with the code (AC-12); (7) every test, drill and file name below is a proposal and nothing is implemented. Notes: Q:\f1-scratch\t0b-changelog.md.
# Temporal graph serving contract
Status: proposed exact schema for Gap 4 slice E; review required before code.
This document follows the [serving wire conventions](serving-wire-conventions.md), whose shared owner decisions were selected on 2026-11-02; items still unchecked for it in that note's section 7 remain conformance work.
Nothing in this document activates a file, changes a default, or ships an API.
The owner's 2026-11-02 authorization requires exact schemas to be reviewed first
([next-major](next-major.md), lines 52-58); qualification remains separate.
Scope: REST, `vlite.v1.Graph`, MCP, graph CLI, HTTP clients and correction feeds.
Prerequisites: qualified Rust slice C and the applicable
[Gap 12 graph service](serving-contract.md). The
[temporal design](gap4-temporal-graph.md) defines engine semantics.
All wire names below are proposed. **NEW** items do not exist in this checkout;
[section 9](#9-owner-decisions-and-open-items) lists each owner decision they need.
Cross-document wire vocabulary follows that proposal, pending owner ratification.

**Revision T0 (review merge).** This revision applies the independent review
findings that conform the contract to the owner-selected wire decisions 3A, 7, 8A,
10A, 12A, 21 and 23 (cited inline as review A1-A20), records the lane-owner rulings of
2026-11-06 (rounds 1 and 2; round 2 supersedes round 1 where they conflict), writes
the questions that still need an owner as **OPEN DECISION** blocks (B1b, B6, B7;
indexed in [section 9](#9-owner-decisions-and-open-items)), appends
[section 10](#10-activation-runbook-mandatory-for-activation-acceptance),
[section 11](#11-memory-facts-guarantees) and
[section 12](#12-rollout-sequence-and-implementation-order),
and re-verifies every `file:line` citation against this checkout (a citation that
could not be verified says "(unverified)"). The document stays **PROPOSED**:
nothing here is ratified, approved, activated or released. Text headed
"Coordinator ruling (provisional, 2026-11-06)" records a ruling made under owner
delegation; it is provisional and still needs owner ratification.

Non-goals: temporal nodes, transaction-time snapshots, bitemporality, inferred
contradictions, historical vectors/evidence, TTL replication, new query grammar,
an online converter, dashboard temporal editing, performance or storage claims.

## 1. Verified current coverage

These citations describe this checkout, not the older audit baseline of sibling
designs. A shared-open refusal is not a separate temporal-aware implementation.

| Surface or invariant | Current coverage and evidence |
|---|---|
| Public Rust writes | `TemporalSegment`, `TemporalOp::{CreateEdge,PromoteEdge,AppendSegment,ReplaceSegment,CloseSegment,RemoveSegment}` exist at `src/property_graph/temporal.rs:27-103`; `GraphOp::Temporal` at `src/property_graph/mod.rs:354-355`. |
| Rust reads, not public | `GraphRef::as_of`, `GraphRef::temporal_read`, `TemporalReadView::{get_edge,get_edge_by_key,get_node,get_node_by_id,find_nodes,neighbors,timeline,query,explain,query_text,explain_text,traverse,shortest_path,retrieve}` are crate-private at `src/property_graph/temporal/read.rs:124-131,233-449` (`as_of`/`temporal_read` entry points, then the view methods; the view's `retrieve` at 438-449 calls the plain `retrieve`, which has no redaction predicate, so ruling B5 in [section 4](#4-capability-activation-and-authorization) refuses `GraphRetrieve` and `GraphQueryText` on flag-64 files). Do not advertise them as released public APIs. |
| Identity and bounds | One ID/key, fixed endpoints/type, no segment ID; half-open signed epoch-ms intervals, open ends, no merging, 256 maximum: `src/property_graph/engine/temporal.rs:1-24,72-88`; adapter bounds are `Option<i64>`: `src/property_graph/temporal.rs:31-35`. |
| Stored presence | Bounds use one presence byte, bits 0/1, followed by present little-endian i64 values; tags 65-69 do not alter legacy edge payloads: `src/property_graph/engine/codec/temporal_entries.rs:1-20`. This is storage encoding, not protobuf encoding. |
| Activation/open gate | `FLAG_TEMPORAL=64`: `src/format.rs:72-79` (its doc comment at 74-75, "set atomically with the first temporal write", is stale: no write sets it); header refusal before frame loading: `src/format.rs:1449-1460`, with `temporal::supported()` ORed in at 1454-1456 and the flag-needs-graph check at 1471; non-test support is false: `supported()` at `src/property_graph/temporal.rs:268-278`, switched only by the `cfg(test)` thread-local at 280-283 (`Activation`, 285-305). The standalone engine is also test-gated: `supported()` is `cfg!(test)` at `src/property_graph/engine/temporal.rs:26-35` (its comment at 26-30 still says reads do not exist). Only the engine gate is compiled twice: in the `vlite` crate and, by path include, in `crates/vlite-graph` (`crates/vlite-graph/src/lib.rs:13-14`). |
| Writes without capability | Refuse before planning: `src/property_graph/mod.rs:1000-1007`; no mutation enables a file. Copy-out is crate-private, gate-checked, frame-preserving and new-lineage: `src/property_graph/temporal/activate.rs:1-17,37-81`. |
| Server and dashboard | Server opens through `Database::open`/`open_encrypted`: `src/bin/vlite-server.rs:538-541`, reaching the header gate above. The dashboard fetches REST, not database files: `src/dashboard/graph.js:575-594`; it has no independent `UnsupportedFeature(64)` open site. Static assets being available does not mean a refused database is served. |
| CLI and MCP open | Graph CLI write/read opens: `src/bin/vlite-graph-cli.rs:274-317`; MCP plain/encrypted/read-only opens: `src/bin/vlite-mcp.rs:236-239`. Both reach the same header gate. CLI maps the error to `unsupported_feature`: `src/bin/tool_support/graph_json.rs:45`. |
| Embedded bindings | C plain opens: `bindings/c/src/lib.rs:818-820`; encrypted/read-only/byte opens: `bindings/c/src/features.rs:388-403`; Python delegates there: `bindings/python/vlite_embedded/__init__.py:139-168`; Node delegates there: `bindings/node/index.js:112-118` and `bindings/node/addon.cc:230`; WASM byte open: `bindings/wasm/src/lib.rs:55`. All inherit header refusal, not temporal support. |
| Feeds | `Database::temporal_changes_after` is public but gated: `src/property_graph/temporal.rs:232-265`. `GraphChange::Temporal` and five `TemporalChange` variants: `src/property_graph/change.rs:70-122`. Ordinary feeds refuse instead of dropping entries: `src/property_graph/feed.rs:278-286`; REST and webhooks call ordinary `changes_after`: `src/server/features.rs:120`, `src/server/webhooks.rs:429`. JSON `unsupported_temporal` and gRPC `Unspecified` are unreachable markers, not wire support: `src/changes.rs:1131`, `src/server/grpc.rs:947-949`. The shared set type `GraphEventSet` has only `Empty` and `Ttl` (`src/changes.rs:195-211`) and `vlc2` tokens parse only `ttl` (`src/changes.rs:286-289`). |
| Existing REST graph routes | Graph lifecycle, batch, node/edge get, find, neighbors, traverse, shortest-path, explain, schema, retrieve and query routes: `src/server/graph.rs:124-185` (the read-POST suffix array `QUERIES` at 124-139, `route_op` 146-159, `read_post` 162-168, `routes()` 170-185). A route absent from `QUERIES` is admin-only under the deny-unmapped rule (`src/server.rs:889-896`). Slice E adds timeline beside `/edges/get` and extends existing requests; it does not duplicate traversal routes. |
| Existing MCP/CLI | Graph tool names and shared JSON dispatch: `src/bin/tool_support/graph_mcp.rs:34-54,119-136,184-193`; CLI commands, JSON stdin and writable flag: `src/bin/vlite-graph-cli.rs:34-68,194-207`. |
| Existing gRPC | `ChangesRequest`, `WatchChangesRequest`, graph enums/operations/event tags: `proto/vlite/v1/vlite.proto:488-563`. Gap 12's proposed `Graph` service is in `docs/projects/roadmap/serving-contract.md:143-167,212-222`, not an implemented service in that proto (it declares only the `VLite` and `Kv` services). |
| Existing clients | HTTP feed methods: `clients/python/vlite_client/__init__.py:1865-1871`, `clients/javascript/index.js:701-704`, `clients/dotnet/VLite.Client/Client.cs:235-239`. No HTTP client has a graph accessor, so every proposed client method below is **NEW**. |
| Capability discovery | `/v1/capabilities` reports a fixed `features` object (`src/server/admin.rs:468-480`) plus extra booleans (`503-522`); no temporal member exists (**NEW**, OPEN DECISION B7). |
| Feature gating in CI and release | Test and clippy steps run `--all-features` (`.github/workflows/ci.yml:120,123,332`; `.github/workflows/release.yml:100,101,209`); the shipped artifacts use the explicit list at `release.yml:103,210` and `Dockerfile:53`; `tests/graph_surface_parity.rs` also runs without it (`ci.yml:136`); the public item list is pinned by `scripts/api_surface.py check` (`ci.yml:140-142`, baseline `tests/fixtures/api_surface/vlite-default.txt`). No workflow sets `RUSTFLAGS`, and only `crates/vlite-graph/Cargo.toml:10-11` declares a check-cfg (`feature="json"`); the root crate declares none. A `--cfg` gate is invisible to `--all-features`, so the existing jobs keep testing the closed configuration (see the B1 ruling in [section 4](#4-capability-activation-and-authorization)). |

## 2. Numbered acceptance requirements

1. **TE-001, activation:** refuse unsupported temporal files before publishing
   any database handle, route data or feed event. Only an explicit, qualified
   per-file copy-out may add capability 64; never convert on open or first write.
   Until activation, release builds refuse every flag-64 file because the build gate
   of [section 4](#4-capability-activation-and-authorization) stays closed.
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
6. **TE-006, parity:** REST, MCP and the graph CLI (and gRPC once its slice lands)
   enforce identical graph/tenant grants before lookup; local CLI/bindings confer
   only the file access they already own. Owner: server adapter; test: the
   authorization matrix of [section 8](#8-independent-acceptance-and-delivery).
7. **TE-007, feeds:** opt in to correction vocabulary before receiving it;
   preserve durable commit order and whole-commit cursors; emit no event when
   wall time merely crosses a validity boundary. Feeds and webhooks are a follow-up
   of activation (section 4 ruling C): until they land every feed and webhook
   refuses a flag-64 file explicitly.
8. **TE-008, compatibility:** no legacy field/tag/enum redefinition, silent
   downgrade, automatic activation, or changes to old-file/default behavior.
9. **TE-009, publication:** schema approval, implementation acceptance and
   activation qualification are three distinct gates; document every supported
   public surface and explicitly refuse the rest: every graph surface either
   supports temporal (REST, MCP, CLI) or refuses with 409 `unsupported_feature`.
10. **TE-010, build gate (review B1/B2; Coordinator ruling, provisional, 2026-11-06,
    round 2):** all temporal serving behavior, every new public item, route, OpenAPI
    addition and capability member exists only under `--cfg vlite_unstable_temporal`
    (a rustc cfg, not a Cargo feature) until a separately owner-approved step deletes
    the cfg; the closed build's public item list, OpenAPI and `/v1/capabilities` stay
    byte-identical. Owner: engine, server adapter and CI; tests: the closed-build
    goldens, the dedicated cfg CI job and the release assertions of section 4.
11. **TE-011, one-way activation runbook (review C1):** the runbook and drills of
    [section 10](#10-activation-runbook-mandatory-for-activation-acceptance) are
    mandatory for activation acceptance. Owner: docs, CI and maintenance tooling;
    test: the named drills DR-1..DR-10 of section 10.
12. **TE-012, memory-facts guarantees (review C2):** the guarantees of
    [section 11](#11-memory-facts-guarantees) hold for the temporal batch,
    timeline and (later) feed. Owner: engine and server adapter; test: the named
    cases MF-1..MF-8 of section 11.

## 3. Shared JSON and engine semantics

All JSON objects reject unknown request fields. `graph` is an unqualified
tenant-local name in REST paths and a required string in MCP/CLI requests.
IDs, revisions and sequences keep the existing decimal-string output and
decimal-string-or-exact-integer input (`src/server/graph.rs:4-8`).
New temporal times are **canonical decimal strings on output** (signed i64
epoch milliseconds UTC); input accepts those strings or exact JSON integers over
the whole i64 range ([wire conventions decision 12A, owner-selected](serving-wire-conventions.md#6-owner-decisions-23);
review A1 removes the earlier ±(2^53-1) integer cap, which that decision does not allow).
Reject floats, exponents, whitespace, `+`, leading zeros (except `0`), `-0` and
out-of-range values; they fail as REST 422 `invalid_json`. Never parse a timestamp
through floating point. A client whose JSON number type cannot hold an integer
exactly sends a decimal string or its big-integer type (section 5); that is a client
precondition, not a server restriction. The server needs one exact i64 reader beside
the existing u64 helpers (`parse_u64`/`de_u64`: `src/server/graph.rs:827-845`,
`src/bin/tool_support/graph_json.rs:82-100`; no i64 reader exists today), the shared
decision-12 helper (**NEW**; owner: server adapter; test: replay the extremes of
`tests/fixtures/temporal_intervals/golden.json:30-47` and `read-cases.json:21-22` as
JSON integers and as strings through every surface, plus the 2^63 and -2^63-1
rejections of `read-cases.json:279,290`). Prose
`Time`/`TimeInput` abbreviate `GraphTemporalMillis` (output) and `GraphTemporalMillisInput`;
`ID`/`U64String` a decimal u64 string (input `GraphTemporalU64Input`);
`Bounds`/`Segment` the `GraphTemporal*` schemas. TTL clock output is unsigned u64 ms.

`Segment = {valid_from_ms: Time|null, valid_to_ms: Time|null, properties: Metadata}`.
**Both bound members are required on input** and nullable: null means unbounded on
that side, and an absent member is 422 `invalid_json` (review A8), so that a dropped
key (for example a JavaScript `undefined` that serialization omits) can never write
`[from, +infinity)` silently. Output always includes both, null for
negative/positive infinity respectively. `properties` defaults to `{}`; null is
invalid. No integer is an infinity sentinel. An all-time segment is
`{"valid_from_ms":null,"valid_to_ms":null,"properties":{}}`.
For correction selectors, the `old_bounds` object is required and, like a segment,
carries both members: a null member selects the corresponding infinite end, never a
wildcard, and an absent member is 422 `invalid_json`. Owner: server adapter
(`OpIn` parsing); test: the absent-bound write golden of section 8.
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
`src/property_graph/engine/codec/temporal_entries.rs:246-258` (series bytes),
`src/property_graph/mod.rs:990-992` (library `BatchTooLarge`). REST already
refuses more than 4,096 operations as 422 `invalid_input`
(`src/server/graph.rs:110,717-721`). Transport caps (8 MiB bodies,
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

Names and mappings are verified at `src/property_graph/temporal.rs:56-103`;
revision/no-op handling at `src/property_graph/engine/mutate/temporal.rs:12-65,139-155`.
`expected:any`, `expected:absent`, missing revisions and null are invalid for
the five existing-series operations. That needs a **NEW** required-revision type
in the server's own `OpIn`, because the existing `ExpectedIn` defaults to `any`
(`src/server/graph.rs:887-894`; review A15; owner: server adapter; test: one
refusal golden per rejected form). Create has no expected field: identical
keyed retry returns `created:false`; another live edge at the key conflicts.
Corrections compare the revision before the batch, even for close+append in the
same batch (`src/property_graph/temporal.rs:43-47,57-60`).
Do not correct a newly created series in the same batch; create its complete
series instead. New wire acceptance must test this against the planner.
Ordinary edge patch cannot alter a temporal series: it fails with `temporal_edge`
(`src/property_graph/engine/state.rs:1138-1143`), and an ordinary keyed
`CreateEdge` retry never matches a temporal edge, so it fails with
`graph_key_conflict` (`src/property_graph/engine/mutate.rs:447-464`). Ordinary
delete removes the whole series. No automatic close, merge or cross-series
exclusivity.

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
(`src/bin/tool_support/graph_json.rs:1016-1055`,
`src/property_graph/mod.rs:361,375,399` for `GraphWrite`, `GraphOpOutcome` and
`GraphBatchResult`, with the temporal outcome arm at 1114-1128); their new wire inputs need golden tests.

### Reads: extend existing requests; add bounded timeline

Optional `as_of_ms: TimeInput` on the typed structural POST reads (node/edge get,
find, neighbors, traverse, shortest-path) and on `explain` of those six. `GraphRetrieve`
and `GraphQueryText` gain no `as_of_ms`: on a flag-64 file they answer 409
`unsupported_feature` until a redacting temporal variant exists (ruling B5 in
[section 4](#4-capability-activation-and-authorization); a named non-goal of the
capability matrix).
Absent means T=C captured once per operation, checked for i64 overflow.
No client TTL-clock override: C is server/local epoch ms, returned but never
accepted. Expiry is destructive retention, not history: at C equal to an edge or
endpoint expiry the edge is invisible, even when T is earlier
(`src/property_graph/engine/temporal_visibility.rs:9-30`, `is_due` is `deadline <= C`
at `src/property_graph/engine/state.rs:80-82`; default T and the overflow check at
`src/property_graph/temporal/read.rs:68-79`, the captured view at `141-182`).
No new text grammar; `as_of_ms` is outside the query string.

| REST POST suffix; operationId | Request fields, beyond `as_of_ms`, `limits`, `projection` (timeline accepts none of the three) | Response-specific fields |
|---|---|---|
| `/edges/get`; `GraphGetEdge` | `edge:E` required | `edge: Edge or null`, with selected `validity` |
| `/neighbors`; `GraphNeighbors` | `node:E` required; `direction:"out"` default (`out,in,both`); `types` omitted/null = all, nonempty array selects types, `[]` invalid; `edge_conditions:[]`; optional `limit`, `cursor` | `nodes:[Node]`, `edges:[Edge]`, each selected edge has `validity` |
| `/traverse`; `GraphTraverse` | `start:[E]` required; direction/types as above; `expand_filter:{}`, `return_filter:{}`, `edge_filter:[]`, `include_paths:false` | `nodes:[{depth:uint,node:Node}]`, `edges:[Edge]`, `paths:[{nodes:[ID],edges:[ID]}]` |
| `/shortest-path`; `GraphShortestPath` | `source:E`, `destination:E` required; direction/types as above | `shortest_path:{outcome:"found",path:Path,nodes:[Node],edges:[Edge]}` or `{outcome:"no_path_within_hops"}` or `{outcome:"exhausted",truncation:[Reason]}` |
| `/edges/timeline`; **`GraphEdgeTimeline`** | `edge:E` required; `start_index:uint=0`, `end_index:uint=256`; `0 <= start <= end <= 256`; no filters or cursor; no read options | `timeline:null` or `{edge_id:ID,revision:ID,start_index:uint,end_index:uint,total_segments:uint,segments:[Segment]}` |

The base typed request fields above are verified at
`src/bin/tool_support/graph_json.rs:507-605`; output shapes at
`src/bin/tool_support/graph_json.rs:731-765,950-1010`. Edge output keeps today's
`source`/`source_key`/`target`/`target_key` names (`graph_json.rs:741-752`) while
write inputs and feed events say `destination` (`src/server/graph.rs:969-974`,
`src/changes.rs:1096-1098`); this contract renames neither.
Filters are `{labels:[string]=[],conditions:[Condition]=[]}`; edge filters are
typed conditions. Projection remains `"all"` (default), `"none"`, or
`{"only":[string]}` (`src/bin/tool_support/graph_json.rs:491-505,839-845`).
Neighbor `limit` defaults to effective `limits.max_output_edges` (200 by
default), ceiling 2,000; page size must be at least one, including when derived
from an output cap. Preserve these existing rules
(`src/property_graph/engine/query.rs:479-489,511-528,740-744`).
Work/output limits still apply independently.

On a temporal file every successful read is HTTP 200, keeps its existing
response shape and adds top-level `as_of_ms:Time` and
`ttl_clock_ms:U64String` (`src/property_graph/temporal/read.rs:19-39`);
non-temporal responses are unchanged. Typed reads share
`{graph,view_sequence,complete,truncation,stats,cursor,...}`
(`src/bin/tool_support/graph_json.rs:950-1010`); explain keeps
`{graph,explain:{...}}` (`graph_json.rs:938-948`); retrieve keeps its own
`stats` and has no cursor today (`graph_json.rs:1542-1595`) and refuses on a
temporal file (ruling B5).
Edge and node field names remain those cited above; a missing/hidden get or
timeline is null, not 404. Timeline ignores validity, not TTL. Today it does no
budgeted work: the engine returns `complete:true`, empty `truncation`, default
`stats` and a null `cursor` (`src/property_graph/temporal/read.rs:336-347`). This
contract changes that (review A16, partly applied): the engine charges each
returned segment through the read meter, one `max_edges_examined` unit per segment
with the deadline and cancel checks every 64 units
(`src/property_graph/engine/meter.rs:87-98`, `src/property_graph/engine/limits.rs:41`),
so `stats.edges_examined` equals the segments returned and a stopped meter returns
an explicitly incomplete range (`complete:false`, a `truncation` reason, the
ascending prefix; the caller resumes at `start_index + segments.length`). Bytes are
not charged: no byte meter exists (`GraphLimits` has eight work and output fields
and no byte field, `limits.rs:70-85`); they are bounded by the 256-segment window,
at most 256 segments of 65,536 encoded property bytes each, about 16 MiB
(`limits.rs:10-37`). Owner: engine; test: the timeline `stats` golden, a
deadline-mid-range case and the 256-segment worst-case body.
A legacy-edge timeline is 422 `invalid_input` category `not_temporal`. Indices
are requested half-open segment indices, sliced at actual length; report them
unchanged with the total count before slicing and ascending retained segments.
Timeline absence and empty-series timeline are distinct
(`src/property_graph/temporal/read.rs:299-348`). A complete timeline certifies
the requested range, not all history.

`Stats` preserves `nodes_visited,nodes_scanned,edges_examined,nodes_returned,
edges_returned,paths_returned,elapsed_ms,indexes`; reasons preserve strings
`visited_limit,edge_limit,output_limit,deadline,cancelled` or
`{"degree_limit":{"node_ids":[ID]}}`
(`src/bin/tool_support/graph_json.rs:775-789,813-824`).
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
`src/property_graph/temporal/read.rs:141-182` (`captured`, called with the request
cursor by `find_nodes`, `neighbors`, `query` and `explain` at 259, 272, 360, 374).
Reject malformed cursors and cross-graph reuse.
No cursor for traverse, shortest-path or timeline; no stateless timeline
continuation guarantee across revisions. Repeat ranges with the same returned
revision/sequence, or restart if either changes.

> **Coordinator ruling (provisional, 2026-11-06, round 2): B3 forged temporal cursor** (review B3; supersedes the option list of the review)
> **Finding (skeptic, blocking for D4).** The engine takes T and C from a temporal
> (v2) cursor and compares them only with explicit overrides
> (`src/property_graph/temporal/read.rs:141-182`; `temporal_selectors` at
> `src/property_graph/engine/cursor.rs:334-338`, `decode` at 305-330). Cursor integrity
> is an FNV-1a 64 checksum that its own header calls "not a MAC", and the header's
> statement "a forged cursor can only choose a resume position within the same read"
> (`cursor.rs:14-16`) is false for v2 cursors, which store `as_of_ms` and
> `ttl_clock_ms` (`cursor.rs:6-7`). A client can re-encode a cursor with C in the
> past, recompute the fingerprint and checksum, and later pages then return
> retained-but-expired nodes and edges, which defeats D4 ("C is never accepted").
> Ordinary read paging on a flag-64 file has the same exposure because
> `GraphRef::execute` takes T and C from the cursor as well
> (`src/property_graph/read.rs:769-836`).
> **Ruling.** The server adapter seals every cursor it hands out for a read on a
> flag-64 file with a keyed MAC and verifies it before the engine sees the bytes.
> No wire change: the cursor stays an opaque lowercase hex string
> (`src/bin/tool_support/graph_json.rs:607-611,960`; the OpenAPI cursor schema sets
> no length limit), longer by the tag.
> - Construction: HMAC-SHA-256 (RFC 2104) over `sha2` with a 32-byte random key drawn
> once per process by `getrandom::fill` (usage: `src/crypto.rs:270`) and a
> constant-time tag comparison with `subtle` (feature `server`, `Cargo.toml:22,68`).
> Only crates already in the lockfile and the root manifest are used (`sha2`, `subtle`,
> `getrandom`); a new dependency, for example an `hmac` crate, needs the coordinator's
> approval first. The MAC input is a domain label, the tenant-qualified stored graph
> name and the engine cursor bytes.
> - Failure: a forged, stale or other-process cursor (any tag that does not verify,
> including one issued before a restart) answers the SAME existing stale-cursor error as
> an honest stale cursor, with identical status, code and message: 409
> `graph_cursor_stale` (`Error::GraphCursorStale`, `src/server.rs:1732-1734`, message
> "the graph changed after the cursor was issued; restart the read", `src/lib.rs`
> `Display`). The response is therefore no oracle for the key or the C value.
> - Scope: cursors issued for reads on a flag-64 file, ordinary and temporal (v2).
> Cursors of ordinary files (v1) are unchanged (TE-008). Local stdio MCP and the CLI are
> file-authorized and keep the engine checksum; proxied MCP forwards REST cursors.
> - Documentation: a cursor is valid only on the process that issued it; after a
> restart, or against another server process, paging restarts from the first page
> after `graph_cursor_stale`.
> - Owner: server adapter; docs for the cursor paragraph. Tests: (1) a fail-first
> forged-older-C test (re-encode a v2 cursor with an earlier C and a recomputed
> fingerprint and checksum; committed first so that it demonstrably fails on the
> unsealed code, then passes); (2) a hand mutant on the verify path (skip the compare,
> compare a prefix only, accept an empty tag; each must fail a named case); (3) pool
> replay: tests 1 and 2 are replayed on the independent worker pool, and one case shows
> that a cursor issued on one blocking worker verifies on another worker while a
> cursor issued by another process answers `graph_cursor_stale`; (4) an RFC 4231
> known-answer test of the HMAC.

### Errors and resource rules

Codes not marked **NEW** exist today (`src/server.rs:1569-1755` and `graph_code`,
`src/server.rs:1759-1784`); proposed mapping additions follow
[wire conventions section 2](serving-wire-conventions.md#2-error-registry-c2-c9).

| Condition | REST / wire code | gRPC status |
|---|---|---|
| Missing authentication | 401 `unauthorized` | UNAUTHENTICATED |
| Wrong grant/tenant | 403 `forbidden_scope`; write through read-only principal: 403 `read_only` | PERMISSION_DENIED |
| Temporal unavailable, file not opted in, or feed not opted in | **NEW** scoped 409 `unsupported_feature`, `details:{feature:"64",category:"capability"}` (`feature` is a string, review A6); legacy routes keep 500 `internal_error` | FAILED_PRECONDITION |
| Missing graph/entity for a mutation | 409 `graph_not_found`, `graph_edge_not_found`, `graph_node_not_found` | FAILED_PRECONDITION |
| Expected revision mismatch | 412 `precondition_failed` (`src/server.rs:1642-1646`) | ABORTED (`status_code`, `src/server/grpc.rs:208-231`, the 412 arm at 223) |
| Keyed create conflicts with another live edge | 409 `graph_key_conflict` | FAILED_PRECONDITION |
| Malformed or oversize JSON; wrong type, unknown field, absent bound member or non-canonical/out-of-range time | 400 (syntax) / 413 (oversize) / 422 (shape) `invalid_json` (`parse` and `json_error`, `src/server/graph.rs:336-357`) | INVALID_ARGUMENT for adapter validation; transport rejection per wire conventions section 2.2 |
| Invalid temporal operation or read; more than 4,096 operations | 422 `invalid_input`, `details.category` below for temporal reasons | INVALID_ARGUMENT |
| Above graph ceiling | 422 `graph_limit_exceeds_ceiling` | INVALID_ARGUMENT |
| Admission saturation | 503 `busy` | UNAVAILABLE |
| Poisoned database / indeterminate commit | 503 `poisoned` / 503 `commit_indeterminate` | INTERNAL / UNKNOWN |
| Stale read / wrong-lineage feed / below history floor | 409 `graph_cursor_stale` / `cursor_compacted` / `cursor_before_history` | FAILED_PRECONDITION |
| **NEW** feed cursor bound to another feature set | 409 `cursor_mode_mismatch` | FAILED_PRECONDITION |
| **NEW** gRPC adapter only: whole response or feed commit cannot fit the 16 MiB encode cap (decision 3A; REST has no response cap and no `response_too_large`, review A2) | gRPC-internal `ApiError` status 413 `response_too_large`; no cursor advance | RESOURCE_EXHAUSTED |
| **NEW**, deferred (review A11): transport or adapter deadline before commit, no partial allowed | 504 `deadline_exceeded`; not part of the first REST/MCP/CLI slice | DEADLINE_EXCEEDED |

Proposed D8/D9 replacements follow [wire conventions decisions 4, 7 and 21](serving-wire-conventions.md#6-owner-decisions-23)
and [section 2.2](serving-wire-conventions.md#22-one-reachable-family-oversize-deadline-cancel-busy).
**Withdrawn:** separate request-size, transport-deadline and cancellation codes,
and per-draft error-body members/trailer; the shared registry and context proposal replace them.
For each new code, the temporal handler/adapter builds an `ApiError`:
`unsupported_feature` from the existing `Error::UnsupportedFeature` or explicit
temporal/feed negotiation refusal; `cursor_mode_mismatch` from the feed feature-set
check; `response_too_large` (gRPC only) from response preflight; `deadline_exceeded`
(deferred, review A11) from the engine stop reason before commit. No new public
`Error` variant or global `UnsupportedFeature` conversion arm is proposed. Legacy
routes retain their existing mapping: `Error::UnsupportedFeature` falls through to
500 `internal_error` (`error => internal(error)`, `src/server.rs:1752`).
Error context uses only `details`, per [wire conventions section 3](serving-wire-conventions.md#3-encoding-rules-c12)
and decision 21 (owner-selected): one optional `error.details` object on REST, the
same JSON the gRPC trailer carries. Today the shared REST error path has none:
`ApiError::into_response` emits only `{error:{code,message}}`
(`src/server.rs:1526-1542`), and the `details` member of `ApiError`
(`src/server.rs:1498-1507`) reaches gRPC only, as the `vlite-error-details-base64`
trailer (`src/server/grpc.rs:240-245`); only the memory adapter builds its own REST
`details` (`src/server/memory/wire.rs:103`). REST `details` for the graph routes is
therefore **NEW** (owner:
server adapter; test: the refusal golden below). `details.feature` is a **string**
(for flag 64 `"64"`), as `MemoryErrorContext.feature` already is
(`src/openapi.json:194`, `feature: {type: string}`), never the integer 64 (review A6).
Temporal `details.category` is the reason prefix before `:` of the engine
`InvalidQuery` message: `bounds,order,overlap,segment_limit,no_segment,close,
not_temporal,temporal_edge,series_bytes,expected,already_temporal,capability`
(the twelve constants at `src/property_graph/engine/temporal.rs:41-55`; `capability`
is never an `invalid_input` category: a served file reports `unsupported_feature`
with `details.category` `capability`). `graph_error` folds `InvalidCursor`,
`InvalidQuery`, `InvalidSchema` and `InvalidTemporal` into one `Error::InvalidQuery`
(`src/property_graph/convert.rs:174-177`), which REST answers as 422 `invalid_input`
(`src/server.rs:1647-1654`), so the message text is the only discriminator. The
table is exhaustive for this checkout (review A7); each row needs one REST golden
(owner: server adapter; MCP and CLI carry the same code in their error text). In the
Source column `read.rs` is `src/property_graph/temporal/read.rs`, `cursor.rs` is
`src/property_graph/engine/cursor.rs`, `convert.rs` is `src/property_graph/convert.rs` and
`engine/temporal.rs` is `src/property_graph/engine/temporal.rs`:

| Message (exact text or family) | Source | REST status, code and `details.category` |
|---|---|---|
| The eleven reasons with prefixes `bounds`, `order`, `overlap`, `segment_limit`, `no_segment`, `close`, `not_temporal`, `temporal_edge`, `series_bytes`, `expected`, `already_temporal` | `reason` constants, `src/property_graph/engine/temporal.rs:41-54` | 422 `invalid_input`, category = the prefix |
| `capability: the graph state does not have temporal edges enabled` | `engine/temporal.rs:55` | 409 `unsupported_feature`, category `capability` (never an `invalid_input` category) |
| `timeline requires a temporal edge` | `src/property_graph/temporal/read.rs:324` | 422 `invalid_input`, `not_temporal` |
| `timeline bounds must satisfy start <= end <= 256` | `read.rs:306` | 422 `invalid_input`, `bounds` |
| `temporal cursor conflicts with the as_of selector` | `read.rs:155`; ordinary reads `src/property_graph/read.rs:799` | 422 `invalid_input`, no category |
| `temporal cursor conflicts with the TTL clock` | `read.rs:164`; `src/property_graph/read.rs:807` | unreachable from a remote caller (no client C, D4); a hit is an adapter bug: 500 `internal_error` |
| `TTL clock does not fit a default validity instant; specify as_of` | `read.rs:73` (`resolve_as_of`) and `read.rs:104` (`subset_as_of`) | 422 `invalid_input`, no category |
| `temporal read returned no matching segment` | `read.rs:201` | an engine invariant, not caller input: 500 `internal_error`; the adapter maps this exact message until the engine returns a typed internal error |
| `graph batch rejected by graph state` | `src/property_graph/convert.rs:196-202` (`InvariantViolation` and encoding failures) | folded into `InvalidQuery` today, so legacy routes answer 422; the temporal routes map this exact message to 500 `internal_error` |
| Engine cursor failures (`InvalidCursor` reasons such as `cursor belongs to another temporal view` and `cursor is not temporal`) | `src/property_graph/engine/cursor.rs:305-338` | 422 `invalid_input`, no category; a cursor that fails the server MAC never reaches the engine and answers 409 `graph_cursor_stale` (ruling B3); `invalid_cursor` stays the Rust `Error::InvalidCursor` code (`src/server.rs:1665-1669`) |
| Stale cursor after any graph write; forged, other-process or restart-orphaned cursor (ruling B3) | `cursor.rs:326-328`, `convert.rs:173` | 409 `graph_cursor_stale` (existing code and message) |

Any other 422 `invalid_input` has no `details.category`; schema/key failures keep
existing codes. `invalid_request` exists only in CLI/MCP/proxy
(`src/bin/vlite-graph-cli.rs:101`, `src/bin/tool_support/graph_mcp.rs:61`,
`src/bin/tool_support/graph_proxy.rs:190`) for failures REST reports as
`invalid_json`. No broad `internal_error` fallback for validation or the scoped
temporal unsupported-feature failures.

**NEW** refusal JSON (D8; engine text today is
`database header declares unsupported feature flags 0x0040`, `src/lib.rs:638-641`):
`{"error":{"code":"unsupported_feature","message":"temporal capability 64 is unavailable for this operation","details":{"feature":"64","category":"capability"}}}`.
gRPC Status message is that message, code FAILED_PRECONDITION; existing trailers
`vlite-error-code: unsupported_feature` and `vlite-error-details-base64` carry
the code and the base64-encoded JSON details respectively, per wire conventions section 3.
MCP tool failure has `isError:true`, text
`unsupported_feature: temporal capability 64 is unavailable for this operation`,
and `structuredContent:{"error":"unsupported_feature: temporal capability 64 is unavailable for this operation"}`
per [wire conventions section 2.3](serving-wire-conventions.md#23-partial-versus-error).
CLI prints the refusal JSON to
stderr and exits 1 with empty stdout (today's shape lacks `details`: `main` prints
`{"error":{"code","message"}}` only, `src/bin/vlite-graph-cli.rs:335-354`). A failed local open prints this
diagnostic before accepting commands/tools; it is not a successful MCP response.
Bindings keep their language error result, code `unsupported_feature`, feature 64
preserved. All of this is new normalization.

Retain graph limits, defaults/ceilings respectively:
hops 2/4, visited 1,000/10,000, examined 10,000/100,000, degree 1,000/10,000,
deadline 250/5,000 ms, output nodes 100/1,000, edges 200/2,000, paths 20/100.
Reject above ceilings, never clamp; zero hops/output valid, zero work/deadline
invalid (`src/property_graph/engine/limits.rs:63-69,87-108,123-158`).
Charge invalid-at-T adjacency entries too. Use elapsed deadline time, never T.
Deadline and cancellation (D6, review A11): the first REST/MCP/CLI slice implements
only the engine-budget envelope. The graph's own `deadline_ms` and work budgets are
checked by the meter before any work and every 64 units
(`src/property_graph/engine/meter.rs:63-65,87-98`), and a stop returns a certified
incomplete envelope, never a success claim. REST passes `cancel: None` today
(`src/server/graph.rs:450,488,603`), so propagating a client disconnect or a
transport deadline is **NEW** and deferred to a later slice. That slice must use one
named mechanism: the handler owns an `AtomicBool`, a drop guard on the request future
(or the transport deadline) sets it, and it is passed as `ReadOptions.cancel`
(`src/property_graph/read.rs:68`), which the meter already polls. Until then the graph
deadline is the only REST bound; gRPC takes `grpc-timeout` in its own slice.
A graph-budget deadline may return a certified incomplete envelope
while transport time remains; transport expiry and cancellation follow
[wire conventions section 2.2](serving-wire-conventions.md#22-one-reachable-family-oversize-deadline-cancel-busy), not a new domain wire code.
No successful partial serialization, dropped segments or truncated commits.

Wire caps today: REST graph reads 1 MiB (`MAX_READ_BODY`, `src/server/graph.rs:79-80`,
applied at 184), other REST bodies 8 MiB (`MAX_JSON_BYTES`, `src/server.rs:67`, applied at
794), gRPC decode 8 MiB and encode 16 MiB (`src/server/grpc.rs:26,28`, applied at 102-107);
REST responses are uncapped, and the legacy CLI accepts 64 MiB of input
(`src/bin/vlite-graph-cli.rs:72`).
(D10, amended by review A2 to [wire conventions decision 3A](serving-wire-conventions.md#6-owner-decisions-23),
owner-selected): temporal requests keep the existing 1 MiB read and 8 MiB batch request
caps, and the 16 MiB response cap is **gRPC-only** (`response_too_large`,
RESOURCE_EXHAUSTED, in the gRPC follow-up). REST, MCP, the CLI and the HTTP clients have
no response cap and no `response_too_large`; the earlier uniform 16 MiB cap across those
surfaces is withdrawn. Non-temporal behaviour is unchanged. See
[sections 1.5 and 2.2](serving-wire-conventions.md#15-size-and-count-caps-c14).
Reject oversize requests before mutation. A response is bounded by the work that
produces it: reads by the graph limits, a batch by its 4,096 operations, and the timeline
by its 256-segment index window plus the segment charge above; no REST response
preflight is required. Timeline cannot silently omit oversized properties: the caller
requests a smaller index range.

### Commit framing and bounded units (review B4; coordinator ruling, round 2)

**Finding (skeptic, blocking for any feed).** Temporal planning runs `reclaim_due()`
into the same frame as the batch (`src/property_graph/engine/mutate/temporal.rs:79`;
`commit_planned` does the same for every item, `src/property_graph/mod.rs:1043,1049`),
so the number of reclaim entries in a temporal commit is unbounded, and the feed marks
any commit that holds a temporal entry whole (`src/changes.rs:947-952`). That makes the
expansion one unsplittable unit, which breaks the premise of
[wire conventions decision 9](serving-wire-conventions.md#6-owner-decisions-23) that
temporal commits are bounded at 4,096 operations. Promotion also copies the stored
properties into the frame (`src/property_graph/engine/codec/temporal_entries.rs:10-13`,
`src/property_graph/engine/mutate/temporal.rs:93-107`), so about 256 tiny promote
requests give a body near 16 MiB (`src/property_graph/engine/limits.rs:31-35`). Either
can exceed the 16 MiB gRPC encode cap (`src/server/grpc.rs:28`), and the log is
append-only: once such a commit is persisted no later change can split it. (Unverified:
whether a background reclaim job already shrinks the reclaim window.)

> **Coordinator ruling (provisional, 2026-11-06, round 2): B4 commits.** Both parts
> are adopted as a coordinator ruling under owner delegation (not an owner ratification); owner: engine.
> (a) Due reclaim of a batch that contains a temporal operation commits as separate
> EARLIER frame(s), each bounded under the cap and split when needed (greedy, 256
> operations or 1 MiB, the rule of wire conventions section 4); the batch's own frame
> follows. Reclaim is idempotent and touches only items due at the batch's TTL clock.
> Ordinary batches keep today's framing byte for byte (TE-008).
> (b) A temporal batch whose rendered unit (its JSON or proto rendering as a feed would
> deliver it) could exceed the smallest transport cap is refused BEFORE any write with an
> EXISTING error: `Error::BatchTooLarge` (REST 422 `invalid_input`,
> `src/server.rs:1591-1599`; decision 76: no new `Error` variant, no new category, no new
> code).
> Tests: crash-cut tests for a crash between the reclaim frames and the write frame
> (every cut point: after reopen the reclaim is applied or not, the write is absent, a
> retry of the batch succeeds and the final state equals the uncut run); reclaim
> idempotence (replaying it twice changes nothing and it touches only due items); and
> a property test that an over-cap unit is never produced (random promotion-heavy,
> replace-heavy and reclaim-carrying batches: every accepted batch renders at most
> the cap under the oracle's size model, and every refusal consumes no sequence and
> writes nothing).
> Sequencing (round 1, kept): B4 is not an activation prerequisite by itself, because
> feeds refuse on flag-64 files and the follow-up that renders feed units (T4,
> section 6) is parked. Observation for the owner, not a ruling: the log is
> append-only, so commits written before these parts land cannot be repaired later;
> landing them with the temporal write path avoids a permanent defect.

## 4. Capability, activation and authorization

A release build currently refuses at header open even an empty flag-64 file
(section 1). New serving support may accept one only after the engine reads, auth
parity for the supported surfaces and serving qualification all pass; the correction
feed, webhooks and gRPC are follow-ups that refuse explicitly (rulings below).
Qualification does not set any file flag. An accepted temporal file applies fixed T/C
even when the client omits T; old requests cannot see empty temporal properties.
On an old file, ordinary operations remain unchanged; explicit `as_of_ms`,
timeline and temporal writes refuse `UnsupportedFeature(64)`. Feeds on an old file
are unchanged. Unsupported surfaces never open in a "legacy-only" mode or drop
temporal content. An older server may answer a new route with 404 or a new RPC with
UNIMPLEMENTED; clients surface that failure, never retry as a non-temporal operation.

### Build gate (review B1 and B2)

> **Coordinator ruling (provisional, 2026-11-06, round 2): B1 build gate.** The gate is
> `--cfg vlite_unstable_temporal`, a rustc cfg in the manner of `tokio_unstable`. It is
> not a Cargo feature: the round-1 proposal `temporal-preview` is withdrawn, because
> `--all-features` (`.github/workflows/ci.yml:120,123,332`; `.github/workflows/release.yml:100,101,209`)
> would turn a feature on in every existing test run and leave the shipped
> configuration untested. Review item B2 (the gate must close the wire as well as the
> file) is G3 and G8 below. Each requirement has an owner and a test.

- **G1, declaration (engine, CI).** The cfg is declared as a known cfg in both crates
  that compile engine code: in `crates/vlite-graph/Cargo.toml` by extending the
  check-cfg list at lines 10-11, and in the root crate, which has no lints table today,
  by a new `[lints.rust]` check-cfg entry (or `cargo:rustc-check-cfg` from the existing
  `build.rs`, lines 1-8). Without both, `unexpected_cfgs` warns and `-D warnings` jobs
  fail. Test: both crates build warning-free in the default, `--all-features` and cfg jobs.
- **G2, both gates and every registration (engine, server adapter).** The cfg is ORed
  into both `supported()` gates (`src/property_graph/temporal.rs:268-278`,
  `src/property_graph/engine/temporal.rs:26-35`) and into route registration, OpenAPI
  merging, capability registration, MCP tool registration and CLI command
  registration. In `cfg(test)` builds the thread-local `Activation` keeps its
  default-off meaning, so no unit test changes meaning; in non-test builds `supported()`
  is true under the cfg. So that the existing open paths keep refusing in the cfg build
  too, the header gate (`src/format.rs:1454-1456`) admits flag 64 only for an open that
  carries the opt-in of G6, not for every open. Test: closed and open variants of the
  refusal tests, and the goldens of G3.
- **G3, closed means byte-identical (engine, server adapter, CI).** All temporal code
  that this lane adds (the engine's existing crate-private temporal code is not touched and
  stays closed at runtime by the two `supported()` gates) and every new public item (as-of
  reads, `maintenance::enable_temporal`,
  `vlite-maintain enable-temporal`, routes, the OpenAPI overlay, the capability member,
  the MCP tool and CLI command additions) exist only under the cfg. A build without the
  cfg is byte-identical to the base in: (a) the public surface: `python
  scripts/api_surface.py check` against `tests/fixtures/api_surface/vlite-default.txt`
  reports no addition at all (the script's allowance for `vlite::property_graph::`
  additions, `scripts/api_surface.py:15-20`, is not used by this lane); because that list
  holds items but not methods or fields (`scripts/api_surface.py:22-24`), the method level
  is pinned by `python tests/api_reference.py`, which reads public methods, functions,
  routes, tools and commands from the sources (`tests/api_reference.py:9-25`) and, once G7
  makes it skip gated items, fails on any new public method, function, route or tool that
  lacks both the cfg attribute and a `site/api.html` row; (b) the served OpenAPI bytes
  (REG-008, `tests/graph_surface_parity.rs`, run at `ci.yml:135-136`, compares them with the
  baseline captures under `tests/fixtures/graph_baseline/`) and the MCP tool lists
  (`tests/graph_mcp.rs:18-34` on graph-enabled files, REG-008 on vector-only files);
  (c) the `/v1/capabilities` response (a NEW golden, because no test pins the whole body
  today). Tests: these goldens run in the existing default job; mutation: delete the cfg
  attribute of one new public method and one new route, and each of (a) and (b) must fail.
  Decision 76 holds because nothing public ships under the cfg.
- **G4, dedicated CI job (CI).** One job, a separate add-only block in `ci.yml` (a shared
  file), appends `--cfg vlite_unstable_temporal` to `RUSTFLAGS`, keeping `-D warnings`
  (no workflow sets `RUSTFLAGS` today), and runs the library tests, the temporal
  integration tests and clippy (`--all-targets`, `-D warnings`) in its own target
  directory, because RUSTFLAGS changes every crate's fingerprint. The existing default
  and `--all-features` jobs are untouched and keep testing the closed, shipped
  configuration, so gate-closed refusal tests stay valid there. A test that asserts
  closed behavior from a non-`cfg(test)` build (`crates/vlite-graph/tests/temporal_gate.rs`)
  carries `#[cfg(not(vlite_unstable_temporal))]` and gets an open twin in the cfg job.
- **G5, release assertions (CI).** Add-only steps in `release.yml` (a shared file) assert
  that no artifact step sets the cfg (`RUSTFLAGS`, `--config` or a cargo config), covering
  the build lines `release.yml:103,210` and `Dockerfile:53` in the pattern of
  `tests/docker_build_inputs.py:62-76`, and a smoke step runs the shipped `vlite-server`
  on an empty flag-64 file and asserts the open is refused with `unsupported_feature`
  (engine text `database header declares unsupported feature flags 0x0040`,
  `src/lib.rs:638-641`). The release test and clippy steps keep `--all-features`
  (`release.yml:100,101,209`); that does not enable a cfg.
- **G6, opt-in entry point (engine).** The existing open paths (`Database::open`,
  `open_read_only`, `open_encrypted`, `open_encrypted_read_only`, and through them the
  embedded bindings, MCP and the CLI) refuse flag-64 files in every build, closed or
  cfg, for good (until OPEN DECISION B1b). Under the cfg the server opens one through a
  NEW entry point that is crate-private first (server only) and never a changed
  signature of an existing function; the MCP and CLI slices add a cfg-only public entry
  when they need it (OPEN DECISIONS B1b and B6). Test: in the cfg build the new entry
  point accepts an empty flag-64 file and every existing open path refuses it.
- **G7, scanners (CI, docs).** `tests/api_reference.py` reads public methods, functions,
  routes, MCP tools and CLI commands from the sources and requires a `site/api.html` row for
  each; it ignores cfg, although it already skips methods marked `cfg(test)` or `doc(hidden)`
  (`tests/api_reference.py:267,279`), and it reads route literals through `route_keys` and
  `rest_keys` (lines 380-449). Two more guards reuse that discovery and demand an exact
  classification of every registered route with no stale entry: `tests/api_route_inventory.py`
  for the `/v1/graphs` family (`RESERVED_ROUTE_INVENTORY["graph"]`,
  `tests/api_route_inventory.py:51-68,171-217`) and `tests/dashboard_feature_coverage.py` for
  every REST route (`INVENTORY` and `EXCLUDED`, `tests/dashboard_feature_coverage.py:426,522-528`);
  CI runs them at `ci.yml:85,87,90,228,232,233`. A cfg-only route, method or tool would therefore
  break all three or force documenting an unreleased surface. The first slice makes the shared
  discovery skip items under `#[cfg(vlite_unstable_temporal)]` (an add-only change in the style
  of the existing skips, plus a self-test that a cfg-only route, method and tool need no row and
  that the same items without the attribute fail). Deleting the cfg at step 6 makes the scanners
  see the items and requires, in that same change, their `site/api.html` rows, the timeline
  route's entry in `RESERVED_ROUTE_INVENTORY["graph"]` and an `EXCLUDED` entry for it (no
  dashboard temporal controls, section 8).
- **G8, OpenAPI overlay (server adapter).** The temporal paths, component schemas and
  the additions to existing request and response schemas form a cfg-only overlay merged
  into the served document by code under the cfg (the existing merge copies only
  `paths`, `src/server/graph.rs:1777-1806`); `src/openapi.json` and
  `src/openapi_graph.json` stay byte-identical until step 6. Section 7 lists the
  overlay's content. Test: the REG-008 golden of G3 and OpenAPI validation of the open
  document in the cfg job.
- **G9, contributor how-to (docs).** A short how-to, kept in the roadmap docs (not the
  released user docs) until step 6, tells contributors how to build and test under the
  cfg. Illustrative commands, to be verified by the slice: `RUSTFLAGS="${RUSTFLAGS:-}
  --cfg vlite_unstable_temporal" cargo test --locked --lib --target-dir target/temporal`
  (bash) and `$env:RUSTFLAGS = "$env:RUSTFLAGS --cfg vlite_unstable_temporal"; cargo
  clippy --locked --all-targets --target-dir target\temporal -- -D warnings`
  (PowerShell). It states that release and Docker builds never set the cfg, how closed
  and open test twins are written, and that every new public item carries
  `#[cfg(vlite_unstable_temporal)]`.

Deleting the cfg (step 6 of [section 12](#12-rollout-sequence-and-implementation-order))
is a separate, owner-gated change: it removes every annotation, folds the overlay into
the root OpenAPI files, adds the `site/api.html` rows, re-baselines the closed goldens
and settles B1b. The earlier slices cannot do it.

> **OPEN DECISION B1b: shape of the opt-in entry point for flag-64 files (review B1,
> deeper issue; not decided; owner needed at step 6; not a T1 blocker)**
>
> **Question.** When the cfg is deleted, the existing open paths keep refusing flag-64
> files (G6) and only a NEW entry point opts in. What is its public shape? Until the
> owner decides it is crate-private (server only) or cfg-only public, so nothing public
> ships (decision 76).
>
> **Options.** (A) An options value: `Database::open_with(path, &OpenOptions)`; its
> default refuses, a `temporal` member opts in, and the key and read-only choices are
> members too, so one function replaces the open matrix. (B) Named sibling
> constructors, one per existing open path (for example `open_temporal`,
> `open_temporal_read_only`, `open_encrypted_temporal`): no new type, but the open
> matrix multiplies and each is a public API row. (C) A capability token: a value type
> that only an explicit acknowledgement call (for example
> `TemporalOptIn::acknowledge_one_way()`) can construct, passed to one
> `open_with_capability` function, so that the call site documents the one-way effect.
>
> **Reviewers.** Architect and advocate (finding B1): the final opening cannot be one
> global switch; it needs a per-surface or per-open capability whose default refuses.
> Global acceptance (the cfg simply deleted) is not offered because G6 keeps the
> existing paths refusing.
>
> **Recommendation: (A):** the least public growth (one function, one type), it extends
> to later opt-ins without another constructor, and its default stays "refuse". It
> needs the owner because it is public Rust API (a `site/api.html` row,
> `tests/api_reference.py`). Owner: engine; tests: a per-surface open matrix (the
> server opens; each binding, MCP and CLI refuse until they opt in; the default
> refuses), in closed and open variants.

### Scope rulings and non-goals

> **Coordinator ruling (provisional, 2026-11-06): C, the D5 interpretation (round 1, kept).**
> D5 governs activation qualification only. Every graph surface either supports
> temporal (REST, MCP, CLI) or refuses with 409 `unsupported_feature`. The temporal
> feed and webhooks (`graph_events`, slice T4) and gRPC are follow-ups, not activation
> prerequisites. On a flag-64 file the change feeds, webhooks, `GraphQueryText` and
> `GraphRetrieve` refuse explicitly. MCP and CLI support depends on the public opt-in
> (OPEN DECISIONS B1b and B6).
>
> **Coordinator ruling (provisional, 2026-11-06): D (round 1, kept; gated by round 2).**
> A public `maintenance::enable_temporal` and `vlite-maintain enable-temporal` ship after
> qualification, reusing the existing key options (review A12); until step 6 both exist
> only under the cfg (G3). Embedded PUBLIC temporal reads stay out of scope (see OPEN
> DECISION B6 for the items the in-package tools need).
>
> **Coordinator ruling (provisional, 2026-11-06): E (round 1, kept).** The permanent
> erasure barrier stays. The docs say that 0.7.1 ignores the barrier (do not roll back
> while an erasure job exists) and that flag-64 files are one-way. Activation itself
> still needs the owner.
>
> *Verification note (this revision; not part of the ruling).* The tags `v0.7.0` and
> `v0.7.1` already contain the E1 barrier hooks: the E1 commit `cd9a01f6` is an ancestor
> of both and `src/memory/erasure.rs` is in both, while `v0.6.1` and older contain no
> memory code. `gap1-adapter-mapping.md:468` (ES14, proposed) says that 0.7.1 honours the
> barrier at every memory hook but has no route to read, resume or clear a job. The
> sentence "a build without E1 (0.7.1) ignores the job nodes" of
> `gap1-e2-resume-contract.md:502-508` therefore does not match the code. This contract
> states the version-neutral rule of AC-12 in [section 10](#10-activation-runbook-mandatory-for-activation-acceptance),
> and the coordinator confirms or withdraws the 0.7.1 wording before any document says it.
>
> **Coordinator ruling (provisional, 2026-11-06, round 2): B5 retrieve.** `GraphRetrieve`
> and `GraphQueryText` answer 409 `unsupported_feature` on flag-64 files until a
> redacting variant exists, and that variant is a named non-goal of the capability matrix
> below. Evidence: `TemporalReadView::retrieve` calls the plain `retrieve`
> (`src/property_graph/temporal/read.rs:438-449`), whereas REST redacts unauthorized
> collections through `retrieve_visible` (`src/server/graph.rs:606-607`,
> `src/property_graph/retrieve.rs:306-313`). The MCP tools `vlite_graph_retrieve` and
> `vlite_graph_query` and the CLI `query` command refuse the same way, for parity with
> TE-006 (the lane's reading of the ruling, flagged for confirmation). `GraphQueryText`
> also awaits the reconciliation of Gap 6's `as_of_ms: null` on legacy reads
> (`gap6-cypher-subset.md:338-357`) with this contract's omission rule (review A19).
> Owner: server adapter; test: refusal goldens for both routes and their MCP and CLI
> equivalents on a flag-64 file.

Capability matrix when the cfg is gone and a file has been activated (provisional; it
restates the rulings above and does not decide any open item):

| Surface or operation | On a flag-64 file | Status | Owner; test |
|---|---|---|---|
| REST typed reads `GraphGetNode`, `GraphGetEdge`, `GraphFindNodes`, `GraphNeighbors`, `GraphTraverse`, `GraphShortestPath`, `GraphExplain` | served with fixed T/C; `as_of_ms`, `ttl_clock_ms` echo, edge `validity` | in scope (REST slice) | server adapter; wire goldens |
| REST `GraphEdgeTimeline` | served | in scope | engine, server adapter; timeline goldens |
| REST `GraphBatch` with the six temporal operations | served | in scope | server adapter; write goldens |
| REST `GraphRetrieve`, `GraphQueryText` | 409 `unsupported_feature` | **non-goal** until a redacting temporal variant exists (ruling B5) | server adapter; refusal goldens |
| REST `GET /v1/changes`, webhook subscriptions | refuse before any event with 409 `unsupported_feature`; subscriptions blocked | refusals in scope; temporal feed vocabulary is a parked **non-goal** (T4, section 6) | server adapter; section 6 goldens |
| gRPC `Changes`, `WatchChanges` and the Gap 12 `Graph` service | feeds refuse FAILED_PRECONDITION `unsupported_feature`; no temporal graph methods | follow-up of another lane, not an activation prerequisite | gRPC lane |
| MCP tools and graph CLI | typed reads, timeline and writes once the opt-in is public (B1b, B6); `vlite_graph_retrieve`, `vlite_graph_query` and CLI `query` refuse; no feed tool | in scope, gated on B1b and B6 | tool and CLI tests |
| Embedded bindings (C, Python, Node, WASM) | refuse at open | out of scope for good (G6) | existing binding gate tests, unchanged |
| Dashboard | no temporal controls; ordinary graph reads inherit fixed T/C | out of scope | dashboard tests, unchanged |
| HTTP clients (Python, JavaScript, .NET) | flat methods for the served operations | late follow-up (lane slice T6) | client tests |
| Memory fact routes | not served by this contract | reach users only after an explicit owner decision (section 11) | not decided here |
| Public embedded temporal reads | not shipped | out of scope (ruling D); in-package items: B6 | none |

### Operator workflow (proposed; only after separate activation qualification)

Stop/close the source; run **NEW** `vlite-maintain enable-temporal SOURCE
DESTINATION` (positional like `enable-graph`, `src/bin/vlite-maintain.rs:73-80`, whose
report lines it repeats); validate the result; explicitly configure the server to open
DESTINATION; seed new replicas and backups from it. The effects of this copy on
credentials, bindings, feeds, replicas, restores and rollback are the consequence list of
[section 10](#10-activation-runbook-mandatory-for-activation-acceptance), which is
mandatory for acceptance (TE-011).

- **Public form (ruling D).** The proposed public
  `maintenance::enable_temporal(source,destination,key) -> Result<Inspection>`
  (`gap4-temporal-graph.md:606-611`) does **not** exist: today only the crate-private
  `enable_temporal` exists (`src/property_graph/temporal/activate.rs:37-41`), which
  refuses with `UnsupportedFeature(64)` in a closed build (lines 42-44). The function
  and the command ship after qualification, and until step 6 only under the cfg (G3).
  There is no REST, MCP or SDK activation operation. Owner: engine (maintenance) for the
  function, maintenance tooling for the command.
- **Keys (review A12; D7).** Reuse the existing key options; never a key argument and
  never a new `--key-env` flag: `--encryption-key-file PATH`, or the environment variables
  `VLITE_ENCRYPTION_KEY` or `VLITE_ENCRYPTION_KEY_HEX`, exactly one source (otherwise
  "select only one encryption key source"), as `vlite-server` does
  (`src/bin/vlite-server.rs:295-296,432-440`); `vlite-mcp` accepts the file option as
  `--encryption-key-file` or `--key-file` (`src/bin/vlite-mcp.rs:135-141,156-164`).
  `vlite-maintain` has no key option today: `enable-graph` passes `None`
  (`src/bin/vlite-maintain.rs:73-80`), so the command adds the same options to its own
  parser. Keys never appear in command arguments, audit or responses. Owner: maintenance
  tooling; test: an encrypted source activates with the file and with each environment
  variable, and refuses with two sources, with a wrong key (`EncryptionKeyMismatch`) and
  with none (`EncryptionKeyRequired`), the outcomes the engine already has
  (`src/property_graph/temporal/tests/activate.rs:349-384`).
- **Engine behavior kept.** The crate-private copy-out preserves every plaintext frame,
  sequence, revision and absolute deadline under a new lineage, resealing encrypted frames
  under the same key with a fresh salt; the source is untouched and the destination must
  not exist (`src/property_graph/temporal/activate.rs:1-17,37-81`). Do not
  compact/reclaim as part of activation or implement it as a header edit. An open source,
  an incomplete tail, a wrong or missing key, no graph, an already temporal source, an
  existing destination and indeterminate publication remain explicit failures
  (`src/property_graph/temporal/activate.rs:27-35,42-55`).
- **Recommendation, not a ruling.** Because the effect is one-way and whole-file, the
  command should require an explicit acknowledgement option, in the pattern of
  `strip-graph --confirm-graph-data-loss` (`src/bin/vlite-maintain.rs:81-88`), and print
  the consequence list of section 10; the option's spelling is for the owner together with
  OPEN DECISION B1b.

### Authorization

Every temporal read route and tool, including timeline, requires `graph_read` plus the
graph binding; batch requires `graph_write`. Any mixed vector/attachment mutation also
requires the affected collection `write` under the same write guard. No new scope, no
validity-time exemption, no collection-wildcard implication. These preserve existing rules
(`src/server/graph.rs:10-56`). `GraphRetrieve` and `GraphQueryText` are not served on
flag-64 files (ruling B5), so the pivot-collection `read` and evidence-redaction rule
(`src/server/graph.rs:47-56`) has no temporal case to cover until a redacting variant
exists. Qualify tenant-local graph names before lookup (`RequestScope::guarded_graph`,
`src/server/guard.rs:320-333`), reject physical-name bypass, unqualify responses and filter
lists. Authorize before looking up existence: every refusal of this contract that depends
on the file (409 `unsupported_feature`) is produced after the guard, so a caller without
the grant still sees the existing 403 or missing-graph answer. Apply identical policy in
gRPC interceptors (a follow-up of another lane) and proxied MCP; local stdio MCP/CLI is
file-authorized, not an unauthenticated remote service.

**Grants are evaluated once, at admission (review A10).** `authorize` and `authorize_need`
(`src/server.rs:1021-1101`) and the per-route `guarded_graph` call
(`src/server/graph.rs:405,417,430,467`) run before the work is queued. `run` and
`run_read` then take an owned worker permit with `try_acquire_owned` and answer 503 `busy`
when none is free, so there is no wait queue (`src/server.rs:370-440`), and only the
hidden-set travels into the worker (`guard::within`, `src/server/guard.rs:64`). Nothing
rechecks a grant inside the worker today, so the earlier sentence "recheck mutable grants
before queued execution" described no current invariant and is withdrawn: the temporal
routes keep the existing evaluate-once semantics (the role snapshot taken at admission
governs the request). A recheck inside the worker would be a separate decision with its own
test; this contract does not make it.

**Authorization matrix for temporal requests (review A9).** Owner: server adapter. Test:
one golden per row on the REST slice, and the MCP and CLI equivalents where they apply.

| Request | Principal | Answer |
|---|---|---|
| Typed read, explain, timeline or batch | scoped credential whose `graphs` grant does not name the graph | 403 `forbidden_scope` before any lookup (`src/server/guard.rs:320-324`) |
| Any temporal request on a graph a memory binding reserves | principal without `raw_access` on that binding (admin and read-only tokens, signed-in sessions, other credentials) | answered as for a graph that does not exist (`src/server/guard.rs:13-15,325-333,427-446`) |
| Typed read, explain or timeline on a reserved graph | credential holding `raw_access` | served under the ordinary `graph_read` rule |
| Batch with temporal operations on an ordinary graph | `graph_write` and the graph binding, plus collection `write` and binding for mixed vector or attachment operations | served |
| Batch carrying any `temporal_*` operation on a reserved graph | credential holding `raw_access`, the only principal that sees the graph | refused as a whole before any operation runs: 422 `invalid_input` with the existing text "a node key or label is reserved for agent memory" (`RESERVED_NAME`, `src/server/graph.rs:698`) |
| `GraphRetrieve` or `GraphQueryText` on a flag-64 file | any principal that passed the guard | 409 `unsupported_feature` (ruling B5) |

Why the batch-on-a-reserved-graph row exists. The raw-write guard `refuses_raw_batch` covers node
create, upsert and patch only (`src/memory/names.rs:34-50`, called at
`src/server/graph.rs:723-725`), and a `raw_access` credential reaches the reserved graph
(`src/server/guard.rs:133-170`). Memory writes keyed edges with `CreateEdge`
(`src/memory/consolidate.rs:699-720`). A raw `temporal_promote_edge` on one of them turns it
into a temporal edge; memory's keyed create retry then fails with `graph_key_conflict`,
because a temporal edge is never an identical retry (`src/property_graph/engine/mutate.rs:447-463`,
mapped at `src/property_graph/convert.rs:155`), an edge patch fails with `temporal_edge`
(`src/property_graph/engine/state.rs:1138-1143`, text at
`src/property_graph/engine/temporal.rs:49-50`), and nothing demotes the edge. The rule has
the shape of wire-conventions decision 14 (raw writes to reserved objects are refused).
Requirements: (1) a NEW non-test predicate for "this stored graph is reserved by a memory
binding" beside `Reserved::raw_allowed` (the existing `Reserved::is_reserved`,
`src/server/reserved.rs:68-72`, is `cfg(test)` only), recorded on the guarded handle; (2)
the refusal is reachable only when the guard did not hide the graph, so a principal that
cannot see a reserved graph keeps the missing-graph answer and learns nothing from a temporal
batch; (3) it refuses the whole batch for every credential including `raw_access`;
(4) the graph-level check sits beside `refuses_raw_batch` and does not replace it. Tests:
the golden "raw temporal write to a reserved graph" (section 8) and a case that a
non-raw credential receives the missing-graph answer. A correction to the review text: the
failure of memory's own writes is `graph_key_conflict` for the keyed create retry and
`temporal_edge` only for a patch, which memory's production code does not issue (a search of
`src/memory` finds `GraphOp::PatchEdge` only in `src/memory/names/tests.rs:275` and
`src/memory/stale/tests.rs:280`).

> **OPEN DECISION B7: capability discovery (review B7 and A19; not decided; owner needed)**
>
> **Question.** How does a client learn that a server serves temporal graph operations?
> `GET /v1/capabilities` returns a `features` map of booleans and a `routes` list
> (`src/server/admin.rs:468-480,503-522`). The OpenAPI schema keeps `features` an open map
> (`additionalProperties: {"type":"boolean"}`, `src/openapi.json:97`), its `routes` list
> carries no graph route, and no test pins the whole body (tests assert single members, for
> example `tests/graph_server_surfaces.rs:490-493`).
>
> **Options.** (a) Add the member `features.temporal_graph`, a boolean that is true when the
> server serves temporal graph operations on the file it opened; additive, no schema change.
> (b) No discovery: a client probes with a harmless read and treats 404 as "older server" and
> 409 `unsupported_feature` as "file not activated"; nothing is added to the wire. (c)
> Option (a) plus a descriptor of the served operations and limits, for clients that
> feature-detect per operation; the largest surface to keep stable.
>
> **Reviewers.** The merged review lists the member as a MINOR additive recommendation
> (findings B7 and A19, following `src/server/admin.rs:468-480`).
>
> **Recommendation: (a)**, true only when the build serves temporal operations and the served
> file carries flag 64, and registered only under the cfg until step 6 so that
> `/v1/capabilities` stays byte-identical in the closed build (G3). Owner: server adapter;
> tests: a golden of the open response and the closed-build golden of G3.

### Audit and metrics

**NEW** audit fields (D12; today's request observation, metrics and scoped-denial audit
are in `src/server.rs:1103-1269` (`observe`) and `1293-1343` (`record_request`),
`src/server/audit.rs` and `src/server/observability.rs`): `request_id,transport,operation,principal_id,tenant_id,
graph,lineage,scope,outcome,error_code,feature,as_of_ms,ttl_clock_ms,
view_sequence,expected_revision,committed_revision,complete,truncation,
nodes_visited,edges_examined,request_bytes,response_bytes,elapsed_ms,cancelled`.
Unresolved fields are null, not fabricated; record failed authorization without
looking up a forbidden graph. Never record keys, bounds lists, properties,
query text, evidence, credentials or encryption material.
**NEW** metrics use bounded labels `transport,operation,outcome,error_code,feature,
truncation_reason`; counts for refusals, latency/work/bytes histograms, cancellation and
limit exhaustion. Activation attempts are offline `vlite-maintain` runs, recorded only by
the tool's exit status and output; the server never observes them. No tenant/graph/time/key
labels. No cross-tenant result cache; audit follows existing retention/access policy.
Owner: server adapter; tests: an audit-record golden per outcome and a bounded-label test.

## 5. MCP, CLI and language clients

Scope (ruling C): REST, MCP and the graph CLI support temporal operations; gRPC, the
temporal feed, embedded bindings and the dashboard do not (matrix in section 4). Until
step 6 the tool, command and schema additions below exist only under the cfg (G2, G3), and
MCP and the CLI reach a flag-64 file only through the opt-in open of OPEN DECISIONS B1b and
B6; without it they refuse the file at open, reported as `unsupported_feature`
(`src/bin/tool_support/graph_json.rs:45`).

### MCP tools

All tool schemas use JSON Schema object, `additionalProperties:false`.
Existing tool names remain (`src/bin/tool_support/graph_mcp.rs:34-54`):
`vlite_graph_write` takes `{graph,ops:[Operation]}`; `vlite_graph_get` (exactly one
node/edge), `vlite_graph_find_nodes`, `vlite_graph_neighbors`, `vlite_graph_traverse` and
`vlite_graph_shortest_path` gain optional `as_of_ms`. `vlite_graph_query` and
`vlite_graph_retrieve` gain nothing: on a flag-64 file they refuse with
`unsupported_feature` (ruling B5, section 4).
Their input schema is the corresponding REST body schema plus required
`graph:string`; existing `explain:boolean=false` stays on typed reads.
`vlite_graph_edge_timeline` is **NEW**, schema
`{graph:string!,edge:E!,start_index:integer[0,256]=0,
end_index:integer[0,256]=256}` (no `as_of_ms` or `limits`, as in section 3).
Here `!` means required and `?` optional, not nullable; cross-field start/end
validation is mandatory. Responses are exactly REST JSON in structured content
and equivalent JSON text. Writes are listed/callable only with `--writable`;
read-only calls refuse before mutation. No activation tool. A failure is the string
`<code>: <message>` (decision 22; `src/bin/tool_support/graph_mcp.rs:56-58`). The new tool
lives in `graph_mcp.rs` beside the other graph tools, never in `vlite-mcp.rs`, whose nine
core tools `tests/agent_docs.py:8-12` pins. No temporal-only feed tool exists: a follow-up
feed tool is the generic `vlite_graph_changes` in `graph_mcp.rs` (decision 8A and the
`vlite_<domain>_<verb>` rule), parked with T4 (section 6). Owner: server adapter (tool
registration and the shared JSON module `graph_json.rs`, which the REST routes include by
path too, `src/server/graph.rs:76-77`, so its additions are cfg-gated or purely additive);
tests: the REG-008 `tools/list` golden in the closed build (G3), a schema golden and a call
golden per tool in the cfg build, and a parity test that each tool's JSON equals the REST
response.

### CLI

`vlite-graph-cli --db PATH [--writable] [--explain] COMMAND REQUEST` is kept, with `-` for
stdin (`src/bin/vlite-graph-cli.rs:34-68`). `batch` accepts the six temporal JSON operations
beside the existing eleven (`vlite-graph-cli.rs:62-64`); existing get/read commands gain
JSON `as_of_ms`, except `query`, which refuses on a flag-64 file (ruling B5). Add **NEW**
`edge-timeline` with the MCP timeline shape. No separate timestamp flags, implicit Date
parsing or conversion commands here. Timeline disallows `--explain` (the check at
`vlite-graph-cli.rs:208-210` gains `edge-timeline`); typed temporal reads allow it and
return resolved T/C/sequence in explanation metadata. Reads reject `--writable`; writes
require it (`vlite-graph-cli.rs:200-207`). Success exits 0 with one JSON object; a no-op is
success; any failure, including a refusal, prints `{"error":{"code","message"}}` on stderr
and exits 1 (`vlite-graph-cli.rs:335-354`). Reads open the file read-only and writes take the
exclusive lock (`vlite-graph-cli.rs:274-317`), so the opt-in open of B1b needs a read-only
and a writable variant. The command and help additions are cfg-only (G2). Owner: server
adapter; tests: a command golden per new command and a closed-build help golden.

Two parsers read the batch operations: REST's own `OpIn` (`src/server/graph.rs:934,1033`)
and the shared `OpIn` of `graph_json.rs` (`src/bin/tool_support/graph_json.rs:208,307`) used
by MCP and the CLI. Each gains the six temporal operations, so a parity test must feed one
vector of JSON batches to both and compare the resulting `GraphOp` lists, including the
rejection of every invalid shape (section 8).

> **OPEN DECISION B6: public items that the in-package MCP and CLI need (review B6; not
> decided; owner needed; ruling D keeps embedded PUBLIC temporal reads out of scope)**
>
> **Question.** REST needs no public Rust item: it lives in the library crate
> (`src/lib.rs:83-87`) and calls the crate-private read views. `vlite-mcp` and
> `vlite-graph-cli` are separate binaries that use only public `vlite` items
> (`src/bin/vlite-graph-cli.rs:29-32`), and the temporal read views are crate-private
> (`src/property_graph/temporal/read.rs:124-131,233-449`). Temporal MCP and CLI operations
> therefore need public items, which meets the ruling that embedded public temporal reads
> stay out of scope. In-crate code already calls the crate-private view on flag-64 files
> (`src/memory/recall.rs:847-851`).
>
> **Options.** (a) Documented public API: as-of read methods on `GraphRef`, a timeline
> method, `site/api.html` rows checked by `tests/api_reference.py`, and a documented
> addition to the public surface. (b) Unstable hidden items: `#[doc(hidden)] pub` entry
> points that only the in-package binaries call, outside the documented API (the method
> scanner already skips `doc(hidden)` methods, `tests/api_reference.py:279`; free functions
> are not skipped there, and rustdoc's treatment of hidden items by
> `scripts/api_surface.py` is unverified). (c) MCP and CLI refuse temporal operations and
> flag-64 files for good; temporal access is REST or the proxied MCP (`vlite-mcp --server`,
> `src/bin/vlite-mcp.rs:146-150`) only.
>
> **Reviewers.** Finding B6 (merged review): REST needs no public embedded reads (A15), and
> whether to ship them is a separate owner decision in the D7 area; recommended out of scope
> for the first activation. The collision with MCP and CLI is this document's observation,
> not a reviewer finding.
>
> **Recommendation:** (c) if MCP and CLI parity is not wanted at activation; otherwise (b),
> under the cfg until step 6, because it is the only option that satisfies both rulings (MCP
> and CLI support, no embedded public reads); (a) only on an explicit owner decision to
> publish embedded reads. Under every option the opt-in open of B1b comes first. Owner:
> engine; tests: the closed-build goldens of G3 and, for (b), a test that the hidden items
> are absent from the documented API.

### HTTP clients (late follow-up; slice T6)

The language bindings (`bindings/**`) and gRPC belong to another lane, which adds no temporal
methods to `clients/**`; this lane adds the HTTP client methods late, after the REST slice
(slice T6, section 8). Proposed **HTTP client** surface (every method **NEW**, D13): flat
methods derived from the operation names, as wire-conventions decision 10A and rule 5.4
require (no scoped `client.graph(name)` facade, so the graph name is the first argument),
same request/result types, no engine fallback:

| Operation | Python | JavaScript | .NET |
|---|---|---|---|
| Six writes, atomic mixed batch | `graph_batch(graph, ops)` | `graphBatch(graph, ops, {signal}?)` | `GraphBatchAsync(graph, ops, cancellationToken)` |
| Edge get | `graph_get_edge(graph, edge, *, as_of_ms=None)` | `graphGetEdge(graph, {edge,asOfMs?}, {signal}?)` | `GraphGetEdgeAsync(graph, edge, asOfMs, cancellationToken)` |
| Neighbors | `graph_neighbors(graph, request, *, as_of_ms=None)` | `graphNeighbors(graph, request, {signal}?)` | `GraphNeighborsAsync(graph, request, cancellationToken)` |
| Traverse | `graph_traverse(graph, request, *, as_of_ms=None)` | `graphTraverse(graph, request, {signal}?)` | `GraphTraverseAsync(graph, request, cancellationToken)` |
| Shortest path | `graph_shortest_path(graph, request, *, as_of_ms=None)` | `graphShortestPath(graph, request, {signal}?)` | `GraphShortestPathAsync(graph, request, cancellationToken)` |
| Timeline | `graph_edge_timeline(graph, edge, *, start_index=0, end_index=256)` | `graphEdgeTimeline(graph, {edge,startIndex?,endIndex?}, {signal}?)` | `GraphEdgeTimelineAsync(graph, request, cancellationToken)` |

The temporal change feed has no client row: it is parked with T4 (section 6), and the
existing feed methods (`clients/python/vlite_client/__init__.py:1865-1871`,
`clients/javascript/index.js:701-704`, `clients/dotnet/VLite.Client/Client.cs:235-239`) stay
unchanged. Other read methods in section 3 (node get, find nodes, explain) are derived the
same way and also forward the optional selector; `GraphRetrieve` and `GraphQueryText` get no
temporal methods (ruling B5). Every method is new, so `as_of_ms` is a plain optional
parameter; the `<Operation>With<Option>Async` form of rule 5.4 applies only to an option added
later to an already shipped method.
Python timestamps are range-checked `int` over the signed 64-bit range (bool rejected);
segment bounds are `None` and are sent as explicit `null` (review A8); omit `as_of_ms` for
Python None, never serialize it as null. JavaScript uses `bigint` or a canonical decimal
string, bounds `null` (an `undefined` bound is sent as an explicit `null`, never dropped: an
absent bound member is 422 `invalid_json`); it rejects an unsafe `Number` input, which is a
client precondition and not a wire limit (decision 12A); it maps camelCase to the snake_case
wire. .NET uses `long`/`long?` for validity and nullable segment bounds, `ulong` for IDs/TTL
clocks and `CancellationToken`. Clients decode to those lossless types; no Date/datetime
conversion or retry after unknown write outcome. Typed operation constructors
mirror all six discriminants, including required old bounds/revision.
Expose `details.feature` and `details.category` in typed errors, plus transport code.
Older-server errors remain explicit. Embedded C/Python/Node/WASM keep refusing flag-64 files
at open (G6): HTTP client work never depends on binding support. Owner: clients (this lane);
tests: per language and method, a request golden against the REST goldens, the lossless
integer tests of section 8 and an older-server 404 test.

## 6. Change feed and webhooks: refusals now, temporal feed parked

Coordinator ruling (provisional, 2026-11-06, round 2): the temporal feed (slice T4, which
depends on step 5 of the graph-TTL feed) is **PARKED**: off the critical path, no owner, and
not an activation prerequisite (section 4, ruling C). This contract specifies no temporal
feed vocabulary, negotiation, cursor or webhook delivery. What it requires is that every
feed refuses a flag-64 file explicitly, so that no consumer stalls silently or loses
corrections.

### Required refusals on a flag-64 file (part of the REST slice)

| Surface | Required behavior | Today | Owner; test |
|---|---|---|---|
| REST `GET /v1/changes` | Refuses before any event with 409 `unsupported_feature` and `details.feature` `"64"`, for every query the route accepts: no cursor or any cursor, any limit, an empty feed or a filtered one. The check reads the opened file's flags, as `src/memory/recall.rs:847` does, and does not wait for a temporal entry. | The engine refuses only on meeting a temporal entry (`src/property_graph/feed.rs:278-286`); the handler (`src/server/features.rs:97-130`) maps it through the legacy fall-through to 500 (`src/server.rs:1752`). | Server adapter; golden: an empty flag-64 file and one holding temporal entries both answer 409, a legacy file's answers stay byte-identical |
| gRPC `Changes`, `WatchChanges` | Refuse with FAILED_PRECONDITION and the trailer `vlite-error-code: unsupported_feature`, at the call sites `src/server/grpc.rs:2054` and `979` (the stream helper). | The same engine error converts through the legacy fall-through to INTERNAL; 409 already maps to FAILED_PRECONDITION (`src/server/grpc.rs:207-231`). | gRPC lane (shared mapping); golden in that lane |
| Webhook subscriptions | Every subscription is blocked: its delivery status `last_error` is `unsupported_feature:64`, nothing is delivered and the persisted cursor does not advance. | NEW behavior. `last_error` is set only when a delivery attempt fails (`src/server/webhooks.rs:536`), and a feed-read error is swallowed by `Err(_) => continue` (`src/server/webhooks.rs:429-436`), so a subscription stalls with no status. | Server adapter; golden: a subscription on a flag-64 file shows the status and keeps its cursor, a legacy file's webhook behavior is unchanged |

The webhook blocking needs no schema change (`Delivery.last_error` exists,
`src/server/webhooks.rs:25-31`), ships with the REST slice and is decoupled from decision D11
(the webhook migration), which is parked with T4. The refusal branches are temporal code, so
they exist only under the cfg (G2); a closed build cannot open a flag-64 file at all (G5), so
the branches would be unreachable there.

### Parked (T4) and named non-goal

The temporal correction vocabulary in `GET /v1/changes`, the `graph_events` negotiation for it,
temporal webhook delivery, a generic MCP/CLI changes tool and gRPC feed arms are a **named
non-goal** of this contract. A future owner starts from [wire conventions section 4 and
decisions 8 and 9](serving-wire-conventions.md#4-graph-events-in-the-change-feed-c11), the engine
types (`GraphChange::Temporal` and `TemporalChange`, `src/property_graph/change.rs:70-122`;
`Database::temporal_changes_after`, `src/property_graph/temporal.rs:232-265`) and the earlier
text of sections 6 and 7 of this file at base commit `f8e9f3a4`, which is history and not a
requirement of this revision (read it with `git show` on that commit and this file's path).
The review items that belong to it are parked with it: A5 (the event-set spelling and
`temporal_changes_after`) and the lineage-less webhook dedup key of A13. The premise that a
feed unit stays bounded is established at the write path by finding B4 (section 3).

## 7. Exact OpenAPI and proto proposal

`src/openapi.json`, `src/openapi_graph.json` and `proto/vlite/v1/vlite.proto` stay
byte-identical until step 6 of [section 12](#12-rollout-sequence-and-implementation-order)
(G3, G8): until then the temporal REST additions below form a cfg-only overlay that code
under `--cfg vlite_unstable_temporal` merges into the served document. The existing merge
cannot carry it: it inserts only the fragment's `paths` lines before the last base path
line and leaves every other byte of the base unchanged (`src/server/graph.rs:1777-1806`),
so the overlay needs its own cfg-only merge for the new `components` and for the additions
inside existing closed request and response schemas, and it never alters the closed
document. Owner: server adapter. Tests: the REG-008 byte golden of the closed document, and
a structural validation of the open document in the cfg job. At step 6 the overlay is folded
into the root files (the shared `components` start at `src/openapi.json:133`), never as
duplicate routes.

This is an insertion manifest, not replacement of existing path objects:
preserve their security, parameters, responses and operationIds. Apply
`GraphTemporalReadSelector`'s property directly inside each existing closed request
object (not `allOf` outside `additionalProperties:false`) and extend responses
with resolved selectors and edge validity. The JSON types and conditional
required fields of section 3 are normative.

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
        '413': {description: invalid_json (oversize request)}
        '422': {description: invalid_json (shape), invalid_input}
        '503': {description: busy, poisoned}
components:
  schemas:
    GraphTemporalMillis:
      type: string
      pattern: '^(0|-[1-9][0-9]*|[1-9][0-9]*)$'
      description: Signed i64 epoch milliseconds; enforce -9223372036854775808..9223372036854775807
    GraphTemporalMillisInput:
      oneOf:
        - {$ref: '#/components/schemas/GraphTemporalMillis'}
        - {type: integer, minimum: -9223372036854775808, maximum: 9223372036854775807, description: Exact JSON integer over the whole i64 range (decision 12A); a client whose number type cannot hold it sends the string form}
    GraphTemporalBoundsInput:
      type: object
      additionalProperties: false
      required: [valid_from_ms, valid_to_ms]
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
      required: [valid_from_ms, valid_to_ms]
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
| Existing read POST operationIds `GraphGetNode,GraphGetEdge,GraphFindNodes,GraphNeighbors,GraphTraverse,GraphShortestPath,GraphExplain` | Add request `as_of_ms` above (existing inline `limits` objects unchanged), temporal errors, and response `as_of_ms:GraphTemporalMillis` and `ttl_clock_ms:decimal u64 string`, both required when the file is temporal. Add required `validity` (a `GraphTemporalBounds` object or null) to edge objects in temporal responses. Non-temporal responses unchanged. |
| `GraphRetrieve`, `GraphQueryText` | No `as_of_ms` and no new fields (ruling B5): the overlay only adds `unsupported_feature` to the description of their existing 409 response. |
| `GraphBatch` | **Withdrawn:** closed `oneOf` alternatives on `ops.items` would restructure the shared schema. Proposed [wire conventions section 5.2, decision 23](serving-wire-conventions.md#52-per-file-owner-and-sequence): retain the single open `ops.items` object and `maxItems:4096`; add the six write-table literals to its `op` enum and one temporal `allOf` `if`/`then` branch that `$ref`s `GraphTemporalWriteOperation`. No existing variant changes. |
| `GraphTemporalWriteOperation` component | One object using the input schemas above; `op` enum contains all six write-table discriminants. Six internal `allOf` `if`/`then` branches on the `op` const `$ref` the named schemas `GraphTemporalCreateEdge,GraphTemporalPromoteEdge,GraphTemporalAppendSegment,GraphTemporalReplaceSegment,GraphTemporalCloseSegment,GraphTemporalRemoveSegment`; each is a closed object with the exact properties/required/defaults of its write row, not a `oneOf` union. Source/destination: key strings (1-256 UTF-8 bytes); relationship 1-63 ASCII identifier bytes; key nullable, default null; segments maxItems 256. |
| `GraphTemporalTimelineResponse` | Closed object with every shared read field required (`cursor` null; sequence/TTL decimal u64 strings; canonical `as_of_ms`; `complete`, `truncation`, `stats` as existing reads) plus required nullable `timeline`: a closed object with the six section 3 fields required, segments maxItems 256, response segment bounds required. |
| Error schema | Proposed D8 context follows [wire conventions section 3, decision 21](serving-wire-conventions.md#3-encoding-rules-c12): only optional `error.details`, with `feature` and `category` strings inside it (as `MemoryErrorContext`, `src/openapi.json:194`); no per-draft body members. The shared `Error` schema has only `code` and `message` today (`src/openapi.json:278`), so the overlay adds the optional `details` member (additive; REST emits it only where a handler sets it). Scoped temporal unsupported response fixes `details.feature` to the string `"64"` and `details.category` to `capability`, with message/code as section 3. |
| `/v1/changes` GET | The overlay adds a `409` response, `unsupported_feature` with `details` (the refusal of section 6); the existing path item documents only 200 and 400 (`src/openapi.json:45`). No other change: T4 is parked. |
| `/v1/admin/webhooks`, `/v1/admin/webhooks/{id}` | No schema change: a blocked subscription is the existing delivery status field `last_error`, value `unsupported_feature:64` (section 6). |
| `/v1/capabilities` GET | No schema change: `features` is an open map of booleans (`src/openapi.json:97`). The `temporal_graph` member (OPEN DECISION B7) is registered only under the cfg. |

`BearerAuth`, `MetadataValue` and `Error` are existing root definitions
(`src/openapi.json:135,275,278`); add no second metadata/error type. Use OpenAPI
3.1 null unions, not 3.0 `nullable` (root version, `src/openapi.json:2`).
No REST response in this overlay is `413 response_too_large` (decision 3A: the 16 MiB
response cap is gRPC-only, review A2) or `504 deadline_exceeded` (deferred, review A11);
the timeline is bounded by its index window and the read budget (section 3).
UTF-8 byte lengths, i64/u64 string ranges, start<=end, nonoverlap and conditional
required response fields need runtime validation beyond OpenAPI's structural
checks; goldens must test them.

The proto additions below are exact proposed declarations for the gRPC follow-up of another
lane (Gap 12; section 4, ruling C): they are not part of the REST slice or of activation,
and `proto/vlite/v1/vlite.proto` is untouched by this lane until that lane's slice. Reuse
existing `MetadataValue`/`Expected` (`proto/vlite/v1/vlite.proto:107-130`).
Messages are additive; domain-message reserves and shared-message extensions
follow [wire conventions sections 1.3 and 1.4](serving-wire-conventions.md#13-field-number-allocation-c1).
Never reserve or reuse an occupied field.
The feed messages are removed with T4 (section 6): `GraphChangeOperation` (fields 1-18,
enum values 0-9; `proto/vlite/v1/vlite.proto:505-520`) and the `ChangesRequest`,
`WatchChangesRequest` and `ChangesResponse` messages stay untouched.

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
```

Exact integration into the proposed Gap 12 service/messages (the feed additions of the
earlier revision are removed with T4):

```proto
// Append inside service vlite.v1.Graph; retain all methods already landed.
rpc EdgeTimeline(GraphTemporalTimelineRequest) returns (GraphTemporalTimelineResponse);
// Append to each Gap 12 request named in the read operation table:
// GetNodeRequest, GetEdgeRequest, FindNodesRequest, NeighborsRequest,
// TraverseRequest, ShortestPathRequest, ExplainRequest. RetrieveRequest and
// QueryTextRequest get no selector (ruling B5):
GraphTemporalSelector temporal = 200;
// Append once to shared GraphReadMeta (covers those read responses):
GraphTemporalContext temporal = 200;
// Append to shared Gap 12 GraphEdge; absent = legacy, present empty = all-time:
GraphTemporalBounds validity = 200;
// Append as a new member of GraphWriteOperation's operation oneof:
GraphTemporalWriteOperation temporal = 200;
```

Proposed D3 allocations follow [wire conventions section 1.3, decision 1](serving-wire-conventions.md#13-field-number-allocation-c1):
carve tag 200 out of the temporal fence in each shared message above, leaving
`reserved 201 to 299`; retain the other sibling fences.
Gap 12 must fix the shared GraphEdge/GraphReadMeta and operation-oneof names
before implementation and publish one combined descriptor. The descriptor test
adds a named entry for each tag-200 extension. No feed tag, enum value or
`GraphChangeOperation` change is proposed (T4 is parked), so
`docs/projects/roadmap/serving-contract.md:220` ("keep GraphChangeOperation and
existing stream messages unchanged") needs no amendment from this contract; the TTL
slice owns the shared feed fields and change tags 19-22.
Proto generated JSON mapping uses int64 decimal strings; binary wire uses
ordinary protobuf int64 presence, never the storage presence byte.
JSON `Segment` is flat (`valid_from_ms`, `valid_to_ms`, `properties`), while
proto `GraphTemporalSegment` nests `bounds`; the proto binding converts. A timeline
request has no read options; absent indices mean 0 and 256, and `graph` and
`edge` are required. Section 3's zero-work envelope (complete, empty truncation,
zero stats) travels in `meta` (field manifest: D3).
Required selectors/messages and kind-dependent fields follow the JSON write
table; missing edge/expected/old_bounds/segment/at, kind 0 and inapplicable
nondefault fields are INVALID_ARGUMENT, and `Expected` must carry `revision` for the
five existing-series operations (`any` and `absent` are INVALID_ARGUMENT, as in
section 3). Empty GraphTemporalBounds means open/open,
not unspecified old bounds; every GraphTemporalSegment carries its bounds message
(empty for all-time). Empty segments is legal on create/put. Zero ID, time and
index values retain presence where optional. Timeline absent
means JSON null.

Every new public Rust API needs its own `site/api.html` row and matching
`data-api` key; do not publish proposed private names prematurely. Under the cfg
the scanners skip gated items, so no row exists until step 6 (G7). The scanner's
public-visibility rule and its server/bin exclusions are at
`tests/api_reference.py:27-29,206`. Include REST/gRPC/MCP and new client/binding
reference updates in their implementation slices as applicable.

## 8. Independent acceptance and delivery

Reusable oracles and artifacts (review C4). Extend the independent
[interval oracle](../../../tests/temporal_interval_oracle.py) and
[read oracle](../../../tests/temporal_read_oracle.py), not a copy of the Rust
implementation, with wire-normalization and whole-series revision models (the feed-order
model is parked with T4). Retain the existing
[interval goldens](../../../tests/fixtures/temporal_intervals/golden.json) and
[read cases](../../../tests/fixtures/temporal_intervals/read-cases.json);
add hand-authored request/response goldens at implementation (binary-proto goldens belong
to the gRPC lane). Rust patterns to reuse: the engine tests in
`src/property_graph/temporal/tests/` (`activate.rs`, `exports.rs`, `revisions.rs`,
`writes.rs`) and `src/property_graph/temporal/read/tests.rs`, and the integration tests
`tests/rest_graph.rs` (REST graph routes, auth and limits), `tests/graph_change_feed.rs`
(feed behavior, for the section 6 refusals) and `tests/graph_surface_parity.rs` (REG-008: it
compares the served OpenAPI bytes and the MCP responses of a vector-only file with the baseline
captures, so it pins the closed build; it holds no old-file refusal case).
Their source-of-truth role is logical semantics, not proof of persistence/auth.

| Matrix | Required independent assertions |
|---|---|
| Times/intervals | from-1/from/to-1/to, both infinities, all-time versus legacy, negatives and the i64 extremes accepted as JSON integers and as strings through every surface (replay `golden.json:30-47` and `read-cases.json:21-22`; 2^63 and -2^63-1 rejected, `read-cases.json:279,290`), null versus absent (an absent bound member is 422 `invalid_json`, review A8), rejected float/exponent/leading zero/out-of-range (a JavaScript Number beyond the safe range is a client precondition, not a server refusal), gaps/touching/overlap, 256 accepted/257 atomic refusal. |
| Writes | All six variants; promotion; identical keyed retry/conflict; exact old bounds; stale expected; `any`, `absent` or missing `expected` refused for the five existing-series operations; close no-op/no event; empty retained series; full-series replacement; same-batch supersession; mixed vector/TTL final-op failure leaves everything unchanged; reclaim-heavy and promotion-heavy commits (B4: bounded frames, an over-cap unit refused before any write, crash cuts between the reclaim and write frames); a raw temporal write to a reserved graph (A9). |
| Reads | REST/MCP/CLI/client equality for get/neighbors/traverse/shortest-path/timeline (gRPC when its slice lands); hidden endpoints; TTL C before/equal/after independent of T; post-purge nonresurrection; current node metadata/evidence limitations; `GraphRetrieve`, `GraphQueryText` and their MCP and CLI equivalents refuse 409 `unsupported_feature` on a flag-64 file (B5). |
| Certification | Invalid-at-T edges charged; each work/output cap; the engine-budget stop reasons, including the deadline inside `limits`; shortest-path never provisional; timeline segments charged through the meter with an explicitly incomplete prefix on stop (review A16); no transport truncation masquerading as complete. Transport deadline and cancellation are deferred (review A11). |
| Paging/timeline | Captured default T/C, conflicting override, stale/cross-graph cursor, a forged-older-C cursor (B3: forged, stale and other-process cursors all answer `graph_cursor_stale`), gap null get, empty timeline, legacy refusal, 0/0 and 256/256, start>end and 257, range past length, changed revision between ranges. |
| Feed refusals (section 6) | `GET /v1/changes` answers 409 `unsupported_feature` before any event on an empty and on a populated flag-64 file, for every accepted query form; a legacy file's feed is byte-identical; every webhook subscription on a flag-64 file shows `last_error` `unsupported_feature:64` and keeps its cursor; the gRPC refusals are the gRPC lane's. The temporal feed vocabulary is parked (T4). |
| Persistence | Plain/encrypted reopen, directory checkpoint, compact image, activation copy; checkpoint floor versus lineage mismatch; revisions reacquired; no synthetic history. |
| Activation runbook | The drills DR-1..DR-10 of [section 10](#10-activation-runbook-mandatory-for-activation-acceptance), on a file with memory bindings, scoped credentials and provider bindings. |
| Memory facts | The cases MF-1..MF-8 of [section 11](#11-memory-facts-guarantees). |
| Authorization | No auth, revoked/expired/read-only, graph binding, collection wildcard without graph scopes, tenant physical-name bypass, forbidden existence, mixed collection writes; the reserved-graph rows of the matrix in section 4 (A9); REST and proxied-MCP parity (gRPC when its slice lands). |
| Compatibility/refusal | Old generated client/new server and reverse; old file exact legacy goldens; no automatic capability on write/open; an empty flag-64 file refused by every existing open path (embedded Rust, bindings, MCP, CLI) in every build and accepted only by the opt-in entry point under the cfg; unsupported binding/CLI/dashboard path; no unknown value handled as success. |
| Closed build (G1-G9) | The closed-build goldens: public items (`api_surface.py`) and public methods (`api_reference.py`), the served OpenAPI bytes, the MCP tool lists, the CLI help and `/v1/capabilities`; the open-build twins of the gate refusal tests; the dedicated cfg job; the release assertions and the smoke that the shipped `vlite-server` refuses an empty flag-64 file; the scanner self-test. |
| Schemas/errors | OpenAPI validation of the open overlay, including the six `op` enum literals and the single temporal `allOf` branch; descriptor checks for shared tag 200 (gRPC lane); every error code with REST `details` (`feature` a string); each message of the section 3 table (review A7) answers its row; no REST `response_too_large`; every default/ceiling/ceiling+1, 1/8/16 MiB boundaries; all relative links/API-reference keys. |

Mutation list: flip inclusive start/exclusive end; map null to zero; round i64
through f64; cap integers at 2^53; collapse open/open into legacy; select latest
properties in a gap; resample C on continuation; remove endpoint TTL; stop charging
filtered entries; stop charging timeline segments; check current instead of batch-start
revision; silently merge touching segments; accept segment 257; serialize exhausted as
no-path; treat an absent bound member as null; retain old cursor across lineage; accept a
cursor whose engine checksum is valid but whose MAC tag is missing, short or wrong; compare
the tag by prefix or skip the compare; answer a forged cursor with a different error than a
stale one; let collection wildcard grant graph scope; fold the reclaim into the write frame;
let an over-cap temporal unit through; let a raw credential promote an edge of a reserved
graph; serve `GraphRetrieve` or `GraphQueryText` on a flag-64 file; leave a webhook
subscription unblocked or advance its cursor on a flag-64 file; let `GET /v1/changes` answer
before the flag check; delete the cfg attribute of one new public method or route (G3); OR
the cfg into the header gate for every open instead of only the opt-in (G2). Each mutation
must make an independent named case fail before the corresponding slice is accepted.

The following are proposed file ownership boundaries, not files changed here. The lane
slice ids (T1, T2, T4, T6) come from the F1 slice plan and are not defined in this
repository; the `gap4e-*` names are this document's. Every slice waits for the qualified
engine reads, which stay crate-private: REST needs no public Rust read API
(`src/lib.rs:83-87`). The gRPC slice, the language bindings and `bindings/**` belong to
another lane and also wait for the reviewed Gap 12 shared service/types. Landing order
(details in [section 12](#12-rollout-sequence-and-implementation-order)): `gap4e-gate`, the
shared prerequisites, then `gap4e-engine`, `gap4e-rest` and `gap4e-mcp-cli`; much later,
after Gap 12, `gap4e-grpc` and `gap4e-clients`. Integration must not bypass gates. Shared
files (`ci.yml`, `release.yml`, `src/server/features.rs`, `src/server/admin.rs`,
`src/server.rs`, the scanners) take add-only changes, each in its own commit.

| Slice | Files owned | Required tests and independent acceptance checklist |
|---|---|---|
| `gap4e-gate` (lane T1) | `src/format.rs` (the header gate's opt-in), `src/property_graph/temporal.rs` and `src/property_graph/engine/temporal.rs` (both `supported()`), the root `Cargo.toml` or `build.rs` and `crates/vlite-graph/Cargo.toml` (check-cfg), `src/maintenance.rs`, `src/bin/vlite-maintain.rs` and `src/property_graph/temporal/activate.rs` (`enable_temporal`, `enable-temporal`; cfg-only), the shared scanner discovery in `tests/api_reference.py` (add-only skip plus self-test), add-only blocks in `.github/workflows/ci.yml` and `release.yml`, the contributor how-to and the runbook docs | G1-G9; the open and closed twins of the gate refusal tests (`crates/vlite-graph/tests/temporal_gate.rs`, `src/property_graph/temporal/tests.rs:222-249`); the dedicated cfg job; the release assertions and smoke; the activation tests of section 4 (an encrypted source per key source); the drills DR-1..DR-10 (section 10). Accept only when every closed-build golden is unchanged. |
| `gap4e-engine` (with lane T2) | `src/property_graph/mod.rs` (`commit_planned`, batch admission), `src/property_graph/engine/mutate/temporal.rs`, `src/property_graph/temporal/read.rs`, `src/property_graph/engine/meter.rs` (timeline charging); engine owner: this lane | B4: bounded reclaim frames, idempotent reclaim, crash cuts between the reclaim and write frames, and the property test that an over-cap unit is never produced; A16: timeline `stats`, a deadline-mid-range case and the 256-segment worst-case body; any new public entry point (B6) cfg-only; engine unit tests keep the thread-local meaning. |
| `gap4e-rest` (lane T2) | `src/server/graph.rs`, `src/server/features.rs` (add-only lines, shared with another lane), `src/server/webhooks.rs` (the blocking of section 6 only), `src/server/admin.rs` (the capability member: add-only), `src/server/guard.rs` and `src/server/reserved.rs` (the reserved-graph predicate of section 4), `ApiError`/auth/error-mapping glue in `src/server.rs`, the cfg-only OpenAPI overlay code and data (the root OpenAPI files stay unchanged until step 6), relevant server tests/docs. Not `src/bin/tool_support/graph_json.rs`: REST has its own `OpIn`, `explanation`, `read_output` and `batch_output` (`src/server/graph.rs:934,1653,1665,1743`) | Oracle/wire goldens; all routes/auth/limits; the A9 matrix; scoped handler-built errors with REST `details`; the single temporal `GraphBatch` branch; the cursor MAC tests (B3); the B5 refusals and the section 6 refusals on a flag-64 file; the capability member and the overlay golden; old-file goldens; source unchanged by activation. The shared prerequisites of section 12 step 2 land before it as separate add-only commits. Accept only with independently reproduced negative cases and no gate changes. |
| `gap4e-mcp-cli` | `src/bin/tool_support/graph_mcp.rs`, `src/bin/tool_support/graph_json.rs` (shared by path with REST, `vlite-mcp` and `vlite-graph-cli`: `src/server/graph.rs:76-77`, `src/bin/vlite-mcp.rs:1-2`, `src/bin/vlite-graph-cli.rs:17-18`; additions cfg-gated or purely additive), `graph_proxy.rs`, `src/bin/vlite-mcp.rs`, `src/bin/vlite-graph-cli.rs`, command/tool tests and reference docs; needs the opt-in open of B1b and the items of B6 | Stdin exact int parsing, tool-schema validation, writable/read-only listing/calls, local/proxy parity, stderr/exit/open refusal, timeline, the `query` and `retrieve` refusals, the two-parser parity test of section 5, the closed-build tool-list and help goldens. Accept only after independent malformed-input and no-silent-fallback cases. |
| `gap4e-grpc` (another lane) | `proto/vlite/v1/vlite.proto`, `src/server/grpc.rs`, graph service implementation/tests, generated descriptor fixtures, `site/api.html` rows | After Gap 12, and not an activation prerequisite (ruling C): named descriptor additions for shared tag 200; unary parity; the feed refusals of section 6 as FAILED_PRECONDITION; old descriptor compatibility; transport cancellation per wire conventions. Accept only with independent binary goldens and REST-equivalent authorization. |
| `gap4e-feed` (T4, parked) | none: no owner (section 6) | none specified by this contract. |
| `gap4e-clients` (lane T6) | `clients/python/vlite_client/__init__.py`, `clients/javascript/index.js`/`index.d.ts`, `clients/dotnet/VLite.Client/Client.cs` and model files/tests/docs; never `bindings/**` | Late, after the REST slice: lossless language types, all six operation builders, REST-fixture parity, explicit null bounds, older-server behavior; run the binding gate tests unchanged. Accept only with independent wire captures and API-reference completeness. |

Test harness (review A17). In `cfg(test)` builds the capability switch is a thread-local
(`src/property_graph/temporal.rs:280-305`), the handlers run on `spawn_blocking` threads
(`src/server.rs:384,429`), and the integration tests in `tests/` link the library built
without `cfg(test)`. Under the cfg, `supported()` is true on every thread of a non-test
build, so the server's temporal tests are integration tests in `tests/` run by the cfg job
(G4); engine unit tests keep the thread-local meaning (`Activation::on()`), and a test that
asserts closed behavior from a non-test build carries `#[cfg(not(vlite_unstable_temporal))]`
with an open twin.

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
annotations propose replacements and remain open for owner ratification. Revision T0
keeps every D number and adds its amendment beside it ("Amended T0"); coordinator
rulings are provisional and are not owner ratification, and the OPEN DECISION blocks
that remain are indexed after the table.

| Decision | Options | Recommendation / blocking consequence |
|---|---|---|
| D1: signed JSON time and ids | Decimal strings with exact integer input; integers only; formatted dates | Formerly: Approve: times take strings or integers within ±(2^53-1), larger integers refused; ids/revisions keep today's string-or-integer input (`src/server/graph.rs:4-8`), clients send strings. **Amended T0 (review A1; wire conventions decision 12A, owner-selected):** times take canonical decimal strings or exact JSON integers over the whole i64 range with no ±(2^53-1) cap, read by one shared i64 helper beside the u64 helpers; a client whose number type cannot hold an integer exactly sends a string; ids/revisions keep today's string-or-integer input. Blocks serializer/client goldens. |
| D2: feed negotiation | Explicit existing-route opt-in plus per-subscription version; new feed endpoints; implicit new vocabulary | Formerly: Approve `temporal=true`/`temporal_v1`. Legacy feeds refuse temporal files before any event, and a client that asked for temporal fails `unsupported_feature` without `feed_version`. Blocks feed and activation acceptance. **Withdrawn:** separate temporal negotiation; the shared feature set and echo replace it ([wire conventions, decision 8 (owner-selected)](serving-wire-conventions.md#6-owner-decisions-23)). **Amended T0 (ruling C; round 2): PARKED with T4**, no owner and no activation prerequisite: this contract specifies no temporal feed negotiation, requires the explicit refusals of section 6 and names the vocabulary a non-goal. |
| D3: Gap 12 tag manifest | Formerly: Shared tag-100 extension points; allocate different reviewed tags; JSON-only RPC payloads; new stream messages | Formerly: Approve these typed extensions after resolving Gap 12 shared names and the GraphReadMeta field manifest. One combined descriptor review is required; never guess occupied tags. Tag/enum 100 on the existing change messages amends `serving-contract.md:220` (then cited as line 206); approve that explicitly. **Withdrawn:** tag-100 temporal allocation conflicts with the ALG band; propose section 7's shared tag 200. [Wire conventions, decision 1 (owner-selected)](serving-wire-conventions.md#6-owner-decisions-23). **Amended T0:** the feed allocations (change tag 23, enum value 10, feed fields 3/4/5) are parked with T4 and removed from section 7; the tag-200 extensions are proposals for the gRPC lane and no activation prerequisite; `serving-contract.md:220` needs no amendment. |
| D4: remote TTL clock | Server-owned C only; explicit admin clock override | Keep server-owned C and return it, never allow a remote caller to resurrect retained-but-expired data. Blocks read request schema approval. **Amended T0 (review B3; Coordinator ruling, provisional, round 2):** a forged cursor could carry C, so the server seals every flag-64 read cursor with a per-process HMAC-SHA-256, with no wire change; forged, stale and other-process cursors answer the same `graph_cursor_stale`; a cursor is valid only on the issuing process (section 3). |
| D5: activation readiness | Qualify all serving/feed surfaces before acceptance; permit explicitly unsupported bindings to remain closed | Formerly: Require complete server/feed support before server acceptance; keep individual bindings closed until separately qualified. No per-request weakening and no gate/default changes here. **Amended T0 (Coordinator ruling C, provisional, round 1; review A14):** D5 governs activation qualification only. Every graph surface either supports temporal (REST, MCP, CLI) or refuses with 409 `unsupported_feature`; the temporal feed, webhooks and gRPC are follow-ups and no activation prerequisite, and they refuse explicitly on a flag-64 file (section 6). Individual bindings stay closed (G6). |
| D6: wire failure/deadline | Explicit transport errors plus bounded incomplete read envelopes; always discard budget results | Approve section 3 precedence/caps/error codes; graph budget can report incomplete, transport cancellation/deadline cannot report success. gRPC deadlines come from `grpc-timeout`; REST has no client deadline and passes `cancel: None` (section 3), so REST propagation is NEW. Blocks parity/error goldens. [Wire conventions, decision 4 (owner-selected)](serving-wire-conventions.md#6-owner-decisions-23); transport expiry/cancel follows its section 2.2. **Amended T0 (review A11):** the first REST/MCP/CLI slice needs only the engine-budget incomplete envelopes; the transport deadline and REST cancellation are deferred, with the cancel mechanism named in section 3. |
| D7: public activation API and CLI | Keep `enable_temporal` crate-private; public `maintenance::enable_temporal` plus `vlite-maintain enable-temporal`; library only | Formerly: After activation qualification, add both. Encrypted sources: `--key-env NAME` (never a key argument) or plaintext only. A public Rust API needs its own `site/api.html` row and is not authorized here (`gap4-temporal-graph.md:500`; section 4 cites the rest). **Amended T0 (Coordinator ruling D, provisional, round 1; review A12):** both ship after qualification, cfg-only until step 6; the key comes from the existing options (`--encryption-key-file` or `VLITE_ENCRYPTION_KEY[_HEX]`, exactly one source), never a key argument and no `--key-env`; the `site/api.html` row is added when the cfg is deleted (G7); activation of any file still needs the owner. |
| D8: `UnsupportedFeature` REST mapping | Flag 64 only; every flag; keep 500 | Formerly: Flag 64 only: 409 `unsupported_feature` with NEW `ApiError` members (`feature` body field, `vlite-feature` trailer); other flags keep today's 500 (`src/server.rs:1752`). No new public `Error` variant. Blocks refusal goldens. **Withdrawn:** per-draft members/trailer would fork error context; propose scoped handler-built `ApiError` with `details.feature` and `details.category` under section 3, preserving legacy routes. [Wire conventions, decision 7 (owner-selected)](serving-wire-conventions.md#6-owner-decisions-23); Wire conventions, decision 21 (owner-selected). **Amended T0 (review A6):** `details.feature` is the string `"64"`, never the integer, and REST `details` is itself NEW. |
| D9: new wire codes | Formerly: `request_too_large`, `response_too_large`, `graph_timeout`, `graph_cancelled`; reuse 413 `invalid_json`, 503 `busy`; none | Formerly: Provisional. Today 504 maps to INTERNAL and 408/413 to INVALID_ARGUMENT (`src/server/grpc.rs:207-231`); the vocabulary is settled in the cross-document wire review before any REST/gRPC golden. **Withdrawn:** the domain-specific request/deadline/cancel vocabulary conflicts with the shared registry. Proposed replacement: section 3 and [wire conventions section 2](serving-wire-conventions.md#2-error-registry-c2-c9), including `response_too_large` to RESOURCE_EXHAUSTED and explicit `deadline_exceeded` to DEADLINE_EXCEEDED, with no wire cancellation code. Wire conventions, decision 4 (owner-selected). **Amended T0 (review A2; decision 3A):** `response_too_large` is gRPC-only; REST has no response cap and no 413 `response_too_large`; `deadline_exceeded` is deferred (review A11). |
| D10: wire caps | Temporal-only 1/8/16 MiB; uniform for all graph requests; none | Temporal-only. REST has no response cap today and an 8-16 MiB write the engine accepts is refused by transport (cites in section 3). [Wire conventions, decision 3 (owner-selected)](serving-wire-conventions.md#6-owner-decisions-23). **Amended T0 (review A2; decision 3A):** the 16 MiB response cap is gRPC-only and REST, MCP and CLI have none; the timeline is bounded by its index window and by charging returned segments against the read budget (section 3); the request caps are unchanged. |
| D11: webhook migration | Formerly: Shared cursor and legacy key; per-subscription progress with `feed_version`; separate temporal dispatcher | Formerly: Per-subscription progress; dedup key unchanged, a lineage key needs a new payload field. Contradicts `docs/WEBHOOKS.md:45,52`; `deny_unknown_fields` blocks downgrade. Needs its own qualification before activation serving. **Withdrawn:** separate versioned progress duplicates cursor ownership; propose immutable feature sets and per-set dispatcher progress, preserving the empty-set path/format. [Wire conventions, decision 8 (owner-selected)](serving-wire-conventions.md#6-owner-decisions-23). **Amended T0 (round 2): PARKED with T4.** Blocking every subscription on a flag-64 file is required now, needs no schema change and is decoupled from this decision (review A13, section 6); the lineage-less dedup key and the downgrade procedure belong to the parked follow-up. |
| D12: audit and metrics | Section 4 fields and labels; existing fields only; none | Approve the NEW fields and bounded labels (cites in section 4); never record keys, bounds, properties or secrets. **Amended T0 (review A10):** grants are evaluated once at admission, so the earlier "recheck mutable grants before queued execution" is withdrawn (section 4); activation attempts are offline tool runs and not server metrics. |
| D13: NEW tools, CLI and client methods | All NEW; REST/gRPC only | Formerly: Approve MCP `vlite_graph_edge_timeline`/`vlite_temporal_changes`, CLI `edge-timeline`/`temporal-changes` and the HTTP graph accessors (none exist today, section 1); each is documented in its slice. **Amended T0 (reviews A3 and A4; decisions 10A and 8A):** MCP `vlite_graph_edge_timeline` and CLI `edge-timeline` only; `vlite_temporal_changes` and `temporal-changes` are withdrawn (a temporal-only feed tool violates 8A; a follow-up feed tool is a generic `vlite_graph_changes`, parked with T4); the HTTP client methods are flat (`graph_edge_timeline`, `graphEdgeTimeline`, `GraphEdgeTimelineAsync`), not a `client.graph(name)` facade, and arrive late (slice T6). |

### Index of rulings and open decisions (revision T0)

| Item | Status | Where |
|---|---|---|
| B1: build gate `--cfg vlite_unstable_temporal`, requirements G1-G9 | Coordinator ruling, provisional, round 2 (supersedes the round-1 Cargo feature) | [section 4](#4-capability-activation-and-authorization) |
| B1b: shape of the opt-in entry point for flag-64 files | **OPEN DECISION** (owner, at step 6; not a T1 blocker) | section 4 |
| B2: the gate closes the wire as well as the file | applied (G3, G8) | sections 4 and 7 |
| B3: forged cursor | Coordinator ruling, provisional, round 2 (server-side MAC) | section 3 |
| B4: unbounded temporal commits | Coordinator ruling, provisional, round 2 (both parts) | section 3 |
| B5: retrieve redaction | Coordinator ruling, provisional, round 2 (409 and a named non-goal) | section 4 |
| B6: public items for the in-package MCP and CLI | **OPEN DECISION** (owner) | section 5 |
| B7: capability discovery | **OPEN DECISION** (owner) | section 4 |
| Ruling C: D5 governs activation qualification only | Coordinator ruling, provisional, round 1 | section 4 |
| Ruling D: public `enable_temporal` and `vlite-maintain enable-temporal` | Coordinator ruling, provisional, round 1 (cfg-gated by round 2) | section 4 |
| Ruling E: permanent erasure barrier stays; flag-64 files are one-way; the "0.7.1 ignores it" wording does not match the code (verification note in section 4, AC-12) | Coordinator ruling, provisional, round 1; the 0.7.1 wording awaits the coordinator's confirmation | sections 4 and 10 |
| T4 (temporal feed) and the gRPC arms | parked or another lane; the refusals are required now | sections 6 and 7 |
| Activating any file; deleting the cfg | not decided here; needs the owner (steps 5 and 6) | sections 10 and 12 |
| Memory fact routes | not decided here; an explicit owner decision | sections 11 and 12 |

Reviewer notes (the review's verdict table is not copied). D1, D10, D11 and D13 drew a
disagreement from at least one reviewer and are amended above (A1, A2, A13, A3 and A4); D4
was agreed in principle and broken by forged cursors (B3, ruled); D5 and D7 were agreed with
amendments (A14; A12 and the rollback documentation of section 10); D8 and D9 were agreed
once `details.feature` is a string and REST has no `response_too_large` (A6, A2); D6 and D12
asked for named mechanisms (A11, A10); D2 and D3 were agreed subject to the shared feed
(parked) and the Gap 12 review.

Open implementation items: review schema references against the Gap 12
descriptor once it is approved (the gRPC lane); verify batch no-op outcomes, full-series byte accounting and the
bounded frames of B4; settle the open decisions indexed above; publish the support/refusal
matrix (section 4) before any release. Parked: the temporal feed (T4) and the webhook
migration (D11). Deferred: the transport deadline and REST cancellation (review A11).
Nothing here implies production readiness. Documentation checks: `python tests/release_docs.py`,
`python tests/content_budget.py`, `python tests/agent_docs.py`,
`git diff --check`.

## 10. Activation runbook (mandatory for activation acceptance)

Status: proposed requirement (TE-011; review C1). This section fixes what the activation
procedure and its drills must contain. It does not activate, schedule or recommend
activating any file: activating a file and deleting the build gate each need the owner
(sections 4 and 12), and the operator pages it requires are written with the opt-in step
(step 5 of [section 12](#12-rollout-sequence-and-implementation-order)), not before.

> **Coordinator ruling (provisional, 2026-11-06, round 2): the runbook and the drills are a criterion of activation acceptance (lane slice T1).**
>
> Acceptance of the activation step is blocked until the checklist below is in the
> operator documentation and every drill DR-1..DR-10 has a recorded pass. Owners: docs
> (checklist and runbook text), maintenance tooling (the commands), CI (the drill
> harness). Activation is one-way and whole-file. Facts reach users only after an
> explicit owner decision, which this contract does not take.

**What the step is.** Activation is the copy-out `enable_temporal`
(`src/property_graph/temporal/activate.rs:37-81`). It reads a closed, graph-enabled source
under its exclusive lock (an open source is refused with `Locked`, `activate.rs:27-35`, so
the server must be stopped first) and writes a NEW file under a NEW lineage whose header
also carries flag 64 and which holds every committed frame of the source unchanged:
sequences, revisions, absolute TTL deadlines and precision are kept, and an encrypted
source is resealed under the same key with a fresh salt (`activate.rs:1-17`). The source is
never modified. The step is therefore:

- **one-way.** No converter returns a flag-64 file to the legacy form, and `strip_graph`
  removes all graph state, temporal state included (`gap4-temporal-graph.md:418,599`);
- **whole-file.** The flag is a header bit of the one shared file (`src/format.rs:72-79`),
  so every tenant, graph and collection moves together and no tenant can opt out;
- **a lineage change for the whole file**, with the consequences below.

User documentation already says that older builds refuse flag-64 files and that the path is
crate-private and gated (`docs/GRAPH.md:265-268`). That text, `docs/TOOLS.md:366`
(`enable-graph`) and `docs/DOCKER-OPERATIONS.md:201-202` (lineage and credentials) are
reconciled when the step is taken, not before.

**Motivating case.** Lane-owner supplied (2026-11-06), unverified in this repository: a live
application has agent memory on, 2 tenants, 6 active memory credentials among about 40
credentials in total, and memory-binding generation 1. For that file the lineage change makes
every scoped credential answer 403 (AC-1), and the six memory credentials are named in the
binding grants, so re-issuing them forces a bindings-file edit with a higher generation
(AC-2).

### Consequence list

Shorthand in sections 10 and 11: a bare `activate.rs` is
`src/property_graph/temporal/activate.rs`; `tests/activate.rs` and `tests/writes.rs` are
files of `src/property_graph/temporal/tests/`; a bare `credentials.rs`, `providers.rs` or
`guard.rs` is under `src/server/`.

Each row is a checklist item with an operator step and a drill. Rows AC-1 to AC-5, AC-7 and
AC-11 largely restate what compaction, upgrade and restore already do to the lineage
(`docs/DOCKER-OPERATIONS.md:201-202`, `docs/RELEASE-0.3.0.md:110-112`); what is specific to
activation is the one-way flag 64 (AC-6 to AC-9, AC-12), the feed and webhook refusal (AC-4)
and the changed read defaults (AC-13).

| # | Consequence | Evidence | Required step; drill |
|---|---|---|---|
| AC-1 | New lineage: every scoped credential of every tenant answers 403 `forbidden_scope`, even on a route that needs no scope. The administrator and read-only tokens are unaffected. Rotation does not repair it. | Create stores the lineage (`src/server/credentials.rs:733-752,765`); `covers` compares it (`credentials.rs:162-165`); it is checked before the scope (`src/server.rs:1073-1076`); rotation clones the old entry (`credentials.rs:851-858`); `docs/DOCKER-OPERATIONS.md:201-202`, `docs/CAPABILITIES.md:189-191` | Before: list the credentials (`GET /v1/admin/credentials`: name, scopes, expiry, holder). After: re-issue each with `POST /v1/admin/credentials`, hand over the new tokens, and revoke the old entries only when the AC-8 window closes. DR-2 |
| AC-2 | Memory bindings: grants name credential IDs, a re-issued credential has a new ID, and a changed grant list needs a higher binding `generation`; without it startup stops. | `src/memory/binding.rs:39-47` (grant principal), `300-307` (generation rule), `149-152` (startup checks the ID format only); `credentials.rs:757` (new ID); `docs/MEMORY.md:84` (`a memory binding changed without a higher generation`), `86-92` (the file is read at startup) | After AC-1: write the new principal IDs into the bindings file, raise every changed binding's `generation` (1 to 2 in the motivating case) and restart with that file. Keep the old file for AC-8. DR-1 |
| AC-3 | Provider bindings: the binding key holds the lineage, so every provider-backed route answers 409 `provider_not_configured` until the configuration names the new lineage. | `src/server/providers.rs:138,516-521` (the configuration's `lineage` is a JSON integer in the key), `src/server/provider_serving.rs:124-130` (lookup uses the opened file's lineage), `providers.rs:541,70` (a miss answers 409 `provider_not_configured`); `vlite-maintain` prints no lineage (`src/bin/vlite-maintain.rs:18-29`), `maintenance::inspect_detail` has it (`src/maintenance.rs:126-137`) | Write the new lineage into every binding and restart. `vlite-maintain enable-temporal` must print the new lineage as the decimal integer the file wants and as the 16 hex digits credentials store (owner: maintenance tooling; test: a golden of the command output). DR-3 |
| AC-4 | Webhooks and feed consumers. The saved `webhooks.cursor` is a `vlc1` cursor of the old lineage, shared by every subscription; a cursor of another lineage fails with `CursorCompacted`, which the dispatcher swallows, so today any lineage change stalls delivery silently. On a flag-64 file feeds and webhooks refuse (ruling C, section 6): `GET /v1/changes` answers 409, every subscription is blocked with `last_error` `unsupported_feature:64`, and nothing is delivered until the parked temporal feed (T4) lands. Activation therefore stops every feed and webhook consumer of the file; the owner must know that before step 6. | `src/server/webhooks.rs:81-87,448` (the saved cursor), `429-437` (feed errors swallowed); `src/changes.rs:439-443`; `docs/WEBHOOKS.md:45,52-53` (one shared cursor, no per-subscription cursor) | The checklist lists every subscription (`GET /v1/admin/webhooks`) and feed consumer and records that they stop. The source's `webhooks.cursor` is not touched, so a rollback (AC-8) resumes where it stopped. Reseeding consumers on the copy is not specified here (parked with T4; the copy keeps sequences, `activate.rs:7-10`, but no re-anchor procedure exists). DR-4 |
| AC-5 | Replicas: a follower of the source refuses every segment of the copy and stays unchanged; followers of the copy are created from it afresh. | `src/replica.rs:144-148` (another lineage refused), `44-83` (`create` from a first segment), `195-200` (`promote`); engine test `tests/activate.rs:492-549`; `docs/MEMORY.md:92` (with a memory binding configured, replication export answers 403, so such a file's followers are file-level) | Stop and re-create every follower from the copy; never promote a follower of the source into the copy's role. DR-5 |
| AC-6 | Backups and point-in-time restore: the source's chain keeps restoring the source (not temporal) and cannot anchor the copy's log; the copy's own history restores at any revision, including one before its first temporal write, as a flag-64 file. Every restore is a new lineage again. | `tests/activate.rs:413-443` (`base backup has a different lineage than the log`), `445-487` (restores of the copy); `src/logops.rs:165-171` (`restore_to` copies the header bytes and replaces only the lineage); `docs/STORAGE-FEATURES.md:67-72` | Immediately before activation take and verify a backup of the source (the rollback point). After it start a new chain from the copy and verify a restore of it (`vlite-maintain drill BACKUP WORKDIR`, `src/bin/vlite-maintain.rs:7-9,65-72`) before the old chain is retired. A restore repeats AC-1 to AC-4. DR-6 |
| AC-7 | Compaction, upgrade and directory conversion keep flag 64, start a new lineage and renumber revisions, so callers holding an `Expected::Revision` from before get a precondition failure. | `src/maintenance.rs:493-507` (`compact_inner`), `545-554` (`upgrade`), both carry the graph flags with flag 64; engine test `tests/writes.rs:763-852` (compaction and conversion only); `docs/GRAPH.md:268`; `gap4-temporal-graph.md:596-601` | The runbook repeats AC-1 to AC-4 after each. DR-7 |
| AC-8 | No reverse converter: rollback is serving the SOURCE and losing every write made on the copy. The source's backups, restore and replicas continue on its own lineage. | `activate.rs:11-14` (the source is never modified); engine tests `tests/activate.rs:155-230` (the source is untouched), `413-429` | Keep, until an explicit decision closes the window: the source file, the old credentials (do not revoke), the old bindings and provider files, the saved `webhooks.cursor` and a copy of the admin directory. DR-8 |
| AC-9 | Builds without acceptance, `v0.7.1` included, refuse the file, its backups, restore outputs, exported log segments and replica bootstrap segments before any frame is loaded. | One gate serves all of them: `read_log_header_with` (`src/format.rs:1423-1460`), used by the loader, `export_log_segment` (`src/logops.rs:333,436-437`) and `ReplicaFollower::create`, which removes its half-written file on error (`src/replica.rs:55-82`); text `src/lib.rs:638-641`. At the `v0.7.1` tag the gate is the same code (`git show v0.7.1:src/format.rs` lines 1449-1456), and the three gate files `src/format.rs`, `src/property_graph/temporal.rs` and `src/property_graph/engine/temporal.rs` do not differ between that tag and the base `f8e9f3a4`; `docs/GRAPH.md:265` | The release notes and the runbook say so. DR-9 |
| AC-10 | Every host that opens the file or one of its copies must run a build that accepts flag 64 before the step: replicas, restore hosts, drill and backup-verification hosts, CI and the rollback host. | AC-9 | The checklist records every host and its build; the release that opens the gate states the minimum versions (step 6). DR-10 |
| AC-11 | Other lineage-bound state is re-acquired or rebuilt: collection tags, the disk-ANN sidecar cache, change consumers' cursors. | `docs/REFERENCE.md:340-343` (tags after compaction, upgrade and restore), `docs/ANN-SNAPSHOTS.md:26-31` (the cache identity holds the lineage and a stale cache is rejected), `docs/RELEASE-0.3.0.md:110-112`; that a copy-out activation invalidates tags is inferred from the same rule, not verified | The checklist repeats the three items; the drills record whether a tag taken before the step is refused afterwards. DR-2, DR-6 |
| AC-12 | Erasure barrier (ruling E): job-node presence is the permanent barrier. A build that does not run the barrier hooks ignores the nodes, so a pending or completed scope becomes readable and writable again, and a later roll-forward cannot erase what was written meanwhile. A flag-64 file is one-way. | `src/memory/erasure.rs:1-6`; `gap1-e2-resume-contract.md:502-508` ("Do not roll back while any job exists. This is a one-way feature to document at serving.") names 0.7.1 as such a build. The code does not support that: `v0.7.0` and `v0.7.1` both contain the E1 hooks (the E1 commit `cd9a01f6` is an ancestor of both tags and `src/memory/erasure.rs` is in both), `v0.6.1` and older contain no memory code, and `gap1-adapter-mapping.md:468` (ES14, proposed) says 0.7.1 honours the barrier at every memory hook but has no route to read, resume or clear a job | The docs say that a flag-64 file is one-way, that a rollback build must run the barrier hooks, and not to roll back to a build that does not while any erasure job exists. The wording "0.7.1 ignores the barrier" of ruling E is not repeated until the coordinator confirms or withdraws it. DR-8 |
| AC-13 | The one silent change: a read that omits `as_of_ms` evaluates at T=C, so an edge that is not valid now is no longer returned, and every read response of a temporal file gains `as_of_ms` and `ttl_clock_ms`. Ordinary edges have no segments and stay visible; non-activated files are unchanged (TE-008). | Section 3 (reads); `src/property_graph/temporal/read.rs:19-39,68-79` | Tell client owners; clients that ignore the `as_of_ms` echo must be listed. DR-1 compares ordinary reads before and after. |

### Mandatory activation checklist

The operator documentation must carry this checklist, and an activation record must show each
item signed off. Owner: docs.

1. The owner decision to activate this file is recorded (outside this document), and the
   target build and the rollback build are named.
2. Inventory (AC-1 to AC-4, AC-10, AC-11, AC-13): credentials with scopes and holders;
   memory bindings with their generations; provider bindings; webhook subscriptions and feed
   consumers (they stop, AC-4); replicas and backup chains; every host of AC-10 on a build
   that accepts flag 64; client owners told of AC-13.
3. Rollback point (AC-6, AC-8): a fresh, verified backup of the source, plus copies of the
   admin directory, the bindings file, the provider file and `webhooks.cursor`.
4. Stop every writer and the server; the source must be closed.
5. Run `vlite-maintain enable-temporal SOURCE DESTINATION` (encrypted: the existing key
   options, section 4), record the printed lineage and `verify` the copy.
6. Start the server on the copy with the administrator token only; confirm the feed and
   webhook refusals (AC-4) and an ordinary read.
7. Re-issue the credentials (AC-1), write the new principal IDs and raised generations into
   the bindings file (AC-2), write the new lineage into the provider bindings (AC-3) and
   restart.
8. Re-create every replica from the copy (AC-5), start a new backup chain and verify a
   restore of it (AC-6).
9. Run the post-activation smoke: the checks of DR-1 to DR-4 on the real file.
10. Keep the rollback window open (AC-8) until an explicit decision closes it; only then
    revoke the old credentials and retire the old chain.

### Drills

Each drill is run against a copy-out of a representative file with the real operator
commands. Owner of every drill: docs for the procedure and CI for the harness; the owner of
the behavior under test is named per row.

| Drill | Scenario | Pass criteria | Owner; test |
|---|---|---|---|
| DR-1 memory bindings (AC-1, AC-2, AC-13) | A file with the graph enabled, two tenants, one memory binding per tenant at generation 1 with an evidence collection and remembered memories, and the credentials its grants name. Copy it out, start the server on the copy with the old bindings file, re-issue the credentials, write the new principals with generation 2 and restart. | The server starts with the unchanged file and the old memory credentials answer 403 `forbidden_scope`; after the re-issue `recall` and `receipts/lookup` return the same memory IDs and receipts as on the source (read responses also carry `as_of_ms` and `ttl_clock_ms`); the new principals with generation 1 stop startup with `a memory binding changed without a higher generation`. | Server adapter. `tests/temporal_activation_drills.rs` (proposed), modeled on `tests/server_memory_binding_record.rs`, run by the cfg job |
| DR-2 scoped credentials (AC-1, AC-11) | The same file with about 40 scoped credentials of different shapes (collection-bound, graph-bound, wildcard, expired, revoked), an administrator token and a read-only token. Copy out; call a no-scope route and each credential's own route; rotate one; re-issue all; call again; read a collection tag before and after. | Every scoped credential answers 403 `forbidden_scope` on both calls and both tokens work; the rotated credential still answers 403; a credential re-issued with the same name, scopes and operations succeeds and the re-issue count equals the inventory; the drill records whether the old tag is refused (unverified). | Server adapter. The same harness |
| DR-3 provider bindings (AC-3) | A provider configuration with one embedding alias bound to a tenant and collection on the source lineage. Copy out; call a provider-backed route with the old configuration; write the lineage printed by `enable-temporal`; restart; call again. | 409 `provider_not_configured` with the old configuration, success with the new lineage, and the printed lineage is in the form the file wants. | Server adapter, maintenance tooling. The harness with `src/server/providers/tests.rs:1218-1229` as the precedent for building a binding from `database.lineage()`; a golden of the command output |
| DR-4 webhooks and feeds (AC-4) | A subscription with a saved `webhooks.cursor`, a REST feed consumer holding a cursor, events pending. Copy out; start the server on the copy. | `GET /v1/changes` answers 409 `unsupported_feature` before any event for every query form, an old-lineage cursor included; each subscription shows `last_error` `unsupported_feature:64`; nothing is delivered; the bytes of `webhooks.cursor` are unchanged; after a rollback to the source (DR-8) delivery resumes from the saved cursor. | Server adapter. The section 6 refusal goldens, with `tests/webhooks.rs` and `tests/graph_change_feed.rs` as precedents |
| DR-5 replicas (AC-5) | A primary with a file-level follower (and a server follower where the server serves replication). Copy out; apply a segment of the copy to the old follower; create a follower from the copy's first segment; write on the copy and sync. | The old follower refuses with `ReplicaDiverged("segment is from another lineage")` and its applied sequence and content are unchanged; the new follower is a flag-64 file and reaches the copy's head with the same series. | Engine (existing coverage `tests/activate.rs:492-549`); the harness adds the server-served path where it exists |
| DR-6 backups and restore (AC-6, AC-11) | The source and the copy each with a backup chain. Restore the copy's log at the activation sequence, at a revision before its first temporal write and at head; restore the source's chain; try the source's base backup against the copy's log; run `vlite-maintain drill` on a backup of the copy. | Every restore of the copy is a flag-64 file with a new lineage and the expected content; the source's chain restores the source without flag 64 and is refused as a base for the copy; the operator drill passes. | Engine (existing coverage `tests/activate.rs:399-487`); the harness adds `vlite-maintain drill` and the credential, provider and webhook consequences of each restore |
| DR-7 compaction and upgrade (AC-7) | `compact`, `compact_with_key`, `upgrade`, directory conversion and a checkpoint of the copy. | Each output keeps flag 64 and the graph flag `FLAG_GRAPH`, keeps every series, accepts a correction with a fresh revision and has a new lineage; an `Expected::Revision` taken before fails with 412 `precondition_failed`. | Engine. Extend `tests/writes.rs:763-852` (proposed `upgrade_of_a_temporal_file_keeps_flag_64`, `encrypted_compaction_keeps_flag_64`; no test covers those two paths today, unverified) |
| DR-8 rollback (AC-8, AC-12) | Write on the copy after the step (ordinary and temporal commits), stop it, start the source under the build that ran before, with the saved admin directory, bindings file, provider file and `webhooks.cursor`. | The source serves its own content; every commit made on the copy is absent and the report states how many were lost; the source's bytes are identical to those before the step; the old credentials, bindings (generation 1), provider file and cursor work as before; nothing restores the copy's temporal edges on the source. | Docs. The harness: a byte comparison of the source, a start of the rollback build, a count of lost commits |
| DR-9 closed builds refuse every artifact (AC-9) | Under the cfg produce an empty and a populated flag-64 file, a `backup`, a `restore_to` output, an exported log segment and a replica bootstrap segment; try each with the closed builds (the shipped release binaries and the `v0.7.1` binaries: `vlite-server`, `vlite-maintain inspect`, `verify` and `drill`, `vlite-graph-cli`, `vlite-mcp`) and with the embedded bindings. | Each refuses with `UnsupportedFeature(64)` (the server and tools exit non-zero with `database header declares unsupported feature flags 0x0040`, `src/lib.rs:638-641`; the graph CLI answers its existing `unsupported_feature` error, `src/bin/tool_support/graph_json.rs:45`), before any frame is loaded and leaving no output file. The release smoke of G5 is this drill's CI form. | CI, docs. The closed twins of the gate refusal tests (`crates/vlite-graph/tests/temporal_gate.rs`, `src/property_graph/temporal/tests.rs:222-249`), the G5 smoke and a one-time run of the `v0.7.1` binaries at step 6 |
| DR-10 host readiness (AC-10) | An inventory of every host that opens the file or a copy (replicas, restore, drill and backup-verification hosts, CI, the rollback host) with its build version. | Every host runs a build that accepts flag 64 before the step, or is recorded as refusing it; the minimum versions in the step-6 release notes equal the inventory's; a restore on a host below them refuses with no partial state (DR-9). | Docs. The checklist review in the activation record, with DR-9 as evidence |

Harness. The drills are scripted in one integration test file run by the cfg job (G4), with
the real operator commands, and each run writes a short report (inputs, observed output,
pass or fail) that is attached to the step-6 decision; the first run settles every behavior
this section marks unverified. Mutants that must fail a named drill: skip the generation
raise (DR-1 startup refusal), skip the lineage rewrite (DR-3), let the feed answer on a
flag-64 file or swallow its refusal (DR-4), re-anchor a follower silently (DR-5), restore
without the flag (DR-6), drop flag 64 in `compact` or `upgrade` (DR-7), mutate the source
while copying (DR-8). The engine-level behavior is already covered by the tests of
`src/property_graph/temporal/tests/activate.rs`: a copy keeps every frame and leaves the
source untouched (155), refusals publish nothing (231), publication faults leave no partial
copy (290), an encrypted copy is resealed under the same key (349), backups and restore on
both sides (399), replicas reseed from the copy (492), a directory store copies out (553)
and a precision image copies out with its catalog (595).

## 11. Memory-facts guarantees

Status: proposed (TE-012; review C2). The memory fact routes `/facts/query` and
`/facts/correct` answer 409 `memory_capability_unavailable` with `details.feature` `facts`
today (`docs/MEMORY.md:25,438`), and `recall` refuses `as_of_ms` with feature
`temporal_read` (`docs/MEMORY.md:441`). They reach users only after an explicit owner
decision (step 8 of [section 12](#12-rollout-sequence-and-implementation-order)); nothing in
this contract serves them or specifies their wire. This section lists what the temporal
batch, timeline and (later) feed must guarantee so that memory needs no further wire change
(MF-1 to MF-6), and the two facts memory cannot avoid (MF-7 and MF-8). Memory writes through
the memory core in process, not through the REST temporal batch, so the reserved-graph
refusal of section 4 closes only the raw route. Owners and tests are per row.

| # | Guarantee | Owner | Case |
|---|---|---|---|
| MF-1 | Revision checks are taken at batch start across mixed operations: every `expected` revision of a batch is compared with the state before the batch, whatever the order and the kinds of its operations (temporal, ordinary, vector, TTL). Close plus append of one series is one atomic supersession, and a stale revision on any one operation refuses the whole batch with 412 `precondition_failed` and changes nothing. | Engine (the rule exists: `src/property_graph/temporal.rs:43-47,57-60`, `docs/GRAPH.md:262`); server adapter (wire) | Existing engine tests `tests/writes.rs:355` and `tests/writes.rs:660`; a REST golden: close plus append in either order, an ordinary patch before a correction of the same edge, a stale revision on the second operation |
| MF-2 | A committed revision per operation: every outcome of a batch carries the revision its edge has after the commit (one value for the operations on one edge); a wholly no-op batch has top-level `revision` null while its outcomes keep the current series revision. A caller can use an outcome as the next `expected` without reading again. | Server adapter | A golden per operation kind (create, promote, append, replace, close, remove), including a no-op and the removal of the last segment (the empty series keeps its identity and revision) |
| MF-3 | Closing a segment at its exact existing end is a no-op: after the revision and segment checks it changes nothing, commits nothing and returns the current revision, so a retried correction is idempotent. | Engine (`src/property_graph/engine/mutate/temporal.rs:139-156`) | The no-op case inside `tests/writes.rs:355` (comment at 408); a REST golden that a retried close returns the same revision and `revision` null |
| MF-4 | Timeline reports `total_segments` (the count before slicing), the requested half-open indices unchanged and ascending retained segments; a range past the length is sliced at the actual length; a legacy edge is 422 `invalid_input` with category `not_temporal`. A series has at most 256 segments, so memory can read a whole fact history in windows of at most 256. | Engine (`src/property_graph/temporal/read.rs:299-348`); server adapter | The timeline goldens of section 8 (0/0 and 256/256, start greater than end, 257, range past length, changed revision between ranges) |
| MF-5 | A distinct `details.category`: capability is separate from the engine reasons, so memory maps the capability refusal to `memory_capability_unavailable` (`serving-memory-contract.md:503`) and the reasons to its invalid-input family without parsing a message. One crate-private classifier, shared by the temporal handlers and the memory adapter, maps each engine message to the status, code and category of the section 3 table. | Server adapter | A table test over the twelve reason constants and the capability refusal (`src/property_graph/engine/temporal.rs:41-55`), asserting the section 3 table row by row |
| MF-6 | The same per-graph authorization hook as memory's planned `raw_access:false` policy: memory operation grants never authorize raw graph access, and the check runs at every raw server adapter before lookup (`serving-memory-contract.md:460-472`). The temporal batch, timeline and the parked feed pass through the same guard (`RequestScope::guarded_graph`, `src/server/guard.rs:320-333`; a hidden graph answers as missing, `guard.rs:427-446`), and a batch with a temporal operation on a reserved graph is refused as a whole even for a `raw_access` credential (section 4, review A9). | Server adapter | The authorization matrix of section 4, including the reserved-graph rows, and a case that a memory binding's own routes still work with the rule in place |
| MF-7 | `MemoryFeed` is a closed schema: `temporal_complete`, `ttl_complete` and `consumer_purge_acknowledged` are `enum [false]` and `additionalProperties` is false (`src/openapi.json:177`; `serving-memory-contract.md:566-572`), so memory cannot advertise a temporal feed without a schema change, and every memory mutation on a flag-64 file still answers `temporal_complete:false`. `require_complete_feed:true` stays 409 `memory_feed_unsupported` before any write (`serving-memory-contract.md:504,570-572`). This contract adds nothing to the memory feed block; its temporal feed is parked (section 6). | Docs, server adapter | A schema-validation golden over memory mutation responses on a flag-64 file |
| MF-8 | Facts need the whole shared file to go through activation: one-way, all tenants (section 10). The memory credentials and bindings change as AC-1 and AC-2 say, and the fact routes stay 409 `memory_capability_unavailable` (`details.feature` `facts`) on a flag-64 file until the explicit owner decision. | Docs, server adapter | DR-1, and a golden that `/facts/query` and `/facts/correct` still answer 409 on a flag-64 file in the open build |

## 12. Rollout sequence and implementation order

> **Coordinator ruling (provisional, 2026-11-06, round 2): the architect's eight steps are adopted as the rollout sequence** (review C3).
>
> Steps 5 and 6 are owner-gated: they do not start without an explicit owner decision
> recorded outside this document. Steps 2 and 3 can start once the owner ratifies this
> contract; this document does not claim that has happened. Steps 1 to 4 change no released
> byte: the closed build's public item list, OpenAPI and `/v1/capabilities` stay
> byte-identical (G3). The sequence orders dependent work; it is not a schedule.

| Step | What lands | Owner; slice | Exit evidence | Owner-gated |
|---|---|---|---|---|
| 1 | The corrections of this document and the gate: check-cfg in both crates, the ORed `supported()` gates, the closed-build goldens, the dedicated cfg CI job, the release assertions and smoke, the contributor how-to and the OpenAPI overlay mechanism (G1 to G9). | Engine, CI, docs; `gap4e-gate` (lane T1) | Every closed-build golden unchanged; the open twins of the gate refusal tests pass in the cfg job | Starts after the owner ratifies the corrections |
| 2 | The shared prerequisites, each a small separate add-only commit (below). | Server adapter | Each commit alone leaves the closed-build goldens unchanged | Starts after the owner ratifies this contract |
| 3 | Behind the gate: the Rust as-of reads and timeline, the REST reads, timeline and temporal `GraphBatch` branch, the capability member (B7) and the MCP and CLI tools; with them the cursor MAC (B3) and the bounded frames (B4). | `gap4e-engine` and `gap4e-rest` (lane T2), `gap4e-mcp-cli` | The section 8 matrix rows for those surfaces; the old-file and closed-build goldens | Starts after the owner ratifies this contract |
| 4 | PARKED: the temporal feed (T4) and the graph-TTL feed step 5. No owner, off the critical path. The refusals of section 6 are required now. | None; `gap4e-feed` | None specified by this contract | Not applicable |
| 5 | Behind the gate: the public `maintenance::enable_temporal` and `vlite-maintain enable-temporal` (the key through the existing key options), the runbook text and the drills of section 10. | Maintenance tooling, docs, CI; `gap4e-gate` | DR-1 to DR-10 recorded as passed (TE-011) | **Yes** |
| 6 | Delete the cfg in a minor release that declares activation one-way and states the minimum replica and restore versions. | The owner decides; engine, CI and docs execute | The criteria below | **Yes** |
| 7 | After Gap 12 (another lane): the gRPC arms, then the flat HTTP client methods. | Another lane (gRPC, `bindings/**`); `gap4e-clients` (lane T6) | The section 8 rows for gRPC and the clients | After Gap 12 |
| 8 | The memory fact routes. | Memory lane | Section 11 | **Yes**: an explicit owner decision |

Criteria for step 6 (all must hold before the owner is asked to take it):

1. Every item of steps 1, 2, 3 and 5 is closed: the slices of section 8 are accepted with
   independently reproduced negative cases.
2. The oracle-parity goldens pass, negative cases included (section 8; the independent
   oracles of review C4).
3. `v0.7.1` and the gate-closed builds refuse every artifact (DR-9).
4. The embedded bindings (Python, Node, WASM, C), MCP and the graph CLI still refuse flag-64
   files at open (G6), unless the owner decides B1b otherwise.
5. The REST feed, webhooks and the gRPC feeds still refuse (section 6).
6. B1b, B6 and B7 are decided and implemented as decided; a public item that B1b or B6
   adds gets its api-surface and API-reference rows (G7) in the same release.
7. The runbook and the drills passed, with the reports attached (section 10).
8. The release notes state: one-way and whole-file; the consequences AC-1 to AC-13 in
   summary; the minimum replica and restore versions; the feed and webhook refusal; the
   erasure-barrier rollback rule (AC-12).

### Implementation order and commit hygiene

**Shared prerequisites.** Three pieces that every temporal surface needs, and that other
lanes may need too, land first. Each is a small separate add-only commit that changes no
released byte, is reviewed alone and runs the closed-build goldens:

- (a) **The error registry with `error.details` on REST** (wire conventions section 2 and
  decision 21). `ApiError` carries an optional details object that the REST body emits only
  where a handler sets it; the memory adapter's own builder (`src/server/memory/wire.rs:103`)
  is untouched, and the `Error` schema extension of section 7 lives in the overlay. Test:
  every existing REST error golden is byte-identical.
- (b) **The decision-12 integer helper**: one i64 reader beside the u64 helpers (section 3);
  no released route uses it before step 3. Test: the extremes of section 3.
- (c) **The decision-23 batch mechanism**: the overlay's `op` literals and one `allOf`
  branch (section 7), mechanism only, with no temporal literal in the closed OpenAPI. Test:
  the served OpenAPI bytes of the closed build (REG-008) are unchanged.

**Shared files.** `.github/workflows/ci.yml`, `.github/workflows/release.yml`,
`src/server/features.rs`, `src/server/admin.rs`, `src/server.rs` and the scanners take
add-only changes, each in its own commit, because another lane edits them too.

**Slice ids.** The lane slices T1 (`gap4e-gate`, with the activation tooling and the
runbook), T2 (`gap4e-rest` with `gap4e-engine`), T4 (`gap4e-feed`, parked) and T6
(`gap4e-clients`) come from the F1 slice plan, which is not in this repository. The MCP and
CLI slice has no T id in this document and none is invented here. gRPC and `bindings/**`
belong to another lane, which adds no temporal methods to `clients/**`; this lane adds the
flat HTTP client methods late (section 5). "Decision 76" is the coordinator's decision, as
cited in sections 3 and 4 (no new public item, `Error` variant included, before the owner
opens the gate); it is not recorded in this repository.

**Order inside the sequence.** Step 1 precedes everything. Steps 2 and 3 may overlap only
through the add-only rule above. Step 5 follows step 3, because the drills need the reads.
Step 6 is last and needs the owner. Every slice acceptance runs the documentation checks of
section 9.
