# Gap 6: bounded Cypher-style subset with writes

Status: private embedded design choices approved individually on 2026-11-02;
nothing in this proposal is implemented or qualified.
This document follows the [serving wire conventions](serving-wire-conventions.md), whose shared owner decisions were selected on 2026-11-02; items still unchecked for it in that note's section 7 remain conformance work.
The dialect identity, semantics and limits below are approved design contracts,
not shipped APIs. Exact public Rust and transport surfaces remain gated.
Scope: one graph, bounded pattern reads and atomic graph writes in embedded Rust,
then separately approved serving. No storage format change.
The [next-major roadmap](next-major.md) includes Cypher with writes
(`docs/projects/roadmap/next-major.md:22`). The
[product gaps](../product-gaps/product-gaps.md) keep a general-purpose language out
of scope while including bounded graph queries with writes. The owner narrowed
the earlier openCypher wording on 2026-11-02 to this separate named,
non-conformant dialect (section 11).

## 1. Requirements and non-goals

1. Accept exactly the grammar in section 3; reject other syntax before mutation,
   with a byte position and the error category in section 7.
2. Keep the existing graph-text parser, typed APIs and their results unchanged.
   An absent selector always means the legacy language; `vlite-cypher-1` is
   reachable only through a new route/tool/RPC, never a field on the closed
   `/query` body (`src/openapi_graph.json:2998-3019`;
   `src/bin/tool_support/graph_json.rs:1600-1610`). Never auto-detect or
   reinterpret text.
3. Execute every read against one committed view, one TTL clock C and one validity
   instant T; enforce direction, labels, types, predicates and all bounds.
4. Bind data only through typed parameters; reject missing, unused and mistyped
   parameters. Never parse parameter content as query text.
5. Execute a write statement as one atomic batch in one commit frame with the same
   limits as `GraphMut::write_batch`: at most 4,096 operations and one 16 MiB
   op-12 frame, never hidden splitting. The public method takes pre-built ops
   (`src/property_graph/mod.rs:397-403`) and cannot express MERGE, so this needs a
   crate-private entry sharing the existing commit path (section 5).
6. Preserve keys, revisions, TTL, temporal capability gates and schema constraints;
   failed planning must change nothing, including tentative expiry cleanup.
7. Produce deterministic rows, mutations and errors for a fixed view, clocks and
   parameters. Exhaustion is an explicit failure, never an empty success.
8. Qualify against an independent interpreter, golden scenarios, mutations, fuzzing
   and typed-API differentials before publishing any surface.
9. Authorize the statement's read/write class and graph binding before lookup;
   tenant and collection permissions must not be inferred from query content.

Non-goals: full openCypher/GQL conformance; procedures, UDFs, subqueries,
aggregation, joins, multi-graph transactions, schema DDL, vector/attachment writes,
TTL setters, temporal-segment writes, transaction-time history, streaming results
and continuation cursors. This is not a replacement for the existing shortest-path,
algorithm or hybrid retrieval APIs.

## 2. Current coverage, verified in this checkout

| Current fact | Verified evidence |
|---|---|
| Existing text is explicitly not Cypher/GQL; each accepted text lowers to one typed query, with no writes. | `docs/GRAPH-QUERY.md:3-13`; `src/property_graph/engine/language.rs:1-16` |
| Node/edge gets, node find, neighbors, BFS traversal and unweighted shortest path exist. | `src/property_graph/engine/query.rs:44-94`; `docs/GRAPH-QUERY.md:93-98` |
| Text supports KEY/ID anchors, labels, types, directions, bounded hops, AND conditions and projection; ORDER BY, SKIP, OR/NOT and writes are rejected. | `docs/GRAPH-QUERY.md:51-80,93-110,129-133` |
| Text query/explain delegate to typed read/explain; public text errors separate query rejection from engine failure. | `src/property_graph/read.rs:381-408,908-932` |
| Text caps are 16,384 bytes and 1,024 tokens; errors are Syntax, Unsupported, Parameter and Limit with byte positions. | `src/property_graph/engine/language.rs:26-65` |
| Typed reads report budget/deadline/cancellation truncation rather than execution errors. | `src/property_graph/engine/query.rs:1-8`; `docs/GRAPH.md:112-113` |
| Node keys are required, edge keys optional; IDs are graph-local and never reused. Types and labels are case-sensitive model strings. | `src/property_graph/engine/model.rs:14-30,76-98,138-170`; `docs/GRAPH.md:45-47` |
| Typed mutations provide create, patch, delete, expiry, attachment, vector and schema operations; keyed edge create can be idempotent. | `src/property_graph/mod.rs:220-329` |
| One batch plans earlier operations into tentative state, then validates/appends/applies one frame. | `src/property_graph/engine/mutate.rs:1-15`; `src/property_graph/mod.rs:397-402,721-746,878-1034` |
| TTL hides due nodes and incident edges; writes reclaim at one clock and failed batches reclaim nothing. | `docs/GRAPH.md:58-94`; `src/property_graph/mod.rs:921-936` |
| Temporal read views exist privately behind the capability gate; AS OF is not existing text syntax. | `src/property_graph/temporal/read.rs:1-3,69-107`; `docs/GRAPH-QUERY.md:51-74` |
| Schemas enforce required/typed/unique properties, endpoints and multiplicity; relationship property indexes/uniqueness are disallowed. | `docs/GRAPH.md:117-120`; `src/property_graph/engine/schema.rs:260-285`; `src/property_graph/engine/error.rs:101-122` |
| REST graph text is read-only and returns text errors as 422; graph privileges and tenant qualification are separate from collection scope. | `src/server/graph.rs:14-43,644-698`; `docs/GRAPH.md:277-296` |

These are current facts, not evidence that the new executor exists. In particular,
new row/path semantics below cannot generally lower to one current `GraphQuery`.

## 3. Exact language

Proposed dialect name: **VLite Cypher Subset 1** (`vlite-cypher-1`).
Keep legacy `parse_graph_query`/`query_text` on their current contract
(`src/property_graph/read.rs:421-440,908-932`). New parser/read/write entry points
are separate; their public Rust names also need review.

EBNF uses `[]` for optional, `{}` for repetition, `|` for alternatives.
Keywords are ASCII case-insensitive; identifiers are case-sensitive.
The parser consumes EOF, not a prefix. No comments or statement terminator.

