# Serving precision contract: P3 Design A

**Status: proposed exact schemas, awaiting owner review; no implementation or
activation approval.** The owner approved Design A in principle on 2026-11-02,
subject to exact-schema review before code
(`docs/projects/roadmap/next-major.md:40-55`). This document applies nothing.
Float32 remains the default. Float16 remains a separate, explicitly selected,
lossy per-collection option, unavailable to production writers until qualified.
This document follows the [serving wire conventions](serving-wire-conventions.md), whose shared owner decisions were selected on 2026-11-02; items still unchecked for it in that note's section 7 remain conformance work.

Scope: collection creation and inspection through REST, gRPC, MCP and clients;
offline compact-store creation and copy-out conversion through CLI. Non-goals:
online conversion, a remote filesystem administration API, mutable precision,
new record/search representations, new storage encodings, changed defaults,
qualification claims or measurements.

Authority: [Rust Design A](p3-precision-api.md), [P3](p3-compact-vectors.md),
[compact contract](compact-storage-contract.md) section 8 decisions 7/8 and
section 9, and [publication design](p3-precision-publication.md).
The [serving contract](serving-contract.md) is the transport-format model.
Rust names below are **designed**, not claims that public APIs exist here.
Citations below were checked against this checkout.

## 1. Current coverage and boundaries

| Surface | Current coverage and verified evidence |
|---|---|
| REST collection create | `POST /v1/collections`; strict name/dimensions/settings request, 201 `CollectionInfo` JSON; no precision input (`src/server/collections.rs:12-15`, `:228-235`, `:267-315`). |
| REST list | `GET /v1/collections`; `{items,format_version}` with grant filtering and tenant-local names (`src/server/collections.rs:192-225`). |
| REST info/settings | `GET /v1/collections/{collection}` adds `text_index`; PUT settings merges existing mutable settings (`src/server/collections.rs:86-90`, `:318-349`, `:380-409`). |
| Rust collection descriptor | `CollectionInfo` has id/name/dimensions/live_records/settings/settings_persisted, no precision or compact field; settings also have no precision (`src/collection.rs:27-50`, `:106-119`). Old named create explicitly chooses private Float32 (`src/lib.rs:2585-2591`). |
| gRPC | List/Get/CreateCollection already exist; Collection tags 1-6, list tags 1-2, create tags 1-3; no precision. Adapters share REST cores (`proto/vlite/v1/vlite.proto:16-20`, `:62-87`; `src/server/grpc.rs:806-870`). |
| CLI create | Actual entry points are shell `--db PATH --create DIMENSIONS` and import `--create` into default, using ordinary create (`src/bin/vlite-shell.rs:11-19`, `:54-72`; `src/bin/vlite-import.rs:376-384`, `:437-441`). There is no `vlite` dispatcher binary: Cargo names server/shell/import/MCP binaries (`Cargo.toml:90-125`). |
| CLI inspect/convert | `vlite-maintain` has inspect and copy-out compact/upgrade commands, not precision conversion (`src/bin/vlite-maintain.rs:5-15`, `:29-55`). V5-to-directory conversion exists in Rust (`src/maintenance.rs:219-253`). Private test-only `convert_precision` is not Design A's public converter: it rejects empty selections and uses different errors (`src/maintenance.rs:257-317`). |
| MCP | Local info/list expose counts, not precision; no collection-create/info-by-name tool in the core tool dispatcher. Proxy mode currently forwards graph tools only (`src/bin/vlite-mcp.rs:24-26`, `:294-320`, `:464-477`, `:490-510`). |
| HTTP clients | Python and JavaScript have list/create/get/settings methods; .NET has list/create returning JsonElement, without a collection-info method in that surface (`clients/python/vlite_client/__init__.py:792-813`; `clients/javascript/index.js:314-325`; `clients/dotnet/VLite.Client/Client.cs:119-133`). |
| Format gates | Flag 16 is compact, flag 32 precision policy. Production `precision_supported()` is false; header parsing excludes flag 32 and returns `UnsupportedFeature(flags & !known_flags)` before encrypted descriptor opening (`src/format.rs:38-41`, `:64-68`, `:1440-1490`; `src/compact/mod.rs:45-65`). Dense source codec is test-only (`src/compact/mod.rs:45-49`). |
| Server opening now | Open errors other than the narrowly guarded NotFound create case propagate (`src/bin/vlite-server.rs:515-544`, `:650-665`). A flag-32 precision image is refused as `UnsupportedFeature(32)`, display **`database header declares unsupported feature flags 0x0020`** (`src/lib.rs:634-637`). Main logs `startup_or_shutdown_failure` and returns failure (`src/bin/vlite-server.rs:823-830`), so no HTTP error response exists. |

Authentic derived-only flag-16 images open; unsupported critical source kinds
refuse (`src/compact/mod.rs:1-15`, `:1406-1428`). Neither a compact envelope nor
a successful legacy open authorizes production float16.

## 2. Numbered, testable requirements

1. **R1, gates:** this design never switches on storage support. Until the Rust
   API is integrated and the relevant backing/read/write/maintenance paths are
   qualified, explicit compact/precision requests fail before publication.
2. **R2, no inheritance:** omitted collection precision always selects Float32,
   including in a compact store whose default collection is Float16. Explicit
   precision requires an already opted-in store, even explicit `"float32"`.
3. **R3, immutable:** precision is fixed even for empty collections. Settings
   replacement, index configuration, writes and reopen never change it.
4. **R4, authoritative reads:** precision and compact status come from designed
   `collection_precision(physical_name)` and `is_compact()` under the same read
   guard as collection info; never infer either from rows, defaults or flags
   exposed by the transport. Leave Rust `CollectionInfo` unchanged.
5. **R5, canonical writes:** submitted dense vectors remain float32 inputs.
   Float16 stores canonical round-to-nearest-even binary16 values widened to
   float32 before commit; overflow is `NonFiniteVector`, never stored infinity.
   Queries are not rounded. Immediate/reopened values and exact scores agree.
6. **R6, offline copy-out:** conversion holds the exclusive read-only source
   lock, leaves source bytes untouched, requires absent output, and never runs
   in place, through a serving handle, or concurrently with serving that source.
7. **R7, parity:** authorization, name qualification, limits, atomicity and stable
   error codes agree across transports. No float32 retry on unsupported precision.
8. **R8, compatibility:** legacy constructors and omitted-precision requests
   retain their behavior. Responses add optional fields only. An absent field
   from an older server means unknown capability, not proof of Float32.
9. **R9, review:** OpenAPI/proto stay frozen until p3-serving-rest and
   p3-serving-grpc respectively start, after the Rust precision API is
   integrated; float16 activation is a later, separately qualified opt-in decision.

