# Independent `vlite-cypher-1` front-end and read fixtures

Source: `docs/projects/roadmap/gap6-cypher-subset.md`, especially sections
3, 4, 5, 6, 7, 9 and 12. All cases are hand-authored original local scenarios; none are
Rust-derived, imported TCK material, or oracle-generated expectations. This is
Part A and Part B, not a Cypher conformance claim.

## Running and embedding

From the repository root:

```text
python -B tests\graph_subset_oracle.py --self-test
python -O -B tests\graph_subset_oracle.py --self-test
python -B tests\graph_subset_oracle.py --self-test --fixture tests\fixtures\graph_subset
```

The default directory is beside the Python module, independent of the working
directory. No fixture is ever generated or rewritten. Python 3.10+ and only its
standard library are required. Set `PYTHONDONTWRITEBYTECODE=1` when embedding.

`parse(text, params=None, params_raw=None, max_hops=2)` returns a `Statement`
tree or raises `Diagnostic(kind, offset, reason=None)`. `outcome` returns the
JSON-shaped expected result described below. Reference values remain in the
external typed map; no parameter content is interpreted as language.
`format_statement` emits a canonical grammatical representation, quoting names
and preserving expression precedence. Formatting changes byte positions but
must preserve the statement tree, parameter roles and binding order.

## JSON containers and hashes

`parse-cases.json` has exactly:

```json
{"schema":"vlite.graph-subset-parse","version":1,"content_hash":"<64 lowercase hex digits>","cases":[]}
```

The hash is SHA-256 of UTF-8 JSON for the entire object **excluding only its
top-level `content_hash` member**. Recursively sort object keys, retain array
order, omit whitespace between tokens, preserve non-ASCII characters rather
than escaping them, and reject NaN/Infinity. This is Python's
`json.dumps(body, ensure_ascii=False, sort_keys=True, separators=(",", ":"),
allow_nan=False)`. The files themselves use deterministic hand-authored key
order, which need not match the canonical hash order. Whitespace changes alone
do not change the hash.

`mutant-map.json` uses the same hash algorithm and has exactly `schema`,
`version`, `content_hash`, `mutants`, `read_mutants`; its schema is
`vlite.graph-subset-mutants`, version 1. Each entry has exactly `name`, `change`
(nonempty description), and `cases` (nonempty array of named parse-case IDs).
Names are unique, reference real implemented switches, and cover every switch.
Every listed case must fail exact comparison when its switch is enabled; a
different diagnostic kind or offset also constitutes a kill. Exceptions are not
used to count kills. Switches are reset after the campaign.

## Case schema

Required fields: `id`, `group`, `origin`, `cite`, `text`, `expect`, and exactly
one of `params` or `params_raw`. Optional fields: `marked`, `offset_note`,
`max_hops`. Unknown fields at any structural level are rejected.

`id` is a unique stable lowercase ASCII identifier matching
`[a-z0-9][a-z0-9-]*`. `origin` is `local`; `cite` identifies the specification
section/rule used in hand reasoning.

Known groups are `read`, `write`, `lexical`, `excluded`, `syntax`, `static`,
`parameter`, `open-cap-offset`, `open-encoding`, `open-envelope`,
`open-static-offset`, `open-param-domain`, `open-caller-hop`.
Groups starting with `open-` are checked normally but counted separately: they
pin a conservative interpretation of an underspecified position, encoding or
classification. See root `oracle-report.md` for the questions and citations.
Do not treat these as uniquely specified public diagnostic coordinates.

`max_hops` is an optional integer caller setting, default 2, ceiling 4.
Zero is legal for node-only statements. Relationship patterns require at least
one permitted hop; bounds above this setting use the exact section 6 reason
`subset hop bound exceeds max_hops`. This caller check is separate from the
dialect's fixed upper bound of 4; it never clamps or raises the caller setting.
Other execution limits are Part B inputs, not silently defaulted by this parser.

### Text and long-value form

`text` is either a UTF-8 string or this exact object:

```json
{"prefix":"MATCH (n)","unit":" ","count":16366,"suffix":" RETURN n"}
```

Expand as `prefix + unit * count + suffix`, with no extra whitespace.
`count` is an integer in 0..20000, never a Boolean. The same single form may
replace a tagged String's `value`, allowing hand-reviewable large parameter
values. No other macros, templates, filename substitutions or generated
parameter names are supported. Expansion precedes parsing/value validation.

### Typed parameters

`params` is an object whose ASCII bare-name keys map to tagged values:

| Tag | Exact fields and payload |
|---|---|
| String | `{"tag":"String","value":"UTF-8 text"}`; value may use the long-value form |
| Integer | `{"tag":"Integer","value":"-9223372036854775808"}`; signed decimal **string**, no plus/leading zeros, i64 range |
| Float | `{"tag":"Float","value":1.0}`; finite JSON number interpreted as binary64 |
| Boolean | `{"tag":"Boolean","value":true}`; JSON Boolean, never 0/1 |
| GeoPoint | `{"tag":"GeoPoint","latitude":12.5,"longitude":-45.0}`; finite numbers, latitude [-90,90], longitude [-180,180] |

The tag defines the value type. An Integer is never coerced to Float, a Boolean
never to Integer; whole i64 values survive JSON readers without loss. A Float
payload spelled `1` is still Float because of its explicit tag. Missing
properties' expression null is not a parameter tag.

`params_raw` is a **JSON text string**, not an object or a long-value form.
Parse it preserving object pairs before constructing the parameter map.
Duplicate root entries produce Parameter before entries can be discarded;
duplicate members inside a tagged value instead produce InvalidMetadata.
This field also permits intentional invalid tags, value shapes and JSON bodies
without making the outer fixture itself malformed. Ordinary `params` validates
tag/payload structure strictly; intentional out-of-range values retain their
valid structural shape for boundary tests.

Fixed limits: 64 distinct names, ASCII bare names at most 256 bytes, total
metadata encoding at most 65536 bytes; failures are Limit at statement EOF.
Missing parameters point to their first reference; wrong-role parameters point
to the first reference whose role cannot accept the value (12(k)). Unused
parameters point to EOF. KEY requires a String at planning; an empty String
is accepted by `key-empty` and rejected as InvalidQuery at execution (12(j)).
ID/SKIP/LIMIT require a
nonnegative Integer; AS OF requires a signed Integer. Generic property parameters
accept every valid tag. Rulings 12(j/k) settle the formerly open empty-KEY
and repeated-name role cases; their existing case IDs and groups are retained.
Individual invalid typed values use InvalidMetadata, not a language Parameter
error. Graph/model validation and mutation conflicts are separate from accepting
a front-end tree.

**Metadata encoding (12(l)):** four bytes for the map count;
each entry adds two bytes for name length, UTF-8 name bytes, one tag byte, and
payload. Integer/Float payloads are eight bytes, Boolean one, GeoPoint sixteen;
String adds four bytes for length plus UTF-8 data. Thus a one-byte name and
4096-byte String cost 4104 bytes. Fifteen such entries plus a final String of
3963/3964/3965 bytes give 65535/65536/65537 including the count:
`4 + 15 * 4104 + 8 + payload_length`. The existing `open-encoding` group
is retained for stable fixture organization, not because the encoding is unruled.

### Accepted expectation

Exactly these fields:

```json
{"outcome":"accept","kind":"read","columns":["n","x"],
 "references":[["key",13]],"bindings":[["n","node"]]}
```

`kind` is `read` or `write`, determined by mutation clauses, not RETURN presence
or whether execution would change anything. `columns` is the ordered projection
name list, empty for a write without RETURN. Unaliased fields use `variable.name`;
otherwise use the decoded alias or variable name. A field needs an alias to be
sorted, even when the dotted column could be quoted.

`references` lists **every occurrence**, including repeated names, in textual
order as `[name, UTF-8 byte offset of dollar]`. `bindings` lists first declarations
in textual order as `[decoded name, "node"|"rel"|"path"]`; repeated endpoint node
variables do not add another binding. CREATE edge endpoints are references, not
declarations. Path declaration precedes its pattern variables. Parsing a write
does not promise that runtime key/schema/deletion constraints will succeed.