```ebnf
statement   = read | write ;
read        = match, [ "AS", "OF", param ], [ where ], returning ;
write       = [ match, [ where ] ], mutation, { mutation }, [ returning ] ;
match       = "MATCH", [ ident, "=" ], node, [ rel, node ] ;
node        = "(", ident, { ":", name }, [ anchor ], [ props ], ")" ;
anchor      = ( "KEY" | "ID" ), param ;
rel         = "-", detail, "->" | "<-", detail, "-" | "-", detail, "-" ;
detail      = "[", [ ident ], [ types ], [ anchor ], [ hops ], [ props ], "]" ;
types       = ":", name, { "|", [ ":" ], name } ;
hops        = "*", "1", "..", uint ;
props       = "{", [ name, ":", param, { ",", name, ":", param } ], "}" ;
where       = "WHERE", disjunction ;
disjunction = conjunction, { "OR", conjunction } ;
conjunction = negation, { "AND", negation } ;
negation    = [ "NOT" ], predicate ;
predicate   = "(", disjunction, ")"
            | field, ( "=", param | "<>", param | "<", param | "<=", param
                     | ">", param | ">=", param | "IS", [ "NOT" ], "NULL" ) ;
mutation    = create | merge | set | remove | delete ;
create      = "CREATE", creation, { ",", creation } ;
creation    = newnode | newedge ;
newnode     = "(", ident, { ":", name }, "KEY", param, [ props ], ")" ;
newedge     = "(", ident, ")", "-", "[", ident, ":", name,
              [ "KEY", param ], [ props ], "]", "->", "(", ident, ")" ;
merge       = "MERGE", ( newnode | keyededge ) ;
keyededge   = "(", ident, ")", "-", "[", ident, ":", name,
              "KEY", param, [ props ], "]", "->", "(", ident, ")" ;
set         = "SET", assignment, { ",", assignment } ;
assignment  = field, "=", param | ident, ":", name ;
remove      = "REMOVE", removal, { ",", removal } ;
removal     = field | ident, ":", name ;
delete      = [ "DETACH" ], "DELETE", ident, { ",", ident } ;
returning   = "RETURN", item, { ",", item },
              [ "ORDER", "BY", sort, { ",", sort } ],
              [ "SKIP", param ], [ "LIMIT", param ] ;
item        = ( ident | field ), [ "AS", ident ] ;
sort        = ident, [ "ASC" | "DESC" ] ;
field       = ident, ".", name ;
param       = "$", bare ;
ident       = bare | quoted ;
name        = bare | quoted ;
bare        = ( letter | "_" ), { letter | digit | "_" } ;
quoted      = "`", qchar, { qchar }, "`" ;
uint        = digit, { digit } ;
letter      = "A" .. "Z" | "a" .. "z" ;
digit       = "0" .. "9" ;
```

`qchar` is a Unicode scalar other than backtick, NUL or a control character;
there is no escaping or doubled-backtick form. Whitespace between tokens is ASCII
space, tab, CR or LF; legacy accepts every `char::is_whitespace`
(`src/property_graph/engine/language.rs:189`), an intentional divergence. A symbol
is never split and the longest wins as in legacy (`->`, `<-`, `<>`, `<=`, `>=`,
`..`; `language.rs:172-175`). Reserved words (below) are rejected only as bare
`ident` (variables, aliases); quoting bypasses reservation, not model validation.
`name` (labels, relationship types, property and map-key names) accepts any bare
word, reserved or not, or a quoted name, as legacy does (`language.rs:585-594`), so
`n.key` and `n.id` are ordinary property names. `$name` is one token (no space
after `$`) and takes any ASCII bare word, reserved or not, as legacy does
(`language.rs:244-257`). Structural `uint` occurs only in a hop bound; all data,
keys, IDs, pagination values and timestamps are parameters. There are no
string/number/boolean/null literals, interpolated names, nested property maps or
list expressions.

Static restrictions are part of the accepted language:

- Exactly one MATCH pattern, containing one node or one relationship between two
  nodes; no comma-separated MATCH, chained relationships or implicit joins.
  Relationship MATCH requires the first node to have KEY or ID. Node-only MATCH
  may scan without an anchor. Brackets are mandatory.
- MATCH labels are conjunctive; relationship type alternatives are disjunctive.
  A relationship anchor is legal only without `*`. Its endpoints, direction, type
  and properties still have to match.
- Only `*1..N` is accepted; any other hop form (open, exact, parameterized, other
  or omitted minimum) is Unsupported. Bounds are decimal digits: a leading zero or
  N = 0 is Syntax, N above 4 (even beyond u64) is Limit, and N must also fit the
  caller's `max_hops` (section 6); legacy differs only by accepting leading zeros
  (`language.rs:733-797`). No zero-length path. A path variable requires a
  relationship. Any `*` form, even `*1..1`, is variable-length: it cannot bind a
  relationship variable (bind a path instead). Its properties constrain every
  traversed edge.
- Node variables may repeat only as the two endpoints, meaning identical IDs.
  All other declarations are fresh. A CREATE/MERGE edge references already-bound
  node variables; it never creates endpoints implicitly. New relationship
  variables are mandatory in writes; direction is outgoing and type singular.
- Write MATCH is node-only or fixed single-hop, never variable-length.
  AS OF is read-only. WHERE precedes mutations and cannot refer to future bindings.
  SET/REMOVE target nodes or fixed-hop edges; label changes target nodes only.
  DELETE targets nodes/edges only; DETACH DELETE accepts nodes only.
- A property or label cannot occur twice in one map/assignment/removal list.
  Duplicate RETURN column names and unresolved variables/aliases are errors.
  An unaliased item names its column by `ident` or `ident.name`; ORDER BY can
  reference only a projected scalar column with a bare/quoted identifier name,
  so a projected field needs an alias to be sorted.
- RETURN follows every read and is optional on writes. SKIP/LIMIT apply only
  to returned rows, never to which mutations run. No RETURN `*` or DISTINCT.

Reserved set, for bare variables and aliases only (ASCII case-insensitive),
versioned with the dialect: MATCH, WHERE, RETURN, ORDER, BY, SKIP,
LIMIT, AS, OF, KEY, ID, CREATE, MERGE, SET, REMOVE, DELETE, DETACH, AND, OR, NOT,
IS, NULL, ASC, DESC, OPTIONAL, WITH, UNWIND, CALL, YIELD, UNION, ALL, DISTINCT,
FOREACH, LOAD, CSV, USE, INSERT, UPDATE, DROP, ALTER, INDEX, CONSTRAINT, EXPLAIN,
PROFILE, SHORTEST, PATH, SEARCH, NEAR, IN, TRUE, FALSE, EXISTS.
Names and `$` parameters are not reserved. No other bare word becomes reserved
without a dialect-version decision.

| Rejection | Exact result |
|---|---|
| CALL/YIELD procedures; function/UDF/aggregate invocation, including EXISTS/COUNT; subqueries (`CALL {}`, `EXISTS {}`, `COUNT {}`) | `GraphQueryError.kind = Unsupported` at introducing token |
| OPTIONAL MATCH, WITH, UNWIND, UNION, FOREACH, LOAD CSV, USE; DDL; vector SEARCH/NEAR; INSERT/UPDATE; EXPLAIN/PROFILE; SHORTEST PATH | `Unsupported` at clause keyword; use dedicated typed APIs where available |
| Any other excluded clause, multi-pattern/chain, DISTINCT, `IN`, `!=`, arithmetic, lists, comments, `;`, literals, any hop form other than `*1..N`, AS OF writes or variable-length write MATCH | `Unsupported` at first unsupported token |
| Malformed supported syntax, duplicate declarations/columns/fields, unknown variable/alias, invalid target kind or reserved bare variable/alias | `Syntax` at offending token; EOF position for missing token |
| Missing/unused/wrong-type parameter; duplicate parameter-map entry before map construction | `Parameter`; first reference, EOF for unused, envelope position for duplicate |
| Text/token/depth/condition/hop limit exceeded | `Limit`; N > 4 is Limit, N = 0 or a leading zero is Syntax; N above the caller's `max_hops` is `Error::InvalidQuery("subset hop bound exceeds max_hops")` (section 6) |

Examples (parameter values supplied separately):

```text
MATCH (n:Person) WHERE n.age >= $age OR n.age IS NULL
RETURN n.name AS name ORDER BY name ASC SKIP $skip LIMIT $limit
MATCH p = (a KEY $start)-[:KNOWS*1..3]->(b:Person) AS OF $t RETURN p
MERGE (a:Person KEY $a) MERGE (b:Person KEY $b)
MERGE (a)-[r:KNOWS KEY $r]->(b) SET r.since = $since RETURN r
MATCH (n KEY $key) REMOVE n.nickname SET n:Archived DETACH DELETE n
```

