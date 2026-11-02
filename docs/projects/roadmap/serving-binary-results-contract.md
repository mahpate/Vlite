# P5: binary search-result serving contract

Status: one design candidate for independent acceptance; not implemented or released.
The [owner's wire approval](next-major.md#decided-2026-11-02) authorizes exact
schemas to be reviewed before code, not automatic acceptance of this candidate.
Evidence below was checked at `00babff460c58b0d80f179de79cfcc3e50c0ce57`.
Proposed means normative only after acceptance. Open means undecided.
No policy, gate, default or release criterion changes here.

## 1. Scope and requirements

Choose **protobuf over HTTP**, one bounded unary response, for P5 delivery
item (5), approved binary serving. This resolves the choice in
[P5 F7](p5-filters-and-serving.md#f7-binary-responses); it does not replace the
[provider/graph serving contract](serving-contract.md).
Only `POST /v1/search` and `POST /v1/collections/{collection}/search` participate.
Non-goals: binary requests, Arrow, streaming batches, record projection,
metadata/payload/vector retrieval, grouped/geo/sparse/named/disk-ANN search,
graph results, new ranking semantics, storage encodings and gRPC auto/explain parity.
No performance or encoded-size advantage is asserted.

1. **BR-1:** absent Accept and wildcard-only Accept retain JSON. Binary is explicit opt-in.
2. **BR-2:** use the identical authorized search execution and ordered results;
   never requery, rerank, round an ID or sort during serialization.
3. **BR-3:** preserve vector distances, hybrid scores and optional leg values,
   plus explain counters and nullable plan, without a JSON round-trip.
4. **BR-4:** emit exactly one complete, versioned protobuf message under the
   limits below, or an explicit JSON error before committing success headers.
5. **BR-5:** bounded independent decoders reject malformed/truncated responses;
   never return a prefix, partial hits, synthesized zeros for absent optionals,
   or an automatic JSON retry.
6. **BR-6:** retain authorization, tenant isolation, admission, audit, metrics,
   request IDs, expiry and cancellation semantics of the JSON path.
7. **BR-7:** HTTP clients remain JSON by default; embedded bindings, MCP and CLI
   gain no transport negotiation. Existing gRPC messages remain wire-compatible.
8. **BR-8:** independently accepted oracle, encoder, HTTP and client slices
   precede qualification. This design makes no release-qualification claim.

## 2. Current coverage

Evidence keys identify existing behavior only; proposed support follows in later sections.

| Key | Current behavior | Verified evidence |
|---|---|---|
| E1 | Both search handlers accept JSON and return `Json<Value>`; no Accept extractor or binary selection in this path | `src/server.rs:2740-2757,3058-3080` |
| E2 | Vector hits expose decimal-string IDs and numeric distances; hybrid hits expose score, nullable distance/BM25 and leg ranks | `src/server.rs:2760-2789,3007-3016`; typed hybrid source: `src/hybrid.rs:85-98` |
| E3 | Explain adds seven counters and plan (null without an auto plan); plan has optional estimate/index field | `src/server.rs:2719-2737,3072-3079`; `src/stats.rs:40-62` |
| E4 | Search k ceiling is 10,000; rerank has separate validation; execution and observation are shared | `src/server.rs:61,2645-2667,2688-2690,3083-3109,3189-3225` |
| E5 | gRPC Search uses shared search core; protobuf has uint64 IDs, double distances, separate hybrid hits | `src/server/grpc.rs:1066-1079,1118-1123,1159-1199`; `proto/vlite/v1/vlite.proto:263-295` |
| E6 | OpenAPI legacy search documents JSON success; named search has a description-only 200 | `src/openapi.json:118,59` |
| E7 | HTTP JavaScript, .NET and Python transports ask for JSON and parse JSON | `clients/javascript/index.js:123-160`; `clients/dotnet/VLite.Client/Client.cs:54-92`; `clients/python/vlite_client/__init__.py:692-708` |
| E8 | Node/Python embedded search calls the local operation bridge, not HTTP; MCP calls the database and emits JSON; shell prints JSON | `bindings/node/index.js:221-231`; `bindings/python/vlite_embedded/__init__.py:250-264`; `src/bin/vlite-mcp.rs:512-555`; `src/bin/vlite-shell.rs:197-219` |
| E9 | HTTP auth/rate limits precede handlers; response carries request ID/no-store; request observation includes denial audit | `src/server.rs:1117-1183,1203-1298` |
| E10 | Collection opening checks qualified name before existence, then lineage/ID binding; wildcard grants remain ownership-bounded | `src/server.rs:1751-1766`; `src/server/credentials.rs:141-169`; `src/server/tenants.rs:462-466` |
| E11 | Errors use the JSON Error envelope and attach an ErrorCode for observation | `src/server.rs:1407-1422` |

Coverage is limited to these inspected surfaces, not an ecosystem-wide assertion.
The current hybrid REST path materializes JSON values [E2]; implementation must
retain typed hybrid hits until the final JSON or protobuf adapter. Preserve
existing gRPC conversion and separate rerank defaults [E5], not silently unify them.

## 3. HTTP negotiation and failure contract

Request body and its `Content-Type: application/json` remain unchanged.
Successful binary response: status 200,
`Content-Type: application/vnd.vlite.search+protobuf;version=1`,
`Vary: Accept`, `Content-Length: N`, and no `Content-Encoding`.
JSON success remains `application/json`, also with `Vary: Accept`.
Merge Vary tokens rather than replacing other middleware tokens.
Keep `X-Request-ID` and `Cache-Control: no-store` [E9].
Do not use the gRPC five-byte message prefix or a `grpc-*` header.
`Accept-Encoding` cannot enable compression of this representation in v1.

### Selection algorithm

After existing middleware auth/admission, validate the JSON request and enforce
the same collection binding as JSON before representation selection; negotiate
before executing search or lazy index maintenance. Preserve existing failure
precedence for invalid body/shape and forbidden collection. Share that preflight
with execution, not a second query or an alternate authorization implementation.

Combine repeated Accept fields as a comma-separated list. Maximum combined value:
8,192 bytes and 64 ranges; exceeding either gives the 431 below.
Parse HTTP media ranges, quoted parameter values and qvalues (0..1, at most
three decimal places); reject malformed syntax, duplicate q or duplicate media
parameters with 400. Type/subtype and parameter names are case-insensitive.
Omitted q means 1. An empty Accept list admits no representation; ignore empty
comma-list elements, but reject malformed nonempty elements.
No sniffing, filename, query argument or request-body format selector.

- JSON candidate: `application/json`, `application/*` or `*/*`, with no media
  parameters. Missing Accept is equivalent to `*/*`.
- Binary candidate: only an explicit `application/vnd.vlite.search+protobuf`
  range, with no media parameter other than optional `version`.
  Absent version means 1; `version=1` and `version="1"` mean 1. All other
  version strings are unsupported, including `01`. Version is not the API,
  database format or package version. q is a weight, not a media parameter.
- For JSON choose its most specific matching range (exact, application wildcard,
  global wildcard); for binary choose explicit version=1 over versionless.
  At equal specificity use the lowest q among duplicates, including zero.
  A more-specific q=0 vetoes a wildcard. Binary never inherits a wildcard q.
  Ignore unknown media parameters as nonmatching ranges; parameters after q
  are Accept extensions and do not select a schema.
- Pick the candidate with highest effective q greater than zero. Ties choose
  JSON. If no candidate remains and a positive explicit binary range requests
  an unsupported version, return `unsupported_result_version`; otherwise
  return `not_acceptable`. An acceptable JSON range can win instead of failing
  an unsupported binary range. Never silently substitute JSON when it is excluded.

| Accept value | Outcome |
|---|---|
| absent; `*/*`; `application/*` | JSON |
| `application/vnd.vlite.search+protobuf` | Binary v1 |
| `application/json, application/vnd.vlite.search+protobuf;version=1` | JSON tie |
| `application/json;q=0.4, application/vnd.vlite.search+protobuf;version=1;q=0.9` | Binary v1 |
| `application/json;q=0, */*;q=1` | 406 `not_acceptable` |
| `application/vnd.vlite.search+protobuf;version=2` | 406 `unsupported_result_version` |
| `application/vnd.vlite.search+protobuf;version=2, application/json;q=0.2` | JSON |
| `application/vnd.apache.arrow.stream`; `*/*;q=0` | 406 `not_acceptable` |

All errors, even when Accept excludes JSON, use
`Content-Type: application/json`, `Vary: Accept` and the existing [E11] envelope:
`{"error":{"code":"CODE","message":"MESSAGE"}}`. Exact new pairs:

| HTTP | CODE | MESSAGE |
|---|---|---|
| 400 | `invalid_accept` | `invalid Accept header` |
| 431 | `accept_too_large` | `Accept exceeds 8192 bytes or 64 ranges` |
| 406 | `unsupported_result_version` | `supported binary result version is 1` |
| 406 | `not_acceptable` | `supported results are application/json and application/vnd.vlite.search+protobuf;version=1` |
| 422 | `binary_result_limit` | `binary result exceeds 10000 hits, 1048576 bytes or a 1024-byte plan string` |
| 500 | `binary_result_invalid` | `search result cannot be encoded as binary version 1` |

These are representation errors, not engine Error variants. Do not expose
offending values, field names, encoded lengths or internal exceptions in messages.
Existing auth, request-body, search, busy and index errors retain their codes.
JSON requests do not acquire a new output cap. Binary requests never truncate
or fall back to JSON when a result cannot be encoded.

## 4. Exact message and byte layout

Proposed future file `proto/vlite/serving/v1/results.proto` contains this entire
schema; it is **not** added by this documentation slice. No services/imports:

```proto
syntax = "proto3";
package vlite.serving.v1;

enum ResultKind {
  RESULT_KIND_UNSPECIFIED = 0;
  RESULT_KIND_VECTOR = 1;
  RESULT_KIND_HYBRID = 2;
  reserved 3 to 15;
}
message VectorHit {
  uint64 id = 1;
  double distance = 2;
  reserved 3 to 15;
}
message HybridHit {
  uint64 id = 1;
  double score = 2;
  optional double distance = 3;
  optional double bm25 = 4;
  optional uint32 vector_rank = 5;
  optional uint32 text_rank = 6;
  reserved 7 to 15;
}
message SearchStats {
  uint64 records_scanned = 1;
  uint64 distance_evaluations = 2;
  uint64 quantized_distance_evaluations = 3;
  uint64 graph_nodes_visited = 4;
  uint64 buffered_records_merged = 5;
  uint64 reranked_candidates = 6;
  uint64 index_candidates = 7;
  reserved 8 to 15;
}
message QueryPlan {
  string strategy = 1;
  optional uint64 estimated_candidates = 2;
  uint64 live_records = 3;
  optional string index_field = 4;
  string reason = 5;
  reserved 6 to 15;
}
message SearchResults {
  uint32 version = 1;
  ResultKind kind = 2;
  repeated VectorHit hits = 3;
  repeated HybridHit hybrid_hits = 4;
  SearchStats stats = 5;
  QueryPlan plan = 6;
  reserved 7 to 15;
}
```

The HTTP body starts at byte 0 with SearchResults, ends at exactly Content-Length,
and has no outer header, magic, checksum, alignment padding, trailer or flags word.
Version is required semantically and equals 1; kind is required and is 1 or 2.
Reserved tags are unavailable, including for payload/metadata/vectors; they are
not optional empty columns. New features need a separately reviewed version.
No tag or enum value may be reused with a different meaning.

| Field encoding | Exact byte rule |
|---|---|
| Tag | Minimal unsigned varint of `(field_number << 3) | wire_type` |
| uint32/uint64/enum | Wire type 0; base-128 little-endian groups, high bit continuation; at most 5/10 bytes; final byte must fit the declared width |
| double | Wire type 1; eight bytes IEEE-754 binary64, least-significant byte first; not native-endian casts |
| string/message | Wire type 2; minimal varint byte length followed by exactly that many bytes; strings strict UTF-8 |
| Fields | Increasing tag number within each message; repeated hits contiguous and in engine rank order; no packed fields |
| Default scalars | Omit zero uint/enum and positive-zero nonoptional doubles; decode omission as zero; emit negative-zero double to preserve its bits |
| Presence | Optional scalar/string emits its tag even for zero/empty; absent decodes to null, never zero/empty |

No vectors, metadata, payload or collection name occur in the frame. IDs are
unsigned 64-bit, including zero and `18446744073709551615`. Distances are
lower-is-better values, scores retain their existing meaning [E2]; do not negate,
rescale or recompute either. All doubles must be finite; a nonfinite engine
result gives 500 `binary_result_invalid`, not null, saturation or dropped hits.
Preserve finite double bits, including signed zero.
Hybrid ranks, when present, are positive uint32; presence of distance equals
presence of vector_rank, and presence of bm25 equals presence of text_rank.
Preserve absence as JSON null in the normalized response.

Kind 1 allows only hits; kind 2 only hybrid_hits. Empty lists are legal, including
k=0; kind reflects the requested result family even with no matches.
IDs must be unique within the response. Maximum 10,000 total hits, also at most
requested k. Serializer preserves the supplied order, including MMR order;
it must not impose a new distance sort.
If explain=false, omit stats and plan. If explain=true, stats must be present
(an empty message represents all zero counters); absent plan means JSON
`"plan":null`. Plan may appear only with stats and only for auto with explain.
Strategy is exactly `exact_scan`, `exact_indexed`, `filtered_ann` or `ann` [E3].
Copy reason unchanged; optional estimate/index_field preserve null versus zero/empty.
Reason must be nonempty. Decoders also validate kind, hit count, explain presence
and auto-plan presence against the original request: auto+explain requires plan;
other requests forbid it. Those context mismatches are local decode errors.

### Bounds, canonical decoding and interrupted transport

Maximum body: **1,048,576 bytes**, inclusive, including all tags and lengths.
Each plan string: at most 1,024 UTF-8 bytes. Exceeding any output bound gives
422 `binary_result_limit` (the same fixed message); invalid presence, kind,
rank, UTF-8 or numeric state gives 500 `binary_result_invalid`.
Determine exact length with checked arithmetic before allocation. Allocate at
most the body cap for the encoded frame; do not encode an unbounded buffer then
measure it. This cap is not a bound on query scratch, RSS or transport buffers.

Decoders check body and nested lengths against remaining bytes before reading
or allocating; enforce two message levels (root plus hit/stats/plan).
Reject overlong/nonminimal varints, overflow, unknown/reserved tags, groups,
wrong wire types, duplicate singular fields, out-of-order fields, noncanonical
explicit default scalars, invalid UTF-8, unknown kind/version, mixed hit lists
and bytes not consumed as part of this one message. A concatenated second
envelope is invalid (duplicate version/kind), never a second result set.
These are a strict v1 profile over protobuf: a generic generated decoder alone
is insufficient validation. Do not adopt its merge/last-field-wins behavior.
Unknown fields require a new negotiated version, not speculative skipping.

Prepare/validate the full frame and release database borrows before 200.
Retain existing admission, check cancellation before sizing, at least every
64 hits during sizing/encoding, and immediately before headers. Disconnect
discards the frame; no background retry. Do not introduce a new search deadline.
After headers, a socket failure terminates the body/connection; do not append
JSON or send a success-shaped prefix. Clients require exactly N bytes, valid
framing and expected representation before exposing any hit. HTTP truncation,
length mismatch or malformed frame is local `binary_result_decode`; a body over
the cap is local `binary_result_limit`; unrecognized response media/version is
local `unsupported_result_version`. These local codes carry no HTTP status
invented by the client and retain the observed request ID if available.

### Worked bytes and golden fixtures

Spaces below separate bytes, not padding. These are canonical whole bodies.

**A. Empty vector result, explain=false (4 bytes):**
`08 01 10 01`.
Offsets 0..1: field 1/version=1. Offsets 2..3: field 2/kind=VECTOR.
Normalized JSON: `{"hits":[]}`.

**B. One vector hit, ID 150, distance 1.5, explain=false (18 bytes):**

```text
08 01 10 01 1a 0c 08 96 01 11 00 00 00 00 00 00 f8 3f
```

Offsets 0..3: version/kind. Offset 4: field 3/type 2. Offset 5: nested length 12.
Offsets 6..8: ID tag 08, varint 96 01 = 150.
Offset 9: distance tag 11; offsets 10..17: little-endian 1.5.
Normalized JSON: `{"hits":[{"id":"150","distance":1.5}]}`.

**C. Empty hybrid result, explain=true with no plan (6 bytes):**
`08 01 10 02 2a 00`.
Offsets 2..3 select HYBRID; offsets 4..5 are present empty stats.
Normalized JSON has empty hits, all seven counters equal to 0 and plan=null;
it is not equivalent to A or to omitting stats.

In the oracle slice, commit hand-authored `.hex` plus expected normalized `.json`
pairs under `tests/fixtures/serving_binary/`, including A/B/C, maximum u64,
signed zero, all hybrid optionals, absent versus present zero, UTF-8 index_field,
all counters and each plan strategy. Expected numeric floats also record their
64-bit hexadecimal representation so JSON parsing cannot hide bit drift.
Fixtures originate from this layout, not a server/generator round-trip.

## 5. gRPC and exact future OpenAPI diff

**No change to `proto/vlite/v1/vlite.proto` or the gRPC service is needed for
this slice.** gRPC already encodes typed hit messages [E5]; HTTP Accept has
no meaning on its RPC. SearchResults is a separate HTTP representation, not a
replacement for `vlite.v1.SearchResponse`. The new schema has no service.
Keep existing tags (`Hit` 1/2, `HybridHit` 1..6, `SearchResponse` 1/2) untouched.
Its absence of HTTP explain/version fields is intentional. Auto/explain parity
belongs to a separately reviewed serving schema, not this candidate.
The HTTP typed refactor may adjust `src/server/grpc.rs` adapters only to preserve
the existing wire and result behavior; run old/new client compatibility checks.

Only the later HTTP slice may edit `src/openapi.json`. Exact proposed delta:

- At `/v1/search` post and `/v1/collections/{c}/search` post [E6], add optional
  header parameter `Accept`, string, default `application/json`, with the
  algorithm/examples from section 3. Keep path spelling `{c}` in OpenAPI.
- Both 200 responses retain JSON; named 200 gains the same JSON response
  schema as legacy. Add content key
  `application/vnd.vlite.search+protobuf;version=1` with
  `{"schema":{"type":"string","format":"binary"}}`; description names
  `vlite.serving.v1.SearchResults`, this contract and the future schema file.
  Do not model binary IDs as JSON numbers or apply new required fields to JSON.
- Both 200 responses document `Vary` (string, example `Accept`) and binary
  `Content-Length` (integer, minimum 4, maximum 1048576); length is required
  for binary only. Document binary no-content-encoding.
- Add 406, 431 and 500 Error responses and extend 400/422 descriptions with
  the exact codes/messages in section 3; retain every existing error mapping.
  Document Vary and JSON error content, including when Accept excludes JSON.
  No new request properties, paths, security schemes or gRPC field allocations.

## 6. Authorization, observation and security

Reuse middleware and `search_core`, not a binary-specific engine entry point [E1, E4, E9].
Search scope, read-only restrictions on lazy approximate builds and rerank
validation stay identical (`src/server.rs:944-1020,2645-2690`).
Use existing expiry/snapshot behavior, never a post-search fetch for projection.
Collection names remain local names at the API boundary, qualified once using
the principal, then checked by name before resolution and lineage/ID after [E10].
`collections:["*"]` is credential reach, not a search request or fan-out:
it grants only collections owned by that tenant and bound database lineage [E10].
It cannot escape a tenant, turn the legacy default alias into all-collections
search, or authorize an unrelated graph/record.

Keep the same matched route labels, request/error counts, latency accounting,
slow-query data and denial-audit behavior [E4, E9]. Count search execution once;
encoding failure still counts the performed search and a failed HTTP request.
Negotiation rejection performs no search and therefore adds no search counters.
Use ErrorCode for new failures [E11], never bypass audit on the binary path.
No new metrics endpoint or high-cardinality labels. If recording encoding
outcome, use only fixed format/version/outcome values in the existing sanitized
request log, never Accept text, IDs, strings, vectors, metadata or payloads.
HTTP request duration includes preparation but not successful receipt by the
client; a post-header disconnect cannot be retroactively reported as HTTP 500.

Checked lengths, depth and counts prevent length confusion and allocation
amplification. No compression, dictionaries or nested opaque bytes are allowed:
reject unexpected Content-Encoding in binary clients rather than feeding it
to an unbounded decompressor. HTTP intermediaries must not transform this
representation; clients still enforce limits if they do.
Errors do not echo data or disclose collection existence before auth. No cached
cross-tenant frames; no-store and the current transport security remain in force.
No checksum is an authenticity mechanism; retain normal TLS deployment guidance.

## 7. Client and tool impact

Proposed HTTP method options, not changes to request JSON:
JavaScript `search(request, {resultFormat = "json"} = {})` and
`searchDetailed(request, {resultFormat = "json"} = {})`;
Python `search(request, *, result_format="json")` and
`search_detailed(request, *, result_format="json")`;
.NET retain current overloads and add
`SearchDetailedAsync(object request, SearchResultFormat resultFormat, CancellationToken cancellation = default)`,
with enum Json/BinaryV1. Leave .NET `SearchAsync` JSON-only: its existing
`Hit.Distance` is nonnullable (`clients/dotnet/VLite.Client/Client.cs:27,159-162`),
so it cannot preserve absent hybrid distances. A nullable typed-hit redesign
is a separate compatibility decision, not a silent change in this slice.
Scope these options to the two search routes, not the entire transport.
Reject unknown option values locally. BinaryV1 sends only
`Accept: application/vnd.vlite.search+protobuf;version=1`, not a JSON fallback.
No automatic probe, downgrade, retry or change to auth/timeout/redirect policy.
Check status first: non-2xx is the existing JSON error path; 2xx requires the
requested content type/version, length and strict frame validation.

Normalize to the existing public response shape, not generated protobuf objects:
decimal-string IDs, hits array for search, hits/stats/nullable plan for detailed.
JavaScript uses BigInt internally for u64; never pass IDs through Number.
Use its existing exact-integer representation for explain counters too
(`clients/javascript/index.js:105-113`).
.NET uses ulong for decoding, then decimal-string IDs inside JsonElement;
Python uses int plus decimal strings for IDs. Zero-valued optional leg data
must remain present, and missing leg data becomes null. Preserve method-specific
return conventions and client exception families; expose the local decode codes
from section 4 without wrapping them as fake server Error responses.

Node and Python embedded bindings do not use HTTP [E8]: no binary option,
FFI type change or behavior change. MCP retains JSON tool results and CLI
retains its current output [E8]; neither negotiates this HTTP representation.
Other HTTP clients remain JSON until separately assigned opt-in decoder work.

## 8. Independent oracle and acceptance tests

The future standard-library-only Python oracle
`tests/serving_binary_oracle.py` independently reads tags, checked varints,
little-endian doubles and bounded nested messages using `struct`, not generated
protobuf bindings, Rust helpers or a production client decoder.
Self-test must decode A/B/C to hand-authored expectations; prove that changing
an expected ID, double bits, nullability, kind or counter makes comparison fail;
and reject deliberately malformed fixtures rather than accepting an empty answer.
Run self-tests as part of the oracle invocation.

| Family | Independent acceptance assertion |
|---|---|
| Goldens | Python accepts canonical manual fixtures; Rust produces identical bytes; supported SDKs normalize identically |
| JSON parity | On a fixed clock/snapshot, same authorized execution supplies both encoders; compare ordered IDs, finite float bits, absent/null fields, counters and plan |
| Modes | Exact, approximate, approximate_ready, auto, hybrid vector/text/both, exact/MMR rerank; preserve current validation and index errors |
| Boundaries | k=0, 1, 10000 and 10001; empty matches; max u64; signed zero; exact byte cap and cap+1 via synthetic encoder values; per-string cap and cap+1 |
| Negotiation | Every section 3 row; q=0 exclusions, duplicate ranges, repeated fields, quoted version, unsupported versions, malformed q, header/range ceilings |
| Auth | Admin/read-only/scoped, missing/expired/revoked credential, wrong scope/lineage, recreated collection, other tenant, wildcard reach and legacy default |
| Observation | JSON/binary success and failure increment matching counters/audits once; negotiation rejection has no search work; no sensitive log content |
| Transport | Pre-header encode failure remains JSON; disconnect/deadline/cancel; short/long Content-Length, HTTP chunking inconsistent with contract, unexpected encoding/type/version; no partial output |
| Compatibility | Old JSON clients/new server unchanged; binary client/old JSON-only server fails explicitly; old gRPC generated clients/new server unchanged |

Mutation corpus: each byte truncation; flipped tag/type/version/kind; duplicate
singular and reordered fields; unknown/reserved tags; zero field number; groups;
overlong/overflowing varints; nested length underflow/overflow and huge declarations;
duplicate IDs; too many hits; mixed families; invalid UTF-8; NaN/infinity;
present rank zero; broken leg presence; plan without stats; unsupported strategy;
explicit default scalar; concatenation, trailing junk and compressed bodies.
Fuzz arbitrary bytes and structured mutations under allocation/time caps;
assert deterministic rejection or full validated consumption, no panic/OOM,
and no acceptance that disagrees with the Python oracle. The corpus must include
all limit crossings even when real search output cannot naturally reach them.
This tests representation parity, not ANN recall or a ranking-quality guarantee.

## 9. Delivery ledger and ownership

Use the existing P5 delivery item (5), **approved binary serving**; the subledger
names below are proposed labels under it, not claims of existing accepted work.
They do not consume Gap 8/12 or P5 ANN/compression ledger entries.
Order is dependency order; each candidate needs independent acceptance before
integration. File ownership below is future work only.

| Ledger name | Depends on | Files owned | Independent-acceptance checklist |
|---|---|---|---|
| P5-5A binary contract | None | This document only | Approve schema, negotiation, errors, bounds and owner decisions; confirm OpenAPI/proto unchanged |
| P5-5B binary oracle | 5A | `tests/serving_binary_oracle.py`, `tests/fixtures/serving_binary/` | Manual bytes/answers, positive and negative self-tests, mutations and bounded decode independently checked |
| P5-5C typed encoder | 5B | `proto/vlite/serving/v1/results.proto`, new `src/server/binary_results.rs`, `src/server.rs` typed Hits/refactor, `src/server/grpc.rs` adapter, required schema build wiring | Exact proposed schema; typed JSON parity; cap-before-allocation; oracle byte equality; unchanged gRPC wire/rerank behavior |
| P5-5D HTTP opt-in | 5C | `src/server.rs` negotiation/adapter, `src/openapi.json`, new `tests/server_binary_results.rs` | Section 5 diff only; header/error table, auth/tenant/audit parity, no search on rejection, full-frame-before-headers and cancellation verified |
| P5-5E HTTP clients | 5D | `clients/javascript/`, `clients/python/`, `clients/dotnet/` implementation/types/tests and their search docs | All three decoders cross-check oracle; exact IDs/presence; defaults and existing signatures preserved; explicit failure on old server |
| P5-5F serving qualification | 5E | Serving parity/fuzz tests, directly related public API/security/client docs and `site/api.html` | Full matrix, compatibility, sanitized observation, published contract agrees with schema/OpenAPI; no default/gate change |

Do not overlap ownership of server.rs between 5C/5D or integrate a partial
client set as completion of 5E. Coordinate schema build wiring with its owner;
do not add an RPC or pull unrelated graph/provider APIs into the same candidate.
Major-release measurement remains outside these slices and follows the existing
roadmap; accepting a codec is not acceptance of F7 qualification.

## 10. Owner decisions and open items

These precise choices refine the approved direction; acceptance must record
each decision rather than imply approval from this document.

| Decision | Options | Recommendation |
|---|---|---|
| Encoding | HTTP protobuf or Arrow | HTTP protobuf v1 as specified; defer Arrow/projection to a separately versioned design |
| Schema identity | Extend existing gRPC SearchResponse or independent HTTP envelope | Independent `vlite.serving.v1.SearchResults`; no gRPC tag/default changes |
| Negotiation | Strict binary opt-in or automatic binary preference | Strict opt-in with JSON ties/default and explicit 406 codes |
| Limits/failures | Full bounded unary error or partial/streamed output | 10000 hits, 1048576 total bytes, 1024 bytes per plan string; explicit 422/500; no prefix/fallback |
| Decoder evolution | Permissive unknown-field merging or closed version profile | Closed canonical v1, new version for additions; keep generated decoders behind strict validation |
| Client reach | Opt in all transports or three HTTP SDKs only | JavaScript/.NET/Python HTTP options above; embedded/MCP/CLI stay unchanged |
| .NET typed hybrid output | Change nonnullable Hit or opt in detailed responses only | Detailed JsonElement binary support only; keep SearchAsync JSON-only until a separate typed compatibility decision |

Open implementation items: independently verify the canonical protobuf negative-zero
behavior of the selected Rust generator; if it omits negative zero, encode that
field explicitly rather than weakening bit parity. Verify generated optional
presence and build isolation before 5C; use a checked typed writer if necessary.
Choose the bounded fuzz runner using existing repository tooling in 5F.
Any collision with another accepted serving schema or inability to retain
JSON/gRPC behavior is a blocker for review, not permission to widen scope.