### Rejected expectation and marker

Exactly `{"outcome":"reject","error":{"kind":"Syntax","offset":7}}`, with optional
`error.reason` **only** for a specification-fixed reason. The four language kinds
are Syntax, Unsupported, Parameter and Limit. Separate section 7 outcomes
InvalidMetadata, InvalidQuery and GraphLimitExceedsCeiling are also represented;
the fixture's offset for an out-of-text envelope is a documented open choice.

Every inline rejection has `marked`: the identical text with exactly one
U+27E6 (`⟦`) inserted **before** the offending character, or at EOF. No `text`
contains this marker. Remove it to recover `text`; encode the prefix as UTF-8 to
obtain `error.offset`, independently of any lexer. Generated-form rejections
instead have a nonempty `offset_note` documenting the byte arithmetic.

## Validation and limits

The self-test validates both hashes, strict structures, IDs/groups, marked
positions, exact complete outcomes, malformed in-memory copies, five seeded
case/map shuffles, every accepted parse/format/parse round-trip, 600 text/token
mutations, 200 raw-parameter mutations, exact numerical boundary checks and the
named mutant campaign. It uses explicit raises, not assertions, including under
`python -O`. No trial writes its mutated inputs back to disk.

Bytes/tokens are checked by lexing before parsing; therefore token cap-1 and cap
cases deliberately contain duplicate labels and end with Syntax, whereas cap+1
ends with Limit. This isolates the token threshold without inventing hundreds
of unique model labels. Parentheses and atomic predicates include their stated
cap-1/cap/cap+1 cases; inline MATCH maps share the predicate budget. CREATE/MERGE
property maps are values, not predicates. Fixed parameter count/encoding boundary
cases similarly reach unused-parameter errors below/at the cap and Limit above it.

The module has separate lexer, parser, static/parameter analysis, evaluator
placeholder, fixture validation and self-test sections. Part B can consume its
plain data-class tree and add evaluation/fixtures at the marked boundary.
The Part A front end does not perform graph lookup or runtime evaluation.
Part B below evaluates reads only. Neither implements authorization, schema
planning, writes, transport, persistence or durability.

## Part B read fixtures

`evaluate_read(graph, statement, params, limits, clock)` accepts either text or a
Part A `Statement`, never reads real clocks, and returns `NOT_MODELED` for writes.
`read-cases.json` uses schema `vlite.graph-subset-read`, version 1, the same
canonical hash, a `graphs` object and a `cases` array. Every case has exactly
`id`, `group`, `origin`, `cite`, `graph`, `params`, `clock`, `limits`, `text`,
`expect`; `graph` is a shared graph ID or inline graph. There are 218 original
local cases and 18 small shared graphs. All expectations were authored from
specification reasoning, never emitted by the evaluator.

Graphs have exactly `nodes`, `edges`, `temporal_capable`, `ttl_clock_ms`,
`view_sequence`. Nodes have decimal-string u64 `id`/`revision`, nonempty `key`,
`labels`, typed `properties`, optional decimal-string u64 `expires_at_ms`.
Edges have the same identity/properties/revision, nullable `key`, `type`,
decimal-string `source`/`target`, optional expiry and optional `segments`.
Segments have nullable signed-i64 decimal-string `from_ms`/`to_ms` and their
own typed `properties`; intervals are ordered, disjoint and half-open.
Graph inputs must be states the engine can hold: it allocates node and edge IDs
up to u64 max minus one and rejects u64 max with `IdSpaceExhausted`
(`src/property_graph/engine/state/tests.rs`), so the `wide` graph uses u64 max
minus one for its IDs and u64 max only for revisions and `view_sequence`. The
engine also stamps every entity with the commit sequence that last wrote it
(`src/property_graph/engine/state/temporal.rs`, `apply_temporal`), so no
revision may exceed its graph's `view_sequence`.