## 4. Model, expressions and deterministic results

**Identity.** A statement selects one graph outside the text. Node and edge keys
occupy separate graph-wide namespaces, not per-label/type namespaces. New nodes
require explicit keys; new edges may be keyless and parallel. Internal IDs are
never user-assigned. KEY requires a nonempty string, ID a nonnegative i64
parameter (this subset cannot address IDs above i64::MAX). Whole entity results
retain full u64 IDs/revisions; serving must encode them losslessly.
Existing identity/creation behavior is evidenced by
`src/property_graph/engine/model.rs:14-30,76-98` and
`src/property_graph/mod.rs:227-270,332-345`.
`n.key`, `n.id`, `n.revision` are ordinary property names (any word is a valid
`name`), not pseudo-fields; identity/revision metadata is obtained from `RETURN n`.

**Labels/types/properties.** Match all listed labels and any listed type; no
schema declaration is required to name a label/type. No case folding or Unicode
normalization. Keys are 1..=256 UTF-8 bytes; at most 8 labels, each matching
`[A-Za-z0-9_]{1,63}`; one relationship type with the same rule.
Properties have at most 64 fields, 1..=256 bytes per field name, 4,096 bytes per
string and 65,536 encoded bytes per entity, including encoding overhead.
These are existing `MAX_KEY_BYTES`, `MAX_LABELS`, `MAX_LABEL_BYTES`,
`MAX_RELATIONSHIP_TYPE_BYTES`, `MAX_PROPERTY_FIELDS`, `MAX_FIELD_NAME_BYTES`,
`MAX_STRING_VALUE_BYTES`, `MAX_PROPERTY_BYTES`
(`src/property_graph/engine/limits.rs:15-30`).
Values are String, Integer(i64), finite Float(f64), Boolean or GeoPoint; geo
requires finite latitude [-90,90] and longitude [-180,180]
(`src/property_graph/engine/model.rs:22-30`;
`src/property_graph/engine/properties.rs:33-79`). No stored null, list, map,
duration or datetime type. Parameters use the same tagged values; no JSON-driven
integer-to-float coercion.

**Null and comparisons.** Missing properties evaluate to expression null,
not a persistable value. `IS NULL`/`IS NOT NULL` always yield booleans.
Other comparisons involving null yield unknown. NOT unknown is unknown;
AND is false if either operand is false, true only if both are true, otherwise
unknown; OR is true if either is true, false only if both are false, otherwise
unknown. Evaluate both operands in textual order; WHERE retains only true.
Map predicates mean conjunctions of field equality to their parameters.
Equality is type-strict, including Integer(1) != Float(1.0); heterogeneous
nonnull equality is false and `<>` its inverse. Float signed zeros compare equal.
Geo equality compares both coordinates numerically.
Relational operators accept same-type integers, floats or strings; other
nonnull pairs fail `Error::InvalidQuery("subset comparison type mismatch")`.
Integers compare without float conversion; strings compare UTF-8 bytes,
case-sensitively without locale collation. No arithmetic or implicit casts.
Current typed equality and numeric range rules are narrower and must not change
(`src/property_graph/engine/filter.rs:1-8,36-42,56-74`); string ranges and 3VL
therefore stay in the new evaluator rather than unsafe filter pushdown.

**Rows and paths.** Node scans consider ascending node IDs. For relationship
patterns enumerate every edge-simple trail of allowed length; a physical edge
may not repeat in one path, nodes may repeat. Keep parallel edges distinct.
Both-direction traversal emits a self-loop once per expansion and can traverse
other edges either way. Endpoint predicates apply only at the two pattern
positions; edge predicates apply at every hop. Keep separate rows for different
paths to the same endpoint. Canonical row identity is the tuple of bound node
IDs, edge IDs and path `(length, edge-ID sequence, node-ID sequence)`, ordered by
variable name in UTF-8 order. Without ORDER BY use ascending canonical identity.
This intentionally differs from legacy text's once-per-node, minimum-depth
traversal (`docs/GRAPH-QUERY.md:138-142`).

Evaluate RETURN, then ORDER BY, then SKIP, then LIMIT. Scalars sort by the fixed
rank Boolean < Integer < Float < String < GeoPoint < null; within type use
false < true, numeric order, bytewise strings, or latitude then longitude.
DESC reverses that key, including null placement; canonical row identity always
breaks ties ascending. No entity/path ordering. SKIP/LIMIT require nonnegative
i64 parameters; LIMIT 0 is valid. Omission returns all rows within caps.
No implicit pagination or partial sorted prefix.

## 5. Writes, schema, time and atomicity

Capture the exclusive write guard before MATCH and hold it until append ends
(hold bound: section 6). Materialize its complete ordered binding rows once;
mutations never rerun MATCH. Without MATCH start with one
empty row; an empty MATCH applies no mutations. Execute clauses in textual order,
each over canonical row order, seeing earlier tentative changes. New bindings
are row-local. A patch/delete clause acts once per distinct entity, not once per
duplicate binding; CREATE acts per row. MERGE checks tentative state each time.
Reading a deleted binding later, including RETURN, fails
`Error::InvalidQuery("subset binding was deleted")`; never return stale views.

**CREATE/MERGE.** CREATE node uses `CreateNode`; a live key fails `GraphKeyExists`
(`src/property_graph/mod.rs:228-233`).
CREATE edge preserves the typed keyed-create rule: identical endpoints/type and
bit-identical properties return the existing edge unchanged, otherwise conflict;
keyless creates always allocate. This is an explicit dialect difference, not
openCypher CREATE conformance (`src/property_graph/mod.rs:253-264`).
MERGE is match-or-create on an explicit key, never on a property scan or endpoint
triple. An existing node must contain all requested labels and equal all supplied
properties; extra labels/properties remain. An existing edge must have the stated
endpoints/type and equal all supplied properties; extras remain.
An incompatible occupied key, node or edge, fails `Error::GraphKeyConflict`, never
creates a second entity or updates it (rustdoc broadening: section 7). Missing keys
create the requested exact content.
No ON CREATE/ON MATCH action or automatic retry.

**SET/REMOVE.** SET changes only named properties or adds labels; REMOVE drops
only named properties or labels. Removal of an absent member is a no-op; no null
assignment or map replacement. Keys, IDs, endpoints and relationship types are
immutable. Patches retain expiry. Each clause/entity patch is one operation;
do not combine patches across clauses because constraints observe operation order.
The existing patch/expiry API is `src/property_graph/mod.rs:210-218,242-246,265-270,275-292`.

**Deletion.** DELETE edge removes that entire relationship. DELETE node fails
`GraphNodeHasEdges` if any retained incident edge remains, including self-loops
or temporal edges not valid at T. In a DELETE list, delete edges in ascending ID
before nodes in ascending ID. DETACH DELETE enumerates incident edges retained
after TTL cleanup, irrespective of validity, lazily
(`src/property_graph/engine/state.rs:315-339`), charging each to the work meter and
failing `BatchTooLarge` as soon as staged operations would exceed 4,096, so no
unbounded set is ever collected; it then deduplicates/sorts them, emits explicit
DeleteEdge operations and deletes the sorted nodes. It is not a special unmetered
cascade: all explicit deletions count toward 4,096. Node removal drops attachments,
not vector records (`src/property_graph/mod.rs:247-252`;
`src/property_graph/engine/error.rs:48-52`). No vector scope is granted: deleting
an attached node still needs the covering write grant the batch route requires
(`src/server/graph.rs:733-737,761-762`).