R2-R6 restate [Design A](p3-precision-api.md) ("Exact reviewed signatures",
"Behavior and compatibility matrix", "Explicit conversion contract"); the
private candidate is not an integrated public API.

## 3. Exact REST operations and JSON

Only precision strings `"float32"` and `"float16"` are accepted, case-sensitive.
No aliases, whitespace normalization, numeric encodings, null or coercion.
Request objects reject unknown properties; precision is a top-level field,
never a setting. Read it from the raw JSON value so absent differs from `null`:
`Option<_>` fields fold `null` into absent (silently Float32) and a typed
`String` turns a wrong type into `invalid_json`. Absent = legacy create;
`null` or an unknown/case-mismatched string = proposed 422 `invalid_input`
with `details.field="precision"`; a non-null, non-string value is a shape
failure, 422 `invalid_json`. A field type that deserializes `Value`, defaulting
to Absent, preserves these distinctions. See [wire conventions, decisions 6
and 21](serving-wire-conventions.md#6-owner-decisions-23). Existing settings
semantics/limits remain: REST updates partially merge, with the strict input and
validation in `src/server/collections.rs:94-170` and `src/collection.rs:59-94`.

| Operation | Exact contract |
|---|---|
| Create named collection | `POST /v1/collections`, required `name:string`, `dimensions:integer`; optional `settings:object` (existing CollectionSettings input, omitted = defaults), `precision:string` (omitted = legacy Float32 create). Explicit precision calls designed `create_collection_with_precision`. Success 201, body CollectionInfo extended below. |
| List precision/store status | `GET /v1/collections`, no body; 200 `{items:CollectionInfo[],format_version:integer,compact:boolean}`. Filter before enrichment; do not reveal another collection's policy. |
| Read precision/store status | `GET /v1/collections/{collection}`, no body; 200 CollectionDetail, preserving text_index and adding precision/compact. |
| Settings update | Existing `PUT /v1/collections/{collection}/settings`; no precision input. Its `{collection,persisted}` response (200 has no schema today, `src/openapi.json:55`; section 9) enriches the nested collection identically. A supplied settings precision fails 422 `invalid_json`, including a same-value request. |
| Create/convert a store | **No REST method/path.** No new admin route, RPC, MCP tool or HTTP client method accepts source/destination paths. Operator stops the service and uses offline CLI. No implicit conversion during open. |

Name pattern `^[a-z][a-z0-9_-]{0,62}$`, dimensions 1..65536, at most 64 live
collections including default, IDs 0..65535; default is reserved for named
create. These are existing engine bounds (`src/lib.rs:2579-2584`,
`:2610-2631`; `src/lib.rs:93`). Tenant-qualified physical names must fit 63
bytes too, so tenant-local names are at most 28 ASCII bytes: the prefix adds
35 bytes (`src/server/credentials.rs:141-149`, `:327-328`).
Transport body limits are unchanged; gRPC remains 8 MiB request / 16 MiB
response (`proto/vlite/v1/vlite.proto:11`).

Example request and complete response for a new empty
collection in an opted-in store:

```json
{"name":"docs","dimensions":3,"settings":{},"precision":"float16"}
```

```json
{
  "id":1,"name":"docs","dimensions":3,"live_records":0,
  "settings":{
    "metric":"squared_l2","hnsw":{"m":16,"ef_construction":100,"seed":"42"},
    "quantization":"none","rerank_multiplier":4,
    "planner":{"exact_max_candidates":4096,"exact_max_selectivity_ppm":20000},
    "metadata_indexes":[],"text_fields":[]
  },
  "settings_persisted":true,"precision":"float16","compact":true
}
```

The existing settings serializer uses string seeds
(`src/server/collections.rs:69-82`); defaults are verified at
`src/collection.rs:42-50`, `src/hnsw.rs:117-124` and
`src/planner.rs:27-32`. CollectionDetail adds this existing shape:

```json
{"text_index":{"declared_fields":0,"available":false,"documents":0,"terms":0,"postings":0}}
```

Text-index values come from the engine, not precision
(`src/server/collections.rs:326-328`; `src/text_index.rs:40-50`).
List embeds the complete create/info object shape (without text_index) in
`items`, not a new precision-only descriptor. `precision` and `compact` are
optional in response schemas for old-server compatibility; new implementations
MUST emit both on every create/list/info/nested-settings descriptor, including
legacy files (`"float32",false`). Top-level list compact is also always emitted,
even for an empty authorized result. Compact is persisted store intent, not
whether any block exists.

Success follows the engine's durability contract. There is no precision PATCH,
hit/record precision field or per-upsert precision parameter.

## 4. Additive proto contract

Existing CreateCollection remains unchanged and always uses legacy Float32
creation. Add a distinct RPC for explicit precision: old servers return
UNIMPLEMENTED rather than silently ignoring an unknown request field and
creating Float32. Do not retry it through CreateCollection. REST's strict old
request parser already rejects an unfamiliar precision field
(`src/server/collections.rs:228-235`).

The following is the exact additive diff inventory; existing fields/messages
remain byte-for-byte unchanged, except for the shown added fields/reservations.
No tags in these messages are currently reserved
(`proto/vlite/v1/vlite.proto:62-87`); these new reservations allocate no fields.
At serving-order step 11, the descriptor test must confirm the proposed
`Collection.precision=7`, `Collection.compact=8` and
`ListCollectionsResponse.compact=3`, and reservations `3 to 15` on
VectorPrecision, `5 to 15` and `100 to 199` on
CreateCollectionWithPrecisionRequest, `9 to 15` on Collection and `4 to 15` on
ListCollectionsResponse: all are absent from, and do not collide with, the
frozen proto's current declarations; the low-tag additions and new-message
reserve follow [wire conventions sections 1.3-1.4](serving-wire-conventions.md#13-field-number-allocation-c1),
without sibling-band fences on baseline-owned messages.

```proto
// Add inside service VLite:
rpc CreateCollectionWithPrecision(CreateCollectionWithPrecisionRequest)
    returns (Collection);

// New enum and message:
enum VectorPrecision {
  VECTOR_PRECISION_UNSPECIFIED = 0;
  VECTOR_PRECISION_FLOAT32 = 1;
  VECTOR_PRECISION_FLOAT16 = 2;
  reserved 3 to 15;
}
message CreateCollectionWithPrecisionRequest {
  string name = 1;
  uint32 dimensions = 2;
  optional string settings_json = 3;
  VectorPrecision precision = 4;
  reserved 5 to 15;
  reserved 100 to 199;
}

// Add inside existing Collection (existing tags 1..6 retained):
optional VectorPrecision precision = 7;
optional bool compact = 8;
reserved 9 to 15;

// Add inside existing ListCollectionsResponse (tags 1..2 retained):
optional bool compact = 3;
reserved 4 to 15;
```

Explicit RPC requires enum 1 or 2; 0 and unknown values fail INVALID_ARGUMENT
with proposed `invalid_input` and `details.field="precision"` per
[wire conventions, decisions 6 and 21](serving-wire-conventions.md#6-owner-decisions-23).
Omitted settings_json means defaults; supplied
settings_json must encode the same strict settings object as REST, not null.
Its errors differ from REST's: malformed JSON text is 400 `invalid_json`; a
non-object or strict-schema, unknown-field or type error (including `precision`)
is 422 `invalid_input`; both are INVALID_ARGUMENT
(`src/server/collections.rs:250-256`).
GetCollection and ListCollections use the enriched Collection messages.
Response precision must be present and nonzero on new servers; compact false
is present, not omitted as a protobuf default. No store-create/convert RPC.
There are no changes to Vector, Hit, Changes, export or stream-import messages.
Unknown response enum values must not be interpreted as Float32.

## 5. MCP contract

Add these tools to both local mode and the existing proxy mode; proxy extension
is proposed work, not a claim that core collection tools are proxied today
(`src/bin/vlite-mcp.rs:24-26`). In proxy mode use the same bearer and REST
authorization; never elevate to the proxy operator's authority.

```json
[
  {
    "name":"vlite_create_collection",
    "inputSchema":{
      "type":"object","additionalProperties":false,
      "required":["name","dimensions"],
      "properties":{
        "name":{"type":"string","pattern":"^[a-z][a-z0-9_-]{0,62}$"},
        "dimensions":{"type":"integer","minimum":1,"maximum":65536},
        "settings":{"$ref":"#/$defs/CollectionSettings"},
        "precision":{"type":"string","enum":["float32","float16"]}
      },
      "$defs":{
        "CollectionSettings":{
          "type":"object","additionalProperties":false,
          "properties":{
            "metric":{"enum":["squared_l2","cosine","inner_product"]},
            "hnsw":{"type":"object","additionalProperties":false,"properties":{
              "m":{"type":"integer","minimum":2,"maximum":64,"default":16},
              "ef_construction":{"type":"integer","minimum":2,"maximum":4096,"default":100},
              "seed":{"oneOf":[
                {"type":"integer","minimum":0,"maximum":18446744073709551615},
                {"type":"string","pattern":"^(0|[1-9][0-9]*)$"}],"default":42}}},
            "quantization":{"enum":["none","scalar_u8"]},
            "rerank_multiplier":{"type":"integer","minimum":1,"maximum":16},
            "planner":{"type":"object","additionalProperties":false,"properties":{
              "exact_max_candidates":{"type":"integer","minimum":1,"maximum":1000000},
              "exact_max_selectivity_ppm":{"type":"integer","minimum":0,"maximum":1000000}}},
            "metadata_indexes":{"type":"array","maxItems":16,"items":{
              "type":"object","additionalProperties":false,"required":["field","kind"],
              "properties":{
                "field":{"type":"string","minLength":1,"maxLength":256},
                "kind":{"enum":["eq","range","eq_range"]}}}},
            "text_fields":{"type":"array","maxItems":8,"uniqueItems":true,
              "items":{"type":"string","minLength":1,"maxLength":256}}
          }
        }
      }
    },
    "annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false}
  },
  {
    "name":"vlite_get_collection",
    "inputSchema":{
      "type":"object","additionalProperties":false,"required":["name"],
      "properties":{"name":{"type":"string","pattern":"^[a-z][a-z0-9_-]{0,62}$"}}
    },
    "annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true}
  }
]
```

The local definition inlines the existing schema constraints
(`src/openapi.json:155`, `:159-168`, `:178`). Engine checks additionally enforce
ef_construction >= m, u64 seed bounds, unique metadata fields and UTF-8 byte
limits that JSON Schema character counts do not replace
(`src/collection.rs:59-94`; `src/server/collections.rs:91-92`;
`src/server/admin.rs:623-635`).

Create/get structuredContent is exactly REST CollectionInfo/CollectionDetail.
Existing `vlite_info` and `vlite_list_collections` keep their empty-object
inputSchema (`src/bin/vlite-mcp.rs:464-465`). Add `compact:boolean` at the
top level of both outputs and `precision:string,compact:boolean` to each
existing collection item; keep `collections`, `count`, existing info fields
and graph extensions, rather than rename them to REST `items`/`live_records`
(`src/bin/vlite-mcp.rs:490-510`). Read-only mode advertises no create tool and rejects direct calls; local
writable mode has filesystem authority, not tenant credential semantics.
Do not add a convert/create-store tool, even in writable mode.

For these new tools, success is the existing content text plus structuredContent
with `isError:false`; failure is `isError:true` with text=message and
`structuredContent:{"error":"<code>: <message>"}`, as proposed in
[wire conventions, decision 22](serving-wire-conventions.md#6-owner-decisions-23).
Proxy preserves the REST code/message; local errors use section 8.
**Withdrawn:** the new-tool-only object-shaped failure proposal added a second
parser shape; see wire conventions, decision 22. Malformed JSON-RPC keeps protocol errors;
valid tools/call with invalid operation arguments uses `invalid_json` or
`invalid_input`, not a success-shaped empty result.

## 6. CLI: exact offline operations

New commands below belong to `vlite-maintain`, not a new `vlite` binary.
Existing commands/output are untouched, including `FAIL: {error}` on stderr
(`src/bin/vlite-maintain.rs:103-111`). New commands print one JSON object on
stdout on success; on refusal they print the section 8 JSON envelope on stderr
and no success object. All exit 0 on success and 1 on failure.
`--json` is required on new inspect/list forms to distinguish existing output.

```text
vlite-maintain create DEST --dimensions N --compact
  [--precision float32|float16] [--backing file|directory]
  [--encryption-key-file PATH]
vlite-maintain collection-create PATH --name NAME --dimensions N
  [--precision float32|float16] [--settings-json JSON]
  [--encryption-key-file PATH]
vlite-maintain collections PATH --json [--encryption-key-file PATH]
vlite-maintain inspect PATH --json [--encryption-key-file PATH]
vlite-maintain convert-compact SOURCE DEST
  [--precision NAME=float32|NAME=float16]... [--encryption-key-file PATH]
```

Create requires `--compact`; precision defaults to float32, backing to file,
dimensions to no default. Destination must be absent, file or directory as
selected. It invokes designed `create_compact`/`create_compact_directory`;
default collection precision is the selected value, not a future collection
default. Format is V5 only; no V3/V4 or commit-timestamp override. New commands
reject unknown and duplicate scalar flags; only `convert-compact` repeats
`--precision NAME=PRECISION`. `--encryption-key-file PATH`
(`src/bin/vlite-server.rs:278`;
`src/bin/vlite-mcp.rs:109` aliases `--key-file`) supplies the CreateOptions
EncryptionKey: exactly 32 bytes is a raw key, otherwise a passphrase with
trailing CR/LF trimmed (`src/bin/vlite-server.rs:425-434`). No literal key
argument; no key in output.

Parser: `vlite-maintain` is positional (`src/bin/vlite-maintain.rs:31-34`);
`tool_support::args` rejects duplicates and treats only `--help`, `--json`,
`--read-only` as valueless (`src/bin/tool_support/mod.rs:14-32`), so new
commands need a parser that also treats `--compact` as valueless and repeats
`--precision`. Packaging: no `[[bin]]` entry exists, `json` is optional
(`Cargo.toml:15,20,63`) and tests run the binary under default features
(`tests/maintenance.rs:191,479`), so `required-features = ["json"]` would break
them (section 11, "vlite-maintain packaging"). p3-serving-cli-mcp owns
`Cargo.toml`.

Collection-create accepts the same request as REST; omission calls legacy
create, explicit precision calls designed precise create. It acquires an
exclusive writable handle and cannot act on a store already served.
Success is the REST extended CollectionInfo. Collections returns the REST list
shape. Inspect returns
`{"format_version":N,"compact":BOOL,"collections":[...]}`, where entries are
extended CollectionInfo in ID order, from a complete, validated read-only open.
A V5 compact store reports `5,true`; a legacy file reports its actual version,
`compact:false` and Float32 descriptors.

Create output is `{"created":true,"format_version":5,"compact":true,
"backing":"file","collections":[...]}`; backing is `"directory"` when selected.
Conversion output is `{"converted":true,"format_version":5,"compact":true,
"collections":[...],"lineage":"DECIMAL_U64"}`. The CLI reopens the output
read-only for `collections` and `lineage` (`Database::lineage()`,
`src/lib.rs:1577`): `convert_to_compact` returns `Inspection`, which carries
neither lineage nor history floor (`src/maintenance.rs:24-34`), so no
history-floor field is output (section 11).
`...` stands for section 3 descriptor arrays.

Conversion invokes designed `maintenance::convert_to_compact` with the full
override list, at most 64 distinct physical names, `"default"` allowed. Parse
each `NAME=PRECISION` exactly; unknown/duplicate names fail before any output
publication. No overrides is valid format-only opt-in. Omitted collections
preserve their own source precision (legacy = Float32); selecting default
does not select any other collection. Source must be a complete closed V5 file
or directory; destination is an absent **file**. Directory output would be a
separate `convert_to_directory[_with_key]` step, not a Design A argument; no
binary calls it today and this contract adds no CLI route to it (section 11).
No in-memory or same-path conversion. Reject equal resolved paths and filesystem
aliases; exclusive locking also prevents serving/conversion races.

For an in-memory create, extend shell's existing create syntax only:

```text
vlite-shell --db :memory: --create N --compact
  [--precision float32|float16] [--script FILE]
```

`:memory:` is a sentinel recognized only when `--create N --compact` are both
present; any other use stays an ordinary path (`Database::create(path, ..)`,
`src/bin/vlite-shell.rs:70`). This calls designed `create_compact_in_memory`;
durability is None. A normal shell `--db PATH --create N --compact
[--precision ...]` creates a compact file. `--compact` or `--precision` without
`--create`, or `--precision` without `--compact`, fails; `--compact` must be
valueless in `tool_support::args` and allowed by the shell's `check_args`
(`src/bin/vlite-shell.rs:56-59`). In memory, `verify`, `backup`,
`compact` and `upgrade` fail `invalid_input` without dropping the handle (they
`drop(db.take())` and reopen by path, `:105`, `:130`); `header`/`format`/`stats`
report `"file_bytes":null` (they read `std::fs::metadata(path)`, `:138`). Shell
`collections` adds precision/compact per row; `header`/`format`/`stats` add
compact plus selected collection precision, keeping their existing fields
(`src/bin/vlite-shell.rs:136-146`). No snapshot path or shell key option is
invented; encrypted memory construction belongs to Rust API qualification.

Server/import create switches remain ordinary Float32 and gain **no** compact
flags, avoiding an accidental opt-in through an open-or-create flow
(`src/bin/vlite-server.rs:495-544`; `src/bin/vlite-import.rs:437-441`).
Operators pre-create a compact store offline and open it after qualification;
the server's in-memory mode is unchanged.

## 7. Data behavior, preservation and clients

Dense primary/named/history inputs canonicalize as specified in
[Design A](p3-precision-api.md), "Behavior and compatibility matrix", and
[P3](p3-compact-vectors.md), "Source storage and reads". Sparse values stay
float32. Finite half overflow is rejected before revisions, import markers,
expiry or graph state change. Batch ordering and graph/vector atomicity remain.
Query dimensions/nonfinite/zero-norm validation and float32 query bits are
unchanged. Exact scoring uses stored canonical values, unchanged f64
accumulation/tie ordering; it does not promise original-input distances.
Public records/search responses retain their shapes and float32 components.

| Path | Required behavior, not an activation claim |
|---|---|
| Ingest | Every REST/gRPC/MCP/CLI write and import path uses the engine's same canonicalization. Existing reject/skip policy and submitted-input retry digest semantics remain; two distinct submissions rounding identically are not the same import retry. |
| Record export | Emit widened canonical float32 values, no new record field. Ordinary record export alone is not a precision-preserving store backup: pre-create the destination with explicit collection policies. Never infer policy from numeric values. |
| Backup/restore | Preserve opt-in, empty catalogs, every immutable precision and canonical value, including encrypted transport. Restore's existing identity behavior is not replaced by precision conversion. |
| Conversion | New lineage, head/floor=max(source head,1), preserve numeric revisions, IDs/high-waters, payload/metadata, named/sparse histories, graph attachments/revisions, closed imports, absolute TTLs including expired/deadline-zero state. Refuse open imports; do not TTL-compact. Widening never recovers discarded bits. |
| Feeds/export/replicas | Catalog restatements emit no events; ordinary precise create retains the existing CollectionCreated event shape. A feed is not a policy backup. Log export/replay carries guarded policy intact. Old-lineage cursors fail CursorCompacted; before-floor cursors fail CursorBeforeHistory. Fresh replica bootstrap is required after conversion; do not transplant cursors/ETags. Numeric CAS revisions are not lineage-bound: reread before conditional writes. |
| Maintenance | Faithfully preserve precision/intent or refuse before output, including compaction/upgrade/directory conversion. Never turn a policy descriptor into a discardable derived cache. |
| Encryption | Designed compact constructors accept CreateOptions.encryption. Conversion requires the source key, preserves encryption with fresh output salt, and has no destination-key option; intentional key change remains reencrypt. No plaintext sidecar/spill or silent decrypt. |

These restate Design A and publication decisions W/P, not today's private
converter, whose empty-selection/open-import behavior differs
(`src/maintenance.rs:288-307`).

Client additions are separate named methods to preserve all positional
arguments, .NET cancellation-token positions and existing return types:

```text
Python:
  create_collection_with_precision(name: str, dimensions: int,
      precision: Literal["float32","float16"],
      settings: CollectionSettings | None = None) -> CollectionInfo
JavaScript / TypeScript:
  createCollectionWithPrecision(name: string, dimensions: number,
      precision: "float32" | "float16", settings?: SettingsUpdate):
      Promise<CollectionInfo>
.NET:
  Task<JsonElement> CreateCollectionWithPrecisionAsync(string name,
      int dimensions, VectorPrecision precision, object? settings = null,
      CancellationToken cancellation = default)
  Task<JsonElement> GetCollectionAsync(string name,
      CancellationToken cancellation = default)
  enum VectorPrecision { Float32 = 1, Float16 = 2 }
```

All explicit methods POST the same JSON; .NET maps enum names explicitly to
lowercase strings, never integers, and rejects the undefined value 0 before
sending. Null settings arguments are omitted; there is no implicit retry,
precision coercion or fallback. Existing methods send no precision.
Python TypedDict/TypeScript response types add optional `precision`/`compact`
(and list `compact`); .NET keeps raw JsonElement. Typed descriptors:
`clients/python/vlite_client/__init__.py:203-226`,
`clients/javascript/index.d.ts:106-114`.
No accessor is proposed; an unrecognized returned precision is kept verbatim,
never defaulted to Float32. Absent fields on an old server remain absent;
display "unknown", not "float32".
No network client store-create/convert API, embedded binding or C ABI changes
are proposed. Generated gRPC clients receive the RPC/messages in section 4.

## 8. Errors, authorization and observability

REST errors retain `{"error":{"code":string,"message":string}}`, verified at
`src/server.rs:1407-1413`, with the optional `error.details` object proposed in
[wire conventions, section 3 and decision 21](serving-wire-conventions.md#3-encoding-rules-c12).
New codes below are proposed adapter mappings,
not new Rust Error variants. gRPC carries the same code in `vlite-error-code`
and message in status; current trailer machinery is
`src/server/grpc.rs:199-215`. CLI new-operation errors print that same JSON
envelope on stderr; MCP new tools use section 5. No paths, keys or submitted
vectors in public errors.

| Refusal / underlying cause | Stable code | HTTP | gRPC |
|---|---|---|---|
| Gate closed: any explicit precision (even `"float32"`); compact create or conversion is offline only. An adapter check, not an engine error: `precision_supported()` is `pub(crate)` (`src/compact/mod.rs:56-66`); `UnsupportedFeature(32)` is only an open-time refusal (`src/format.rs:1442-1453`) | `precision_not_enabled` | 409 | FAILED_PRECONDITION |
| Store below format V4: `CollectionsRequireUpgrade`, `ImportsRequireUpgrade`, `ChangeFeedRequiresUpgrade` (existing mapping, `src/server.rs:1510-1516`) | `requires_format_v4` | 409 | FAILED_PRECONDITION |
| Explicit precision (even `"float32"`) on a store not opted in; designed UnsupportedFeature(16), Design A's only (16) mapping (`p3-precision-api.md:197`) | `compact_required` | 409 | FAILED_PRECONDITION |
| Unimplemented supported-backing path/build, or compact with timestamps; UnsupportedFeature / UnsupportedFeature(2) | `unsupported_feature` | 409 | FAILED_PRECONDITION |
| Unsupported format version; UnsupportedVersion | `unsupported_version` | 409 | FAILED_PRECONDITION |
| Unknown/case-mismatched/null precision, enum 0/unknown; proposed `details.field="precision"` | `invalid_input` | 422 | INVALID_ARGUMENT |
| Malformed JSON text (REST body or gRPC `settings_json`) | `invalid_json` | 400 | INVALID_ARGUMENT |
| REST strict-schema/type/unknown-field rejection, including a non-null non-string top-level precision or `precision` in settings | `invalid_json` | 422 | n/a |
| gRPC `settings_json` non-object or strict/unknown/type error, including `precision` (`src/server/collections.rs:250-256`) | `invalid_input` | n/a | INVALID_ARGUMENT |
| Dimensions/name/settings invalid (designed InvalidCollectionSettings) | `invalid_input` | 422 | INVALID_ARGUMENT |
| Wrong vector width, nonfinite value or finite half overflow; DimensionMismatch/NonFiniteVector | `invalid_input` | 422 | INVALID_ARGUMENT |
| Collection missing / exists / limit / reserved default | `collection_not_found` / `collection_exists` / `collection_limit` / `default_collection_immutable` | 404 / 409 / 409 / 409 | NOT_FOUND / ALREADY_EXISTS / FAILED_PRECONDITION / FAILED_PRECONDITION |
| Open import during offline conversion; designed ImportConflict | `import_conflict` (existing wire code for other operations) | offline | offline |
| Read-only credential / wrong scope | `read_only` / `forbidden_scope` | 403 | PERMISSION_DENIED |
| Missing/invalid bearer | `unauthorized` | 401 | UNAUTHENTICATED |
| Suspended/deleted tenant | `tenant_suspended` / `tenant_unavailable` | 403 | PERMISSION_DENIED |
| Same/aliased source and destination, unsupported path/backing selection, duplicate conversion override (designed InvalidCollectionSettings) | `invalid_input` | offline | offline |
| Locked source / existing destination | `locked` / `destination_exists` | offline | offline |
| Missing/wrong key, inappropriate key/options | `encryption_key_required` / `encryption_key_mismatch` / `not_encrypted` / `invalid_encryption_options` | offline | offline |
| Corrupt image / incomplete source | `corrupt` / `incomplete_tail` | offline | offline |
| Old lineage / before history floor / numeric CAS mismatch | `cursor_compacted` / `cursor_before_history` / `precondition_failed` | 409 / 409 / 412 | FAILED_PRECONDITION / FAILED_PRECONDITION / ABORTED |
| Commit uncertain / database poisoned (`src/server.rs:1577-1593`; `src/server/grpc.rs:181-196`) | `commit_indeterminate` / `poisoned` | 503 | UNKNOWN / INTERNAL |
| Output publication uncertain (offline create/conversion) | `publication_indeterminate` | offline | offline |
| Admission saturated (existing; `src/server.rs:1368-1375`) | `busy` | 503 | UNAVAILABLE |
| Other I/O failure | `internal_error` (CLI: `io_error`, `src/bin/tool_support/graph_json.rs:28`) | 500 | INTERNAL |
| Old gRPC server lacks explicit RPC | `unimplemented` (client classification; no promised trailer) | n/a | UNIMPLEMENTED |

**Withdrawn:** the separate `invalid_precision` code duplicated semantic
validation; see [wire conventions, decision 6](serving-wire-conventions.md#6-owner-decisions-23).
Its replacement is the existing `invalid_input`, built as an `ApiError` by the
explicit-precision handler with the proposed field context from decision 21.

The proposed `precision_not_enabled`, `compact_required`,
`unsupported_feature` and `unsupported_version` REST/gRPC mappings are
`ApiError`s built only by explicit-precision handlers: respectively from the
adapter gate check, existing `Error::UnsupportedFeature(16)`, other existing
`Error::UnsupportedFeature` causes (including timestamps), and existing
`Error::UnsupportedVersion`. They add no global `From<Error>` arm or public
`Error` variant; see [wire conventions, decision 7](serving-wire-conventions.md#6-owner-decisions-23).
`unsupported_feature` uses the proposed `details.feature` and
`details.category` context from decisions 7 and 21.
Today those engine errors reach the 500 catch-all
`internal_error` (`src/server.rs:1624`) and legacy routes keep that. `offline`
rows arise only in the CLI or at startup, which stays a process error
(section 1), and create no HTTP/gRPC mapping. The CLI mapper
(`src/bin/tool_support/graph_json.rs:26-72`) knows only `io_error`, `locked`,
`read_only`, `precondition_failed`, `collection_not_found`, `invalid_input`,
`corrupt`, `incomplete_tail`, `unsupported_version`, `unsupported_feature`,
`encryption_key_required`, `commit_indeterminate` and `poisoned` of these codes
and falls back to `database_error`; the other CLI-reachable codes are new
(section 11, "Error vocabulary").
Their producers are CLI adapter checks for `precision_not_enabled` and
`compact_required`, existing `Error::Io` with AlreadyExists for
`destination_exists`, and existing `Error::EncryptionKeyMismatch`,
`Error::NotEncrypted`, `Error::InvalidEncryptionOptions` and
`Error::PublicationIndeterminate` for their corresponding codes; these
CLI-only mappings add no public `Error` variant or wire mapping.
Fixed gate message: `"precision storage is not enabled in this build"`;
compact-required message: `"explicit precision requires a compact store"`.
Other messages are sanitized and not machine-parsed.

Precedence, first failing step wins, never probing unauthorized store state:

1. Authorization and tenant admission.
2. Strict validation: `invalid_json` for shape failures; `invalid_input` for
   semantic precision failures and gRPC `settings_json` validation.
3. Gate: `precision_not_enabled`.
4. Store version: `requires_format_v4`.
5. Opt-in: `compact_required`.
6. Field and engine validation: `invalid_input`, `collection_exists`,
   `collection_limit`.
7. Mutation.

A gate-closed build refuses EVERY explicit-precision request, even `"float32"`,
at step 3, so steps 4-5 are reachable only with the gate open. Omitted-precision
(legacy) creates skip steps 3-5 and keep today's behavior.

Existing mappings: `src/server.rs:1460-1537` (CAS at `:1523-1527`), `:1577-1593`;
`src/server/grpc.rs:181-196`. A generic 503 must not imply a safe automatic
retry: after `commit_indeterminate` or `publication_indeterminate`, retain
recoverable output, stop/reopen and inspect; never repeat create or conversion
blindly.

Authorization: explicit precision creation needs **the same** collections
permission as ordinary create (`src/server.rs:853-860`, `:981-1017`; gRPC
`src/server/grpc.rs:849-858`): admin, or scoped credentials with `collections`
plus all-collections reach (a tenant wildcard reaches only its own namespace);
read-only denies. No tenant may enable a store, change another collection's
precision or convert; store opt-in and qualification are operator duties, not
credential permissions (section 11, "Creation authority").

Qualify local names once, then use physical names for accessors under the guard
and unqualify responses (`src/server/collections.rs:211-225`, `:283-315`;
`src/server/tenants.rs:462-475`). A physical tenant name never bypasses
isolation; bound-name/ID/lineage checks stay mandatory (`src/server.rs:1748-1765`).
Local MCP/CLI use process filesystem authority and locks; proxy MCP shares REST
grants, tenant suspension, quotas and read-only policy.

Audit proposal: add `precision`, `compact`, `transport` to collection_created;
rejected explicit operations emit `precision_operation_refused` (operation,
requested_precision or null, compact only after authorization, code, request_id,
authorized tenant/actor); offline create/convert audit adds old/new lineage and
per-collection policies, never paths/keys/payloads. Lifecycle audit stays
nonblocking: sink failures are counted and never undo a committed create
(`src/server/collections.rs:172-190`, `:309-312`). Counter
`precision_operations_total{operation,precision,outcome,code,transport}` uses
create_collection/create_store/convert, float32/float16/mixed/unspecified,
success/refused/indeterminate, the fixed code vocabulary and rest/grpc/mcp/cli;
no collection/tenant/path/credential labels.

## 9. Exact OpenAPI amendment inventory

`src/openapi.json` and `proto/vlite/v1/vlite.proto` stay frozen until
p3-serving-rest and p3-serving-grpc respectively start, after the Rust precision
API is integrated (R9). The current OpenAPI uses `/v1/collections/{c}`,
while routers use `{collection}` (`src/openapi.json:54`;
`src/server/collections.rs:13-15`). Keep the existing `{c}` key: no duplicate
path or placeholder rename. Proposed YAML below is a **deep-merge diff**:
add/replace only named leaves, retain existing settings, schemas, parameters,
responses and auth. Response additions do not enter any `required` array.
Existing CollectionInfo/CollectionSettings/Error schemas are
`src/openapi.json:159-169`, `:198`.

```yaml
paths:
  /v1/collections:
    get:
      operationId: ListCollections
      responses:
        '200': {content: {application/json: {schema: {properties:
          {compact: {type: boolean}}}}}}
    post:
      operationId: CreateCollection
      requestBody:
        content: {application/json: {schema: {properties:
          {precision: {$ref: '#/components/schemas/VectorPrecision'}}}}}
  /v1/collections/{c}:
    get:
      operationId: GetCollection
      responses:
        '200': {content: {application/json: {schema:
          {$ref: '#/components/schemas/CollectionDetail'}}}}
components:
  schemas:
    VectorPrecision:
      type: string
      enum: [float32, float16]
      description: Immutable dense stored precision; float16 is lossy.
    CollectionInfo:
      properties:
        precision: {$ref: '#/components/schemas/VectorPrecision'}
        compact: {type: boolean}
    CollectionDetail:
      allOf:
        - {$ref: '#/components/schemas/CollectionInfo'}
        - type: object
          required: [text_index]
          properties:
            text_index:
              type: object
              required: [declared_fields, available, documents, terms, postings]
              properties:
                declared_fields: {type: integer, minimum: 0, maximum: 8}
                available: {type: boolean}
                documents: {type: integer, minimum: 0}
                terms: {type: integer, minimum: 0}
                postings: {type: integer, minimum: 0}
  responses:
    PrecisionRefusal:
      description: Explicit refusal; codes in section 8.
      content: {application/json: {schema: {$ref: '#/components/schemas/Error'}}}
```

No schema `default` on response precision or compact: absence means unknown.
The existing POST 201 references CollectionInfo, so enrichment reaches it. PUT
settings 200 has no schema today (`src/openapi.json:55`); this slice adds
`{collection: CollectionInfo, persisted: boolean}`, both required (body built at
`src/server/collections.rs:407`). Existing POST 201/409/429 and `{c}` 404/429
stay (the 409 description gains the section 8 409 codes); the POST 400, 401,
403, 422, 500 and 503 entries are new, each
`{$ref: '#/components/responses/PrecisionRefusal'}`. Collection operations have
no operationId or tags today (the 12 existing ids are PascalCase): both are
owner decisions (section 11).

When the Rust API becomes public (section 11), each new public item needs a
`site/api.html` row with a matching `data-api` key (`tests/api_reference.py:12-23`,
`:261-289`; the Rust scan skips server/bin code, `:201-205`, the REST/gRPC/MCP
scans do not). Rust keys: `rust:Database::create_compact`,
`rust:Database::create_compact_directory`, `rust:Database::create_compact_in_memory`,
`rust:Database::create_collection_with_precision`, `rust:Database::is_compact`,
`rust:Database::collection_precision`, `rust:fn:convert_to_compact`; document
VectorPrecision in the constructor row, not as a type-only key. Transport keys:
`grpc:CreateCollectionWithPrecision`, `mcp:vlite_create_collection`,
`mcp:vlite_get_collection`.

## 10. Independent acceptance and delivery

Oracle plan: the [precision catalog oracle](../../../tests/precision_catalog_oracle.py)
and [float16 oracle](../../../tests/compact_float16_oracle.py) are models only
(`tests/precision_catalog_oracle.py:1-16`: op-17 plaintext and raw kinds 3/4/5
only; `tests/compact_float16_oracle.py:42-54`: binary16 encoding); neither
validates authorization, wire schemas, CLI lifecycles or production gates.

Goldens are hand-authored (HTTP JSON, protobuf binary, MCP transcripts, CLI
stdout/stderr/exit), never generated by the adapter under test; canonical float
bits come from numeric oracle fixtures. Assert required fields, presence, tags,
status/code/trailer and absence of unexpected fields.

**Gate-closed refusal goldens (required now; `precision_supported()` is false
outside tests, `src/compact/mod.rs:56-66`):**

| Matrix | Required assertions |
|---|---|
| Refusal | Every explicit-precision create, even `"float32"` or in an empty compact Float32 store, and every compact create or conversion fails `precision_not_enabled` before any engine call; no downgrade, fallback or file/source mutation. |
| Validation order | Each section 8 precedence step beats later ones; every bound and bound+1; enum unknown/zero and null/case failures use `invalid_input` with proposed `details.field="precision"`, wrong non-null precision types use `invalid_json`; gRPC `settings_json` text versus object errors. |
| Auth | Admin/scoped wildcard/tenant wildcard/read-only; 28/29-byte tenant local names, 63/64-byte unqualified names; qualify exactly once, scope before lookup, revoked/suspended/deleted grants, filtered lists. Proxy and local authority distinguished. |
| Legacy unchanged | Omitted-precision creates behave as today on every transport; record/search shapes and old Float32 files/constructors unchanged. |
| Compatibility | Old client/new response ignores extras; new client/old response keeps absent fields unknown. Old gRPC explicit RPC UNIMPLEMENTED; old REST rejects precision; no fallback. |
| Specification | Validate proposed schemas against hand-authored goldens; step-11 protoc descriptor tag/reservation check per section 4; OpenAPI operation IDs/paths and API-reference keys; clients preserve optional response fields and positional signatures. |
| Error shape/scope | MCP failures are strings per section 5; the four new 409 mappings apply only to explicit-precision handlers, legacy routes retain their mappings, and offline refusals create no wire mapping. |

**Gate-open success goldens (deferred to the activation decision; none written now):**

| Matrix | Required assertions |
|---|---|
| Defaults/immutability | Old requests choose Float32 beside Float16 default; explicit Float32 still requires compact. Settings defaults/replacement and empty/drop/recreate/reopen never inherit or mutate policy. |
| Enrichment parity | `precision`/`compact` on every create/list/info/settings descriptor, legacy files `"float32"`/`false`, equal across REST/gRPC/MCP/CLI; existing MCP count names retained. |
| Backings/lifecycle | File/directory/memory, empty/populated, plain/encrypted; fresh create, post-floor named create, failure before/after publication, lock conflict, source byte equality, absent/no-clobber output; unsupported codec/build/backing/format/timestamps, wrong key, corrupt/truncated image, open import, destination aliases/existence. |
| Numeric/ingest | Ties-to-even, signed zero, subnormal, underflow, overflow; primary/named/history/graph batches/import reject/skip/retry; query not canonicalized; unchanged IDs/order/distance and record shape. |
| Preservation | Export/reimport with explicit destination policy; backup/restore/reencrypt; TTL zero/expired state, histories/attachments/revisions/import high-waters; new lineage/floor, old cursor refusal and fresh replica bootstrap. |

Mutations that must each fail a targeted assertion (test-only, never a release
option): drop a precision/compact response field; return Float32 for absent
metadata; inherit default precision; permit settings precision; route explicit
Float32 to legacy create; round queries; omit precommit rounding/overflow
refusal; canonicalize retry digests; reuse a proto tag; retry the explicit RPC
through old create; skip tenant qualification/auth; drop empty catalogs; reuse
conversion lineage; discard encryption/expired state; publish over existing
output; bypass the production gate.

Each slice starts **after the Rust precision API is integrated**, keeps storage
activation closed (so it lands only the gate-closed goldens; section 11, "Rust
API visibility during the hold") and requires independent acceptance, not only
its author's test run. File ownership below is proposed, not authorized by this
documentation task.

| Slice | Owned files | Required tests and independent-acceptance checklist |
|---|---|---|
| `p3-serving-rest` | `src/server/collections.rs`, `src/server.rs`, approved `src/openapi.json`; coordinated audit/metrics in server modules | Contract goldens; strict schema/defaults/atomic reads; auth/tenant/refusal matrix; review exact errors and schema against independent fixtures; API-reference rows complete. |
| `p3-serving-grpc` | `proto/vlite/v1/vlite.proto`, `src/server/grpc.rs` | Step-11 descriptor/tag/reservation goldens per section 4, generated-old-client interoperability, explicit-RPC UNIMPLEMENTED/no-fallback, REST parity and trailers; independent sign-off on no tag reuse or default ambiguity. |
| `p3-serving-cli-mcp` | `src/bin/vlite-maintain.rs`, `src/bin/vlite-shell.rs`, `src/bin/vlite-mcp.rs`, relevant `src/bin/tool_support/` proxy support, `Cargo.toml` | CLI process/lock/no-clobber/source-byte goldens; local/proxy MCP schemas and errors, read-only rejection, offline-only conversion; independently exercise Windows aliases/publication failures and production refusals. |
| `p3-serving-clients` | `clients/python/vlite_client/__init__.py`, `clients/javascript/index.js`, `clients/javascript/index.d.ts`, `clients/dotnet/VLite.Client/Client.cs`, their existing tests | Request goldens, optional response fields, no coercion/retry, cancellation/positional compatibility, old/new server matrix; independent review against approved REST/proto, not client-generated expectations. |

At [serving-order step 11](serving-wire-conventions.md#51-order), internal
order: rest, grpc, cli-mcp (its proxy needs rest), clients (need rest/proto
goldens). Each slice owns its reference updates and tests; none may edit shared
storage gates to make an adapter test pass. Activation of float16 remains a
**separate qualified opt-in decision** after lifecycle/preservation
qualification for every promised backing. Float32 stays the default.

## 11. Owner decisions and open items

| Decision to approve | Options | Recommendation |
|---|---|---|
| Explicit gRPC creation | New RPC as above / optional field on old RPC with capability handshake | New RPC: old-server UNIMPLEMENTED prevents ignored-field Float32 fallback. |
| Creation authority | Same collections permission / admin-only precision request / new credential scope | Same permission after operator store opt-in; no gate or scope expansion. |
| Response placement | Add precision+compact to each descriptor and compact to list / separate precision endpoint | Additive fields as specified; Rust CollectionInfo stays unchanged, reads are atomic. |
| Remote conversion | None / admin-only path-based endpoint with separate lifecycle protocol | None. Offline CLI only; never convert in place or while serving. |
| CLI coverage | Maintain persistent create + shell memory / add server/import activation flags / add a directory-output conversion route | Maintain/shell only; conversion writes a file (no CLI route to `convert_to_directory`); `--encryption-key-file PATH`; server/import defaults untouched. |
| Error vocabulary | Contextual stable codes above / generic unsupported_feature for all causes | Contextual codes; extend the CLI mapper (`src/bin/tool_support/graph_json.rs:26-72`) with every section 8 code the CLI reaches; legacy mappings unchanged; uncertainty distinct. Wire conventions, decision 6 (owner-selected); Wire conventions, decision 7 (owner-selected); Wire conventions, decision 21 (owner-selected). |
| MCP response names | Preserve existing collections/count / rename to REST items/live_records | Preserve existing keys and add fields; only new tools use REST descriptor shapes. |
| Rust API visibility during the hold (`p3-precision-api.md:188,229,242`; `src/compact/mod.rs:56-66`; `src/bin/vlite-mcp.rs:18`) | Design A unshipped, `pub(crate)` gate, adapters refuse before the engine / `pub` API with gate-closed refusal / `pub` behind a cargo feature | Unshipped: bins see only the public API, so CLI/MCP land parsing and refusals; success goldens (section 10) wait for a `pub` API and the activation decision. No new Error variant or feature flag. |
| New MCP tools and proxy extension (`src/bin/vlite-mcp.rs:24-26`, `:464-474`) | Both, local and proxy / local only | Both, with REST grants; the proxy never elevates (section 5). |
| Structured MCP error (`src/bin/vlite-mcp.rs:673-681`: string errors today) | New tools only / all tools | All tools: one string failure `"<code>: <message>"`; the new-tool-only object-shaped proposal is withdrawn. Wire conventions, decision 22 (owner-selected). |
| Audit and metric additions (section 8) | As specified / audit only / none | As specified; no tenant, path or collection labels. |
| .NET public API (`clients/dotnet/VLite.Client/Client.cs:119-133`) | Enum plus named method / string argument | Enum plus named method; lowercase wire strings. |
| Store-level `compact` in the list response | Include / omit | Include: one persisted store flag. |
| `history_floor` in conversion output (`Inspection` has none, `src/maintenance.rs:24-34`) | Include / omit | Omit; output `collections` and `lineage`. |
| In-memory CLI syntax (`src/bin/vlite-shell.rs:70`) | `:memory:` sentinel with `--create N --compact` / new `--memory` flag | Sentinel; any other use stays a path. |
| `vlite-maintain` packaging (`Cargo.toml:15,20,63`; `.github/workflows/release.yml:102,209`; `Dockerfile:53`; `tests/maintenance.rs:191,479`; `tests/reliability.rs:344`) | (a) `[[bin]] required-features=["json"]`, breaks default-feature tests / (b) `cfg(feature="json")` on the new commands only / (c) hand-written JSON | (b). |
| OpenAPI tags and operationIds (none on collection operations; the 12 existing ids are PascalCase) | PascalCase ids, no tags / camelCase ids / tags | PascalCase ids (`ListCollections`, `CreateCollection`, `GetCollection`), no tags. |

Open blocking items: approve these choices; integrate the designed Rust API
(empty-override conversion, reviewed import errors); qualify each backing and
maintenance/transport path or keep refusing; verify old-client interoperability.
No choice here waives publication, numeric, encryption or production-writer
gates.