Results have `outcome: result`, ordered `columns`/`rows`, signed decimal-string
or null `as_of_ms`, decimal-string `ttl_clock_ms` and `view_sequence`.
Every scalar is tagged; missing properties encode `{"tag":"Null"}`.
Node/Edge tags include their complete identity/properties/revision (labels on
Node; type/source/target on Edge). Path tags have `length`, decimal-string
`edge_ids` and `node_ids`. Errors have `outcome: reject` and `error` with a
language `kind`/byte `offset`, or a runtime `kind`/fixed `reason`, or
`{"kind":"UnsupportedFeature","feature":64}`. No counters are claimed: the
specification does not fully fix all evaluator ordering/layout units.
An overflow of default C->i64 is an open failure classification with no invented
public reason; see `open-clock-conversion`.

Read groups: `scan`, `anchors`, `labels`, `maps`, `directions`, `trails`, `scope`,
`logic`, `equality`, `comparison`, `ordering`, `pagination`, `entities`, `time`,
`limits`, `clock`, `not-modeled`, `open-accounting`, `open-frontier`,
`open-scratch`, `open-clock-conversion`. Unknown groups and fields, duplicate
IDs/JSON keys, invalid hashes, dangling graph IDs/endpoints, overlapping
segments, invalid scalar domains and malformed result cells are rejected.
Fixture validation never silently normalizes graph inputs.

### Clock schema

`clock: {}` defaults to the graph's fixed C, elapsed zero and not cancelled.
All fields are optional:

| Field | Meaning |
|---|---|
| `ttl_clock_ms` | Decimal-string u64 C override, captured once before graph lookup |
| `ttl_samples_ms` | Nondecreasing decimal-string u64 hypothetical later samples; ignored except by the sample-C mutant |
| `elapsed_before_ms` | Nonnegative u64 elapsed milliseconds before work, default 0 |
| `monotonic` | Ordered `{at_work, elapsed_ms}` samples; work indices strictly increase, elapsed never decreases |
| `elapsed_by_phase` | Object mapping `before`, `interval`, `sort`, `finish` to elapsed milliseconds |
| `cancelled` | Boolean cancellation before work, default false |
| `cancel_at_work` | Nonnegative work index at which cancellation becomes true |
| `cancel_phases` | Array of phase names at which cancellation becomes true |

Elapsed time is the maximum applicable fake sample, never C. A deadline is
exhausted at elapsed >= deadline. The deterministic reference checks before
work, at combined work multiples of 64, before/during sorting and at read
completion. Completion checks are a reference policy: section 6 does not
require a read-finish checkpoint, so gap R07 remains unspecified with no new
fixture or switch. Sort checks are more frequent than the maximum permitted interval.
The two unit-64 fixtures are `open-accounting`: they name this exact policy,
not a uniquely required competing-error order for every implementation.
Write/append cancellation and rollback are deliberately NOT_MODELED.

### Caller limits

`limits: {}` uses these defaults, with no clamping:

| Field | Default | Ceiling |
|---|---:|---:|
| `max_hops` | 2 | 4 |
| `max_visited_nodes` | 1000 | 10000 |
| `max_edges_examined` | 10000 | 100000 |
| `max_edges_per_node` | 1000 | 10000 |
| `deadline_ms` | 250 | 5000 |
| `max_output_nodes` | 100 | 1000 |
| `max_output_edges` | 200 | 2000 |
| `max_output_paths` | 20 | 100 |
| `max_binding_rows` | 1000 | 10000 |
| `max_frontier_entries` | 1000 | 10000 |
| `max_returned_rows` | 100 | 1000 |
| `max_work_units` | 100000 | 1000000 |
| `max_scratch_bytes` | 33554432 | 134217728 |
| `write_deadline_ms` | 100 | 1000 |

Above-ceiling values are refused first, then invalid zero work budgets.
Only hops, returned rows and output node/edge/path caps permit zero.
The write deadline is validated as a caller input but never used to execute
writes. A relationship's requested upper bound must fit caller max_hops.