**Schema.** Use the same typed operations and planner validation, including
required/type/unique properties, endpoint labels and UniqueTriple. MERGE on a
different key does not bypass a unique property or UniqueTriple failure.
Changing/removing a required field or endpoint label can fail the entire batch.
Preserve existing per-operation constraint validation; do not allow an invalid
intermediate state just because the final state might conform. No implicit
schema/index creation, relaxation or `EnsureEdge` fallback. Existing rules:
`docs/GRAPH.md:117-120`; `src/property_graph/mod.rs:311-326`;
per-entry enforcement is `src/property_graph/engine/state.rs:930-948`.

**Time.** Follow [Gap 4](gap4-temporal-graph.md) sections 1-4 and
[graph expiry](../../GRAPH.md#expiry-ttl) (`docs/GRAPH.md:64-94`).
Capture epoch TTL clock C once; AS OF binds signed i64 milliseconds T.
On a temporal-capable view, omitted T is checked conversion of C to i64.
Legacy edges are all-time; a temporal edge contributes only its unique segment
containing T in `[from,to)`. Nodes remain current retained anchors; this is not
a historical snapshot of node properties or attachments. Every lookup/expansion
also requires C < expiry for the edge and both endpoints. Missing/expired MATCH
anchors yield no rows; retained history never resurrects purged entities.
Report `as_of_ms` (null on legacy ordinary reads), `ttl_clock_ms` and
`view_sequence`. Explicit AS OF requires the temporal capability and its approved
activation; otherwise return `Error::UnsupportedFeature(64)`, never downgrade.
Current private gate/default conversion: `src/property_graph/temporal/read.rs:69-107`.

Write MATCH uses current validity T=C on temporal-capable files, with checked
conversion, and current TTL visibility. It cannot change temporal segments or
patch/merge an existing temporal series: reject with
`Error::InvalidQuery("subset temporal mutation unsupported")` even if MERGE
would be a no-op. Explicit DELETE/DETACH DELETE may erase the whole series.
CREATE always creates legacy all-time edges; it never promotes them.
TTL setters remain outside this language.

**Commit boundary.** Capture C using the database's nondecreasing graph clock,
not arbitrary query parameters. The current batch samples that clock inside
`commit_planned` (`src/lib.rs:1796-1804`;
`src/property_graph/mod.rs:914-931`). The public `write_batch` takes pre-built
operations (`src/property_graph/mod.rs:397-403`) and cannot express MERGE:
`CreateNode` fails on a live key (`src/property_graph/engine/mutate.rs:370-371`) and
`UpsertNode` replaces all labels and properties (`mutate.rs:382-402`;
`mod.rs:234-235`). The executor therefore needs a crate-private entry that runs
MATCH and decides MERGE inside the planner, then appends through the existing
`commit_graph` (`mod.rs:721-747`): one commit frame (none when nothing changes),
one captured C threaded through MATCH, planning and commit, no second append path.
Keep the public batch signature unchanged.
Stage/validate the complete operations and RETURN result before append.
The first patch/delete of an existing entity uses its captured revision;
subsequent operations on that entity use `Expected::Any` only under the retained
exclusive guard, because earlier operations can change its tentative revision.
New nodes use CreateNode's absent-key check; edges use the existing create
checks. All decisions and the final batch run under that same guard. Do not
retry on conflict.

A successful mutating statement appends one frame; an empty or reclaim-only batch
appends none and returns revision null (`src/property_graph/mod.rs:893-898,1017-1028`).
TTL reclaims are computed entries, not one user operation per expired entity,
but their bytes still fit the body cap (`docs/GRAPH.md:78-87`;
`src/property_graph/mod.rs:601-628`).
I/O is not a promise of definite rollback: keep the existing
`CommitIndeterminate`, `Poisoned` and durability behavior
(`src/lib.rs:338-345,563-580`); a failed append can be indeterminate for the
whole frame, never a successful prefix of operations. Replay-safe on retry: keyed
MERGE, keyed edge CREATE (an identical edge returns `created=false`,
`src/property_graph/mod.rs:253-257`) and MATCH-driven DELETE/REMOVE. Node CREATE
replay fails `GraphKeyExists` and keyless edge CREATE duplicates, so reopen and
inspect before replaying either; no exactly-once claim.

## 6. Limits and accounting

Reuse `GraphLimits::DEFAULT`/`CEILING` exactly
(`src/property_graph/engine/limits.rs:70-108`):

| Limit | Default | Ceiling |
|---|---:|---:|
| max_hops | 2 | 4 |
| max_visited_nodes, distinct including starts | 1,000 | 10,000 |
| max_edges_examined, scans and repeated examinations | 10,000 | 100,000 |
| max_edges_per_node, per expansion | 1,000 | 10,000 |
| deadline_ms | 250 | 5,000 |
| max_output_nodes / edges / paths | 100 / 200 / 20 | 1,000 / 2,000 / 100 |

Validate caller limits before execution, never clamp: first any value above its
ceiling, then zero work budgets, as the typed reads do. Zero hops/output caps are
legal; zero work budgets are not
(`src/property_graph/engine/limits.rs:126-158`). Any relationship needs at least
one permitted hop: a hop bound N (a plain single hop is 1) above the caller's
`max_hops` fails `Error::InvalidQuery("subset hop bound exceeds max_hops")`;
`GraphLimitExceedsCeiling` is reserved for caller limits above the engine ceiling
(`src/lib.rs:451`). The caller's limit is never raised or clamped.

Additional bounds live in a separate proposed `SubsetLimits` type (name and home:
owner decision, section 11). `GraphLimits` is public and closed, without
`#[non_exhaustive]` (`src/property_graph/engine/limits.rs:69-85`; re-exported at
`src/property_graph/read.rs:35`), and the wire `limits` object is closed, so adding
fields to it would be a semver and wire break. `SubsetLimits` is independent of the
Gap 5 caps `max_vertex_operations` and `max_scratch_bytes` (`docs/GRAPH.md:227-228`).
Statement, predicate, parameter and write rows are fixed; the other rows are
caller-configurable, with the default and ceiling shown:

| Bound | Contract |
|---|---|
| Statement | 16,384 UTF-8 bytes, 1,024 tokens, one statement; reuse `MAX_QUERY_BYTES` / `MAX_QUERY_TOKENS` (`src/property_graph/engine/language.rs:26-29`) |
| Predicate | 64 atomic predicates including inline maps (`MAX_CONDITIONS`, `src/property_graph/engine/limits.rs:36-37`); at most 16 nested parentheses |
| Parameters | At most 64 distinct names; ASCII bare names <=256 bytes; total metadata encoding <=65,536 bytes, including names/tags/count; each value obeys section 4; duplicates rejected before deserialization discards them |
| Binding rows and trail frontier entries | Each 1,000 default / 10,000 ceiling, checked before insertion, including rows later filtered/skipped |
| Returned rows | 100 default / 1,000 ceiling, independent of entity caps, including scalar-only rows |
| Evaluator work units | 100,000 default / 1,000,000 ceiling; one per predicate, row projection, sort comparison, frontier operation, key resolution or operation staged |
| Scratch | 32 MiB default / 128 MiB ceiling, at least twice `MAX_BATCH_BYTES` (`src/property_graph/engine/limits.rs:35`) so scratch cannot undercut the batch cap; charge owned decoded values, bindings, frontier, sorting, staged operations and output buffers before allocation |
| Writes | `MAX_BATCH_ENTRIES` = 4,096 user operations; `MAX_BATCH_BYTES` = 16,777,216 encoded body bytes, including TTL reclaims (`src/property_graph/engine/limits.rs:31-35`; `src/property_graph/mod.rs:601-628`) |
| Write deadline | `write_deadline_ms` uint64, 100 default / 1,000 ceiling for statements with a write clause, in place of `deadline_ms` (proposed per [wire conventions, decision 15](serving-wire-conventions.md#6-owner-decisions-23); owner decision, section 11). Such a statement holds the exclusive writer guard from before MATCH until append ends; the batch route holds it only for authorization, planning and append (`src/server/graph.rs:725-729`; `src/server.rs:340`), and a writer blocks readers (`src/server.rs:355-358`). The hold is bounded by this deadline plus append and sync time and is recorded in the existing `write_guard` stats (`src/server/graph.rs:241-245,259-267,293-310`) |

Configurable work, frontier, binding, scratch and write-deadline limits must be at
least 1; the returned-row cap may be 0. Above-ceiling requests use
`GraphLimitExceedsCeiling`; invalid zero requests use
`Error::InvalidQuery("subset work limits must be positive")`.
Fixed parameter count/name/aggregate-byte cap failures are query `Limit` errors
at EOF (parameters are outside the text); invalid individual typed values use
`InvalidMetadata` before graph access. Pagination and AS OF type failures remain
query `Parameter` errors at their references.

Charge every node scanned and adjacency examined, including filtered, expired,
invalid-at-T, duplicate and DETACH-discovery work; repeated examinations count
again. Per-expansion limits apply even when revisiting a node along a new path.
Track distinct visited nodes separately from path/frontier counts.
Output entity caps count distinct node/edge IDs in the final rows, including
entities nested in paths; path cap counts path occurrences. LIMIT/SKIP do not
raise these caps, bound scans, truncate mutation targets or excuse unfinished
sorting. Scratch and work budgets span every phase and are not reset per clause.

Check the monotonic deadline (uint64 `write_deadline_ms` for a statement with a write
clause) and cancellation before work, every at most `CHECK_INTERVAL`
(64) combined work units, during sorting/staging and immediately before commit.
Do not use the epoch TTL clock as a deadline timer. Existing meter behavior is
`src/property_graph/engine/limits.rs:38-41`;
`src/property_graph/engine/meter.rs:12-19,48-107`; extend shared accounting for
the new phases rather than resetting it at typed-API boundaries.
After append starts, cancellation cannot undo a commit; finish or report the
actual indeterminate error, never a false cancelled/rolled-back result.

Unlike legacy typed reads, which truncate (`docs/GRAPH.md:112-113`), this dialect
fails with the specific section 7 error when any budget, deadline or cancellation
is exhausted (requirement 7).

## 7. Errors and transport mapping

Reuse the existing `GraphQueryError` shape/kinds (engine `LanguageError`) and
`GraphTextError::{Query, Read}` wrapper
(`src/property_graph/engine/language.rs:31-63`;
`src/property_graph/read.rs:381-419`); `Read(Error)` also carries write failures
of the new entry point, despite its legacy name. Do not change legacy behavior.
No new public `Error` variant is requested. Static subset execution reasons use
existing `Error::InvalidQuery(&'static str)`; do not stuff dynamic diagnostics
into it (`src/lib.rs:366`).

Two rustdocs become inaccurate for the new dialect and are broadened, documentation
only, with no new variant or behavior change: `GraphTextError::Read`, "The
equivalent typed read failed" (`src/property_graph/read.rs:381-388`), and
`Error::GraphKeyConflict`, today edge-only (`src/lib.rs:429-431`), which also
covers a MERGE node whose labels or properties differ from the pattern.

| Condition | Engine / public result |
|---|---|
| Grammar, exclusion, parameter or parser cap | `GraphQueryError` with Syntax / Unsupported / Parameter / Limit as section 3 |
| Caller limits above the engine ceiling | `GraphError::LimitExceedsCeiling` / `Error::GraphLimitExceedsCeiling` |
| Hop bound above the caller's `max_hops` (section 6) | `Error::InvalidQuery("subset hop bound exceeds max_hops")` |
| Invalid keys/labels/types or target structure | Existing validators / `Error::InvalidQuery` |
| Invalid/nonfinite/oversized property value | `GraphError::InvalidProperties` / `Error::InvalidMetadata` |
| Occupied node CREATE key; conflicting edge CREATE or MERGE key | `NodeKeyExists` / `GraphKeyExists`; `EdgeKeyConflict` / `GraphKeyConflict`; MERGE node mismatch uses `GraphKeyConflict` directly |
| Node still has edges; missing node/edge while applying | `NodeHasEdges`, `UnknownNodeKey`, `UnknownEdgeKey` / `GraphNodeHasEdges`, `GraphNodeNotFound`, `GraphEdgeNotFound` |
| Revision or declared constraint failure | `PreconditionFailed`, `SchemaViolation` / `RevisionMismatch`, `GraphSchemaViolation` |
| More than 4,096 operations or 16 MiB encoded body | `BatchSize` / `BatchTooLarge`, before append |
| AS OF without the temporal capability | `Error::UnsupportedFeature(64)` |
| Runtime budget, comparison-type, deleted-binding or temporal-mutation failure | `Error::InvalidQuery` with exactly one fixed reason below |
| Read-only file, corruption, I/O, poisoned or indeterminate commit | Preserve original `Error`, never relabel as syntax or successful empty result |

Engine-to-public mapping: `src/property_graph/convert.rs:133-204`; public variants
`src/lib.rs:314,325,338-344,427-431`. A MATCH that finds no row is not an error.
The `subset ...` reasons below are stable once shipped: the reason is the only
discriminator inside `Error::InvalidQuery` and appears verbatim in its message
(`src/lib.rs:621`), so tests pin each string and a change is a compatibility
change. Runtime reasons are exactly `subset visited-node budget exhausted`,
`subset edge budget exhausted`, `subset degree budget exhausted`,
`subset deadline exceeded`, `subset cancelled`, `subset frontier budget exhausted`,
`subset binding-row budget exhausted`, `subset result-row budget exhausted`,
`subset output budget exhausted`, `subset work budget exhausted` and
`subset scratch budget exhausted`. The other fixed reasons are
`subset hop bound exceeds max_hops` and `subset work limits must be positive`
(section 6), `subset comparison type mismatch` (section 4),
`subset binding was deleted` and `subset temporal mutation unsupported`
(section 5). Diagnostic positions must be UTF-8 byte offsets, bounded by text
length; messages must not echo keys, values or entire source text.

**Existing mapping, verbatim (no wire change).** The new entry point returns the
same `Error` values, so existing code maps them as today:

| Failure | REST | gRPC (`status_code`, if reused) | CLI/MCP code |
|---|---|---|---|
| `InvalidQuery`, `BatchTooLarge`, `InvalidMetadata` | 422 `invalid_input` | INVALID_ARGUMENT | `invalid_input` |
| `RevisionMismatch` | 412 `precondition_failed` | ABORTED | `precondition_failed` |
| `GraphNotFound`, `GraphKeyExists`, `GraphKeyConflict`, `GraphNodeNotFound`, `GraphEdgeNotFound`, `GraphNodeHasEdges`, `GraphSchemaViolation` | 409, snake_case variant name (`graph_key_conflict`) | FAILED_PRECONDITION | same name |
| `GraphLimitExceedsCeiling` | 422 `graph_limit_exceeds_ceiling` | INVALID_ARGUMENT | same |
| `GraphTextError::Query`, four kinds | 422 `graph_query_syntax/unsupported/parameter/limit` | INVALID_ARGUMENT | same |
| `UnsupportedFeature` | no arm: 500 `internal_error` | INTERNAL | `unsupported_feature` |
| `CommitIndeterminate` | 503 `commit_indeterminate` | UNKNOWN | `commit_indeterminate` |
| `Poisoned` | 503 `poisoned` | INTERNAL | `poisoned` |
| Missing graph scope | 403 `forbidden_scope` | PERMISSION_DENIED | proxied REST code |

Sources: REST `src/server.rs:1472-1480,1523-1534,1577-1624` (500 via `internal`,
`src/server.rs:1425-1436`) and `src/server/graph.rs:22-24,687-699`; gRPC
`src/server/grpc.rs:181-197`, which keys on the REST status and code (no graph RPC
exists today: `docs/GRAPH.md:372`, `proto/vlite/v1/vlite.proto:16-60`); CLI/MCP
`src/bin/tool_support/graph_json.rs:26-72,1612-1627`; MCP failures are
`isError: true` with text `"<code>: <message>"` (`src/bin/vlite-mcp.rs:679`;
`src/bin/tool_support/graph_mcp.rs:12`), and the proxy passes the REST code through
(`src/bin/tool_support/graph_proxy.rs:225-246`). REST text errors carry the byte
position in the message (`src/property_graph/engine/language.rs:65-80`).

**Proposed changes (not implemented; owner approval, section 11).** None is needed
for the dialect to work; without approval the table above applies unchanged.

1. Budget failures keep 422 `invalid_input` plus the fixed reason through the
   existing `InvalidQuery` mapping. Propose 504 `deadline_exceeded` for the
   precommit engine deadline stop reason, produced by a handler-built `ApiError`
   and mapped to gRPC DEADLINE_EXCEEDED; cancellation has no wire code. Follow
   [wire conventions sections 2.2 and 3](serving-wire-conventions.md#22-one-reachable-family-oversize-deadline-cancel-busy),
   including decision 4; fixed reasons remain the budget discriminator.
2. Propose scoped 409 `unsupported_feature` per
   [wire conventions, decision 7](serving-wire-conventions.md#6-owner-decisions-23):
   temporal and explicit-precision operation handlers build an `ApiError` from
   the existing `UnsupportedFeature` variant; no global `From<Error>` arm or
   legacy-route change. The proposed AS OF operation uses the temporal scope.
3. **Withdrawn:** a distinct batch-size status/code duplicates `BatchTooLarge`'s
   existing 422 `invalid_input`; see [wire conventions section 2.2](serving-wire-conventions.md#22-one-reachable-family-oversize-deadline-cancel-busy).
4. **Withdrawn:** changing missing graph/entity statuses would alter existing
   mappings; a missing graph stays 409 `graph_not_found`, per
   [wire conventions section 2.1](serving-wire-conventions.md#21-registry).
5. **Withdrawn:** separate budget and cancellation codes and a second deadline
   mapping conflict with the shared family; proposal 1 follows
   [wire conventions section 2.2](serving-wire-conventions.md#22-one-reachable-family-oversize-deadline-cancel-busy).

Exact envelopes, numeric/geo encoding and any code or status change are settled
before `gap6-serving`; the gRPC column describes a transport that does not exist
today.

## 8. Security and public-surface boundary

Select/qualify the graph through the authenticated request context, never text,
parameters or a user-supplied tenant ID. Use `graph_read` for reads and require
both `graph_read` and `graph_write` for any syntactically mutating statement,
even with zero matched rows, MERGE no-op or explain-only execution. Requiring both
is a deliberate owner decision (section 11): the statement reads graph data through
MATCH and RETURN, while the batch route needs only `graph_write`
(`src/server/graph.rs:14-24`), so `graph_write` alone must not become a read path.
Check bindings before existence lookup. The server must authorize mutating
ASTs separately, not send writes through the existing read-only `/query` route.

Preserve tenant-local qualification `g -> t<tenant_id>__g`, local-name maximum
28 bytes and tenant-local graph allowlists; reject escape attempts rather than
treating a stored-name prefix as authority. `collections:["*"]` never grants
graph scope. Existing scope rules are `docs/GRAPH.md:277-296` and
`src/server/graph.rs:14-43`. Embedded file access, CLI and MCP-on-file have no
server tenant enforcement (`docs/GRAPH.md:295-296`); document that trust boundary.
No query clause can change bindings, tenant, schema, collection or feature gates.

Parameter-only values prevent value injection; a value containing backticks,
keywords, braces or semicolons remains data. Untrusted identifiers must not be
interpolated: choose a preapproved query template instead. Reject duplicate
envelope keys, unknown fields, wrong tagged values and oversized bodies before
unbounded allocation. Gate complete ASTs, not keyword regexes or RETURN presence.
Apply per-principal admission/concurrency quotas in addition to statement bounds.
No procedure loading, filesystem/network access or dynamic execution.
Audit principal, tenant/graph binding, dialect, read/write class, normalized AST
fingerprint, outcome category, counts, budgets and revision; redact query source,
parameters, property values, keys, entity payloads and credentials in logs/traces.
Parse errors record kind/offset only. Authorization failures must not disclose
graph existence or cross-tenant IDs.

REST, gRPC, MCP, CLI, OpenAPI and proto are **public API**. Any selector, route,
tool, RPC, `src/openapi.json`, `src/openapi_graph.json` or proto edit is a wire
change, and the owner must approve the exact wire before `gap6-serving`: the dialect
selector (an absent selector is the legacy language; requirement 2), the separate
write-capable route/tool/RPC, request/result shapes, typed values, errors,
authorization, caps and retry semantics. The approved
[serving contract](serving-contract.md) for other gaps is not Gap 6 approval.

## 9. Independent oracle and qualification

Build a Python reference interpreter independently of Rust lexer, planner,
filters and traversal helpers. Input: plain graph entities, declared schema,
fixed C/T/sequence, typed parameters and limits. Implement the published EBNF,
3VL, edge-simple trail enumeration, canonical ordering and a copy-on-write
mutation transaction directly; on failure compare the entire original graph.
Use Python integers with explicit i64 bounds and binary64 floats, not Python's
bool-as-int equality. The oracle must model typed equality, encoding lengths,
constraints, budgets and indeterminate outcomes without pretending to simulate
filesystem durability. Golden expected outputs are hand-reviewed, not generated
solely by Rust. Fixtures include result columns/types/rows, error kind/offset or
static reason, counters, commit count and final graph/IDs/revisions.

TCK-style scenario subset (describe compatibility, do not claim a TCK pass):

| In | Out, with explicit rejection fixtures |
|---|---|
| Node/relationship MATCH, labels/types/direction, parallel edges/self-loops, bounded trails, missing property and 3VL | OPTIONAL MATCH, multiple patterns/joins, unbounded paths, shortestPath/functions |
| Projection aliases, scalar ordering/ties/null, SKIP/LIMIT, parameter binding | DISTINCT, aggregation, WITH, UNION, UNWIND, lists/maps as values |
| Keyed MERGE, CREATE, SET/REMOVE, DELETE/DETACH DELETE, constraint rollback | Property-based MERGE, ON CREATE/ON MATCH, DDL, procedures/UDFs, subqueries |
| Parameterized equivalents of supported openCypher-style scenarios | Full openCypher literal-based scenarios and CREATE/MERGE identity semantics that differ here |

Keep scenario origins and adaptations explicit: original local fixtures are
preferred; import external TCK material only with license/provenance retained.
Add dialect-specific scenarios for KEY/ID, no literals, typed numeric inequality,
expired key reuse, closed temporal edges, AS OF/C independence, gate refusal,
attachments surviving as records, and explicit frame/batch limits.

Mutation campaign must kill: reverse direction; drop a label/type predicate;
allow one repeated edge; double-count a Both self-loop; deduplicate endpoint rows;
coerce i64 to f64; equate bool/integer; treat unknown as false under NOT;
reverse null/tie ordering; apply LIMIT before sorting or mutation; reinterpret
parameter text; skip tenant qualification or accept collection wildcard;
MERGE by triple; bypass unique/required/endpoint constraints; omit a DETACH edge;
off-by-one TTL/validity interval; sample C per clause; accept 4,097 operations;
omit encoding overhead; append before final validation; retain partial cleanup.
Each mutant needs a named failing scenario, not just aggregate coverage.

Fuzz UTF-8/token boundaries, quoted identifiers, all rejection introducers,
arbitrary parameter bytes, duplicate maps and parse/format roundtrips.
Exercise cap-1/cap/cap+1 for bytes, tokens, depth, predicates, parameters,
properties, rows/frontier, work, scratch, output, hops, operation count and body
bytes. Fake-clock tests stop before work, at unit 64, during sorting and just
before append. Verify cancellation after append does not claim rollback.
Inject final-op schema/revision failures and write/sync faults; reopen to verify
whole-frame recovery, not partial state. No performance/size measurements.

Differentials: compare node scans/gets and single-hop reads directly with typed
`query`/`neighbors` after normalizing projections; compare fixed-clock AS OF
reads with Gap 4's gated APIs. For multi-hop cases compare endpoint sets and
minimum depths with `traverse` only when filters/visibility semantics coincide,
not raw rows: the legacy BFS deduplicates endpoints. Independently validate every
returned path and compare full rows to Python. Compare writes to equivalent
explicit `GraphOp` batches, including metadata, revisions, TTL preservation,
constraints and reopen state. These tests complement, not replace, the oracle.

## 10. Delivery and independent acceptance

All paths below are proposed ownership for future slices, not files created by
this design. Each slice stops at its boundary and gets independent acceptance
before its dependent starts. Existing tests remain intact.
For `gap6-serving`, `/v1/graphs` paths belong only in `src/openapi_graph.json`;
`src/openapi.json` holds components only for those graph operations, per
[wire conventions section 5.2](serving-wire-conventions.md#52-per-file-owner-and-sequence).

| Slice / dependencies | Files owned | Required tests and independent-acceptance checklist |
|---|---|---|
| `gap6-parser-planner` / owner dialect/grammar approval | New `src/property_graph/engine/subset.rs` and `engine/subset/{lexer,parser,plan}.rs`; module registration in `src/property_graph/engine/mod.rs`; new `tests/graph_subset_parser.rs` | Golden accepted/rejected ASTs, all error offsets, reserved names, parameter validation, parser caps/fuzz, no graph access. Reviewer checks grammar-to-AST completeness, deterministic diagnostics and unchanged legacy parser. |
| `gap6-read-executor` / parser-planner; approved Gap 4 capability contract for AS OF | New `engine/subset/{eval,read,meter}.rs`, `src/property_graph/subset.rs` (including the proposed `SubsetLimits` and its validation, section 6); registration in `src/property_graph/mod.rs`; narrow reuse hooks in `read.rs` and `temporal/read.rs`; new `tests/graph_subset_read.rs` | Python read oracle, typed differentials, 3VL/numerics, every path/direction, sort/ties, T/C boundaries, `SubsetLimits` validation, all budget stops. Reviewer checks full-row semantics, no unsafe pushdown, one view/clocks, and no mutations/public serving. |
| `gap6-write-executor` / read-executor | New `engine/subset/write.rs`; `src/property_graph/subset.rs`; `src/property_graph/mod.rs` for the crate-private statement entry sharing `commit_graph` (section 5); private clock/planner integration in `src/lib.rs`; documentation-only rustdoc broadening of `Error::GraphKeyConflict` (`src/lib.rs`) and `GraphTextError::Read` (`src/property_graph/read.rs`), section 7; new `tests/graph_subset_write.rs` | Atomic CREATE/MERGE/patch/delete/DETACH, shared binding dedup, schema/revision failures, closed temporal incident edges, TTL race boundaries, 4,096/4,097 ops, exact body cap, commit-frame count (one, or none when nothing changes), writer-guard hold-time stats, rollback/reopen/fault tests. Reviewer verifies one commit frame with no extra append path, captured C threaded through, complete precommit staging, no partial commit path and the guard hold bound (section 6). |
| `gap6-serving` / write-executor plus exact owner-approved public wire | `src/server/graph.rs`, `src/bin/tool_support/{graph_json,graph_mcp,graph_proxy}.rs`, `src/bin/vlite-graph-cli.rs`, `src/openapi.json`, `src/openapi_graph.json` (`serving-contract.md:196`); `src/server/grpc.rs` and `proto/vlite/v1/vlite.proto` only if the Gap 12 Graph service is approved (`serving-contract.md:131,202-205`); `docs/GRAPH-QUERY.md`, `docs/GRAPH.md`, `docs/MCP.md`, `docs/TOOLS.md`; corresponding REST/gRPC/MCP/CLI tests | Exact schema snapshots and wire parity, lossless typed values/IDs, read/write separation, refusal of write statements for read-only and `graph_write`-only credentials, tenant escape/wildcard denial, duplicate/unknown fields, absent selector still selecting the legacy language, redaction and indeterminate no-retry behavior. Reviewer checks against approved wire inventory; no silent upgrade of legacy `/query`. |
| `gap6-qualification` / serving (or explicit embedded-only release decision) | New `tests/graph_subset_oracle.py`, `tests/fixtures/graph_subset/`, `tests/graph_subset_differential.rs`, mutation/fuzz harnesses under `tests/`; this design's status and `docs/GRAPH-QUERY.md` qualification notes | Independently authored golden corpus and Python interpreter, named mutant kills, fuzz regressions, all boundary tests and restart/recovery differentials. Reviewer reproduces evidence, checks TCK in/out matrix and verifies existing typed/text behavior unchanged. No conformance claim beyond accepted scenarios. |

The independent reference/scenarios are developed alongside the first executor
slices; qualification owns their final gate, not permission to delay validation.
Dependencies make the slices serial, so shared files are never edited
concurrently: `gap6-read-executor` and `gap6-write-executor` both edit
`src/property_graph/subset.rs` and `mod.rs`, and `gap6-serving` and
`gap6-qualification` both edit `docs/GRAPH-QUERY.md`. The read hooks in `read.rs`
and `temporal/read.rs` and the OpenAPI files are shared with
[Gap 4](gap4-temporal-graph.md) (slices C reads and E serving), so coordinate with
its owner first. Do not enable temporal files or change approved activation as a
side effect.

## 11. Owner decisions needed

The owner approved all ten recommendations below individually on 2026-11-02.
This authorizes private embedded implementation under the stated dependencies,
not public API publication, new transport errors, serving activation or release.
The exact public inventory and proposed wire/error changes remain separate
approval gates; private engine completion is not public feature delivery.

| Decision | Options | Owner-approved choice / remaining gate |
|---|---|---|
| Compatibility/name | Claim openCypher subset with compatibility exceptions; or versioned named VLite dialect | Approve `vlite-cypher-1` as a separate, named, non-conformant dialect with a published supported-scenario matrix; it narrows the approved "openCypher with writes" item (`docs/projects/product-gaps/product-gaps.md:118-119`). KEY identity, parameter-only values and idempotent keyed CREATE make an unqualified openCypher claim misleading. |
| First-release REST writes | Embedded writes only; or separately authorized REST write-query endpoint alongside existing reads | Embedded first; REST writes only after the exact wire and threat/rollback tests pass. Never make existing read-only `/query` write-capable. |
| Error mapping | Existing mapping with fixed reasons only (section 7); or also the section 7 proposed changes 1-5 (new codes/statuses, new gRPC arms) | Approve the existing mapping now; take each proposed change only after wire review, with the owner choosing code and status. Any new public Error variant needs a separately enumerated approval item; none is proposed here. Wire conventions, decision 4 (owner-selected); Wire conventions, decision 7 (owner-selected). |
| Reserved words | Versioned complete reserved set; or contextual keywords that may change parsing later | Freeze section 3's set in version 1 for bare variables and aliases only; labels, types, property names and `$` parameter names stay unrestricted as in legacy (`src/property_graph/engine/language.rs:244-257,585-594`); add keywords only with explicit compatibility review. |
| Match breadth/path semantics | Single-pattern edge-simple trail rows; or legacy once-per-node BFS rows; or larger multi-pattern language | Approve single-pattern trail rows with explicit caps; retain BFS only in legacy/typed APIs. Defer joins and OPTIONAL MATCH. |
| MERGE/CREATE identity | Explicit-key identity and typed keyed-create behavior; or property-based merge and strict always-new CREATE | Approve explicit keys and documented existing edge-create idempotency; no inference from schema uniqueness or endpoint triples. |
| Time and fail-closed output | AS OF reads only and whole-statement errors on exhaustion; or temporal writes/partial results | Approve read-only AS OF, no segment syntax, and fail-closed rows/writes; existing typed truncation remains unchanged. |
| Public API inventory | Approve exact Rust/wire names/shapes before implementation (dialect selector default, new route/tool/RPC, `SubsetLimits` name and home, `src/openapi.json`, `src/openapi_graph.json` and proto edits, rustdoc broadening of section 7); or keep all new entry points private until qualification | Private embedded slices first: no new public Rust item (`SubsetLimits` included) and `GraphLimits` unchanged until approved. An absent selector stays the legacy language. Approve the exact REST/gRPC/MCP/CLI/OpenAPI/proto contract before `gap6-serving`, including whether transports may ship at different times. |
| Write-statement deadline and scratch | Same values as reads (`deadline_ms` 250/5,000; Gap 5 scratch 64/256 MiB); or `write_deadline_ms` 100/1,000 and scratch 32/128 MiB in `SubsetLimits` (section 6) | Approve the lower values: a write statement holds the exclusive writer guard from before MATCH until append ends, and the guard hold time is recorded in the existing `write_guard` stats. Proposed representation: uint64 `write_deadline_ms`, with above-ceiling values rejected as in section 6. Wire conventions, decision 15 (owner-selected). |
| Mutating-statement authorization | Require `graph_read` and `graph_write`; or `graph_write` alone | Require both: the statement reads through MATCH and RETURN, while the batch route needs only `graph_write` (`src/server/graph.rs:14-24`), so `graph_write` alone must not become a read path. |

Every implementation slice still needs independent acceptance. Public API
publication and serving require the remaining exact-surface approvals and
qualification; no openCypher-conformance or release claim is authorized.

## 12. Diagnostics clarifications (provisional, 2026-11-03)

Sections 3, 4, 6 and 7 fix which statements are accepted and which error
category and position each rejection reports. Comparing the parser/planner with
an independent reading of those sections left the cases below open. They are
settled here under the standing rule for unforeseen decisions (backward
compatible, long-term, provisional) and await owner ratification. None changes
an accepted or rejected statement, a limit, a public name, a wire shape or the
error mapping; each only fixes the kind or position reported for an already
rejected statement, or corrects the independent reading.

Principle: `Unsupported` marks a construct that is valid in openCypher, or a
combination of subset features, which this dialect leaves out; it is reported at
the token that introduces it. `Syntax` marks input that is invalid even in
openCypher, or malformed supported syntax; it is reported at the first
offending token. Where sections 3 and 7 enumerate a result, the enumeration
wins. Positions are 0-based UTF-8 byte offsets.

| Input | Result | Corrected side |
|---|---|---|
| a. A character outside the section 3 alphabet and outside a quoted name: non-ASCII whitespace (no-break space, em space, vertical tab, form feed, line and paragraph separators), non-ASCII, wide or accented letters in a word or keyword, emoji | `Syntax` at that character | reference |
| b. `#` as a comment; only `//` and `/* ... */` are comment forms | `#`: `Syntax` at the `#`; the two comment forms: `Unsupported` at the opener | reference |
| c. Empty or unterminated quoted name | `Syntax` at the opening backtick | reference |
| d. `ON CREATE` or `ON MATCH` after any mutation clause, `MERGE` included | `Unsupported` at `ON` | parser |
| e. `AS OF` in a statement with a mutation clause | `Unsupported` at `AS` | parser |
| f. Function or UDF call, plain or qualified (`f(x)`, `n.f(x)`) | `Unsupported` at the name before `(` | parser |
| g. Any hop form other than `*1..N` (`*`, `*1`, `*2..3`, `*..3`, `*1..$h`, `*1..]`) | `Unsupported` at the `*`; `*0..N` and a leading-zero bound stay `Syntax` at the bound, N above 4 stays `Limit` | reference |
| h. A relationship variable or anchor together with `*` | `Unsupported` at the `*` | reference |
| i. Bracketless relationship shorthand (`-->`, `--`, `<--`, `<-->`) | `Unsupported` at the first `-` or `<-`; any other token after that opener except `[` stays `Syntax` at that token | parser |
| j. `KEY $k` bound to an empty string | Planning accepts it; the existing key validators reject it at execution as `Error::InvalidQuery` (section 7) | reference |
| k. A parameter used in several roles that suits the first but not a later one | `Parameter` at the first reference whose role the value cannot satisfy | reference |
| l. Parameter byte cap | The 65,536-byte cap counts the metadata encoding: 4 for the count, then per parameter 2 + name length + 1 for the tag + the value (string 4 + length, integer or float 8, boolean 1, geo point 16); an unused parameter still reports at EOF | reference |
| m. `CREATE` or `MERGE` relationship written inward (`(a)<-[r:T]-(b)`) | `Syntax` at the `<-` | parser |

The parser/planner changes (d, e, f, i, m) alter only the reported kind or
position. The independent reference and its fixtures take a, b, c, g, h, j, k
and l. The differential between them is re-run on both fixed heads before either
is wired into CI. Cases the table does not list, such as MERGE `(a)` followed by
anything other than `<-`, stay open and keep the behavior of sections 3 and 7.