Node scans charge each scanned node; expansions charge every adjacency before
TTL/type/validity/repeated-edge filtering. Distinct visited nodes include starts;
per-expansion degree accounting restarts on each expansion, including revisits.
Each atomic predicate, projected row, sort comparison, frontier push/pop and
key resolution costs one work unit. Work is never reset between phases.
Binding rows are bounded before insertion, including later WHERE rejects.
Returned-row and distinct output-entity caps apply after ORDER/SKIP/LIMIT;
path caps count occurrences, including projecting a path twice. Path-nested
entities count too. LIMIT 0 does not bypass scans, projection or sorting.

### Open accounting and independent boundary checks

The specification does not fix a frontier scheduling algorithm or an owned
scratch layout/release schedule. This evaluator keeps deterministic depth-first
pending trails and a conservative cumulative logical owned-byte ledger. It
charges before creating binding/frontier/sort/output structures, but that
ledger is not a Rust allocation-size or peak-live-memory claim.
Logical scalar sizes reuse Part A's count/name/tag/payload convention; binding
references and ID slots cost eight logical bytes, and owned entity output
accounts keys/labels/types/properties/IDs/revisions.

No golden invents an exact graph scratch-byte count or frontier peak.
`open-scratch` pins only the guaranteed one-byte failure and caller
ceiling-1/ceiling/ceiling+1 outcomes. `open-frontier` likewise pins the caller
ceiling triple for a tiny trail. Seventy-five separate numerical trials check
all caller ceilings and every insertion/work/output/scratch bound at declared
cap-1/cap/cap+1 counts. In particular, frontier exhaustion is verified against
explicit requested insertion counts and scratch against declared owned bytes,
without pretending those counts are uniquely implied by a graph.
Exact graph-level exhaustion triples for these two resources remain dependent
on specification clarification. Read C->i64 overflow's public kind and precise
check/competing-error order are also open; see `oracle-report.md`.

### Read self-test and mutation campaign

Both required normal and `-O` self-tests preserve all Part A checks and add
exact comparison of 218 read cases, 38 malformed read/map trials, 75 numerical
boundaries, five seeded case/node/edge shuffles, input immutability and an
independent returned-path consistency check against graph endpoints/TTL/time.
`mutant-map.json` has 63 Part A entries plus 70 read switches
(16 required behavior switches and 54 additional switches), with 79 named
read case/switch kills. Every named mapping is actually evaluated; exceptions
never count as kills. Both campaigns restore switches in `finally` blocks.
The read map has the same strict name/reference/coverage/hash validation.
Missing `read-cases.json` cannot produce a successful Part A-only self-test.

The Part A revision has 303 cases. `reject-empty-key` reintroduces the former
planning error and is killed by `key-empty`; `allow-key-type` now tests only
the String requirement, killed by `key-wrong-tag`. `type-first-name-offset`
reintroduces the old offset and is killed by `type-error-first-reference`.
Rulings 12(a/b/c/g/h) fix lexical classifications, opening-backtick diagnostics
and star diagnostics; 12(d/e/f/i/m) have explicit mutation/UDF/shorthand audit cases.

Ten read fixture gaps are closed: R06, R09, R12, T02, W02, W03, W04,
W05, W07 and W08. `key-empty-execution` also kills `read-allow-empty-key`.
R06 isolates the before phase for cancellation and elapsed time. R12 uses
incoming traversal from a live target to an expired source. T02 has the exact
default-time i64 max / max+1 pair beside the older over-range case; the rejection
kind remains an explicitly open conservative InvalidQuery without a reason.

Work-boundary pairs use tiny graphs and cap / cap-minus-one: unanchored scan
2/1, two-row ORDER BY 5/4, two-row canonical ordering 12/11, one-node
one-hop frontier insertion and pop 7/6 each, type-filtered adjacency 4/3,
and inline property-map predicate 3/2. All reject with the exact section 7
reason `subset work budget exhausted` below the total, and all have named
removal switches. The full switch/case mapping and hashes are in the JSON.
