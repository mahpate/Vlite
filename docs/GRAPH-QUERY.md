# Graph query text (experimental, `graph` feature)

VLite's property graph is read through the typed `GraphQuery` API
([`GraphRef::query`](../src/property_graph/read.rs)). The **graph query text** is
a narrow, read-only notation for those reads (EPIC-015 ITEM-051, REQ-070,
REQ-071). Every accepted text translates to **exactly one** typed
`GraphQuery`. It adds no execution semantics, no writes and no capability the
typed API lacks. Anything the typed API cannot express is rejected with a typed
error that names the byte offset and the supported alternative.

The syntax is VLite's own. It borrows the familiar `MATCH (a)-[:T]->(b)` pattern
shape, but it is **not** Cypher, GQL or openCypher and is not compatible with
them. Texts written for other systems are usually rejected rather than
reinterpreted.

It is separate from the vector [query language](../src/query_language.rs).
Vector queries always start with `SEARCH`, graph texts always start with `MATCH`,
and each parser rejects the other's texts. The two languages have different
entry points: `vlite::query_language::parse` for vectors, and
`vlite::property_graph::parse_graph_query` / `GraphRef::query_text` for graphs.

## Entry points

| Surface | Call |
|---------|------|
| Rust | `parse_graph_query(text, &params) -> GraphTextQuery` (parse only; no graph access) |
| Rust | `graph.query_text(text, &params, &limits)` → `GraphRead<GraphOutput>` |
| Rust | `graph.explain_text(text, &params, &limits)` → `Explanation` |
| CLI | `vlite-graph-cli --db F query '{"graph":"g","query":"...","params":{...},"limits":{...}}'` (and `--explain`) |

- **Graph.** The graph is chosen outside the text: the `GraphRef`, or the
  CLI's `"graph"` field.
- **Parameters.** `params` is a `Metadata` map (engine metadata JSON in the
  CLI) bound to `$name` values. A missing parameter, an unused parameter, or a
  parameter of the wrong type for `KEY`, `ID` or `LIMIT` is an error.
- **Limits.** The text may override two read limits: the upper bound `N` of a
  hop range `*1..N` sets `max_hops`, and a traversal `LIMIT` sets
  `max_output_nodes`. All other limits come from the `limits` argument. The
  caller's limits and the effective limits are both validated against
  `GraphLimits::CEILING`. Values above the ceiling fail and are never clamped.
- **Projection.** The `RETURN` clause sets the projection.
- **Execution.** `query_text` runs `GraphRef::query` on the translated query, so
  results, truncation, budgets and errors are exactly those of the typed read.
- **Explain.** `explain_text` returns `GraphRef::explain`'s normalized typed
  query. Use that query with the typed API to page with cursors.

## Grammar

The grammar below uses EBNF. Keywords are case-insensitive. Whitespace is free.

```ebnf
query        = "MATCH" , [ variable , "=" ] , [ "SHORTEST" , "PATH" ] ,
               node , [ relationship , node ] ,
               [ "WHERE" , condition , { "AND" , condition } ] ,
               "RETURN" , item , { "," , item } ,
               [ "LIMIT" , value ] ;
node         = "(" , [ variable ] , { ":" , name } , [ anchor ] , [ properties ] , ")" ;
relationship = ( "-" | "<-" ) , [ "[" , [ variable ] , [ types ] , [ anchor ] ,
                                  [ hops ] , [ properties ] , "]" ] , ( "->" | "-" ) ;
types        = ":" , name , { "|" , [ ":" ] , name } ;
anchor       = "KEY" , value          (* a string: the node or edge key *)
             | "ID" , value ;         (* a non-negative integer: the internal ID *)
hops         = "*" , [ "1" ] , ".." , integer ;   (* 1 <= integer <= 4 *)
properties   = "{" , [ name , ":" , value , { "," , name , ":" , value } ] , "}" ;
condition    = variable , "." , name , ( "=" | ">=" | "<=" ) , value ;
item         = variable , [ "." , name ] ;
value        = string | [ "-" ] , number | "true" | "false" | parameter ;
parameter    = "$" , identifier ;
variable     = identifier | quoted ;   (* not a keyword *)
name         = identifier | quoted ;
identifier   = ( letter | "_" ) , { letter | digit | "_" } ;   (* ASCII *)
quoted       = "`" , { any character except "`" } , "`" ;
string       = '"' , { character | '\"' | '\\' | '\n' | '\t' } , '"' ;
number       = digit , { digit } , [ "." , digit , { digit } ] , [ ( "e" | "E" ) , [ "+" | "-" ] , digit , { digit } ] ;
```

- **Direction.** `-[...]->` is outgoing (`Direction::Out`), `<-[...]-` is
  incoming (`In`), and `-[...]-` is either direction (`Both`). The bracketless
  forms `-->`, `<--` and `--` are allowed.
- **Numbers.** Integers are `i64`. A number with a fraction or exponent is a
  finite `f64`.
- **Size caps.** A text has at most 16,384 bytes and 1,024 tokens. It holds one
  statement and is parsed without recursion.

## Mapping to the typed API

The rules below decide which typed query a text becomes. Variables play the
roles *start* (first node), *relationship*, *end* (second node) and *path*
(`p = ...`).

| Text form | Typed query | Notes |
|-----------|-------------|-------|
| `MATCH (n KEY k) RETURN n` / `(n ID i)` | `GetNode(NodeSelector::Key/Id)` | No labels, properties, `WHERE` or `LIMIT`. |
| `MATCH (n[:L] [{f: v}]) [WHERE n.f op v ...] RETURN n[.f, ...] [LIMIT m]` | `FindNodes { label, conditions, limit }` | At most one label. Conditions are the inline properties followed by the `WHERE` conditions. |
| `MATCH ()-[r KEY k]-() RETURN r` / `[r ID i]` | `GetEdge(EdgeRef::Key/Id)` | The endpoints must be empty `()`. Direction is ignored. |
| `MATCH (a KEY k)-[r[:T|U] [{f: v}]]->(b) [WHERE r.f op v ...] RETURN r, b [LIMIT m]` | `Neighbors { node, direction, types, edge_conditions, limit }` | Single hop. `b` takes no labels, properties or anchor (write `*1..1` to filter it). |
| `MATCH [p =] (a KEY k)-[r[:T] *1..N [{f: v}]]->(b[:L...] [{f: v}]) [WHERE b.f/r.f op v ...] RETURN b[, r][, p] [LIMIT m]` | `Traverse { start: [a], direction, types, return_filter, edge_filter, include_paths }` | `max_hops = N`. `return_filter` gets `b`'s labels (all required) and conditions. `edge_filter` gets `r`'s conditions. `include_paths` is set when `p` is returned. `LIMIT m` sets `max_output_nodes = m`. `expand_filter` is empty. |
| `MATCH p = SHORTEST PATH (a KEY k)-[[:T] *1..N]->(b KEY k2) RETURN p` | `ShortestPath { source, destination, direction, types }` | `max_hops = N`. Requires the path variable. No `WHERE`, `LIMIT`, properties or relationship variable. |

- **Conditions.** `=` becomes `Condition::Eq`. `>=` becomes a `Condition::Range`
  with only `min`, and `<=` one with only `max`. Several conditions are
  combined with AND, exactly as typed conditions are.
- **Projection.** `RETURN` of whole variables is `PropertyProjection::All`.
  `RETURN` of only `var.field` items is `PropertyProjection::Only` of those
  fields. Mixing the two forms is unsupported. The projection applies to every
  returned node and edge view, as in the typed API.
- **Anchoring.** A relationship pattern must start from a node anchored with
  `KEY` or `ID`.

Examples:

```text
MATCH (n:Service) WHERE n.tier >= $min RETURN n.tier LIMIT 20
MATCH (a KEY "api")<-[r:CALLS]-(b) WHERE r.weight >= 0.5 RETURN r, b
MATCH p = (a KEY $k)-[:CALLS|READS*1..3]->(b:Database {region: "eu"}) RETURN b, p LIMIT 50
MATCH p = SHORTEST PATH (a KEY "api")-[*..4]-(b KEY "db") RETURN p
```

## Errors

`GraphQueryError { kind, position, message }`. `position` is a byte offset into
the text (at most `text.len()`). Its `Display` is
`graph query byte N: <description>: <message>`, where the description is
`syntax error`, `unsupported construct`, `parameter error` or `limit exceeded`. `GraphRef::query_text` returns
`GraphTextError::Query` for these errors and `GraphTextError::Read` for errors
of the typed read, for example `GraphNotFound`, `GraphLimitExceedsCeiling` or an
invalid value. The CLI codes are `graph_query_<kind>`.

| Kind | Examples |
|------|----------|
| `syntax` | bad token, missing clause, unknown variable, single-quoted string, non-finite or out-of-range number |
| `unsupported` | write keywords (`CREATE`, `DELETE`, `SET`, ...); vector-language keywords (`SEARCH`, `NEAR`, ...); `OR`/`NOT`/`IN`; `>`, `<`, `<>`, `!=`; `ORDER BY`/`SKIP`; `OPTIONAL`/`WITH`/`UNWIND`/`CALL`; functions and aggregates; `NULL` and list values; several patterns or relationships; exact, open or non-1 minimum hop ranges (`*`, `*3`, `*2..3`); parameterized hop bounds; unanchored relationship patterns; `;` |
| `parameter` | missing or unused `$name`; `KEY` without a string; `ID` or `LIMIT` without a non-negative integer parameter |
| `limit` | text above 16,384 bytes or 1,024 tokens; hop bound above `GraphLimits::CEILING.max_hops` (4) |

## Semantics to keep in mind

- **Traversal results.** They are the typed traversal's results. Each node is
  returned once, at its minimum depth. The start node is returned at depth 0
  when it matches the end-node filter. Use each result's `depth` to exclude
  it.
- **`LIMIT` meaning.** On a traversal, `LIMIT` caps output nodes through
  `max_output_nodes`. A capped result reports `complete: false` with
  `output_limit` truncation. On `FindNodes` and `Neighbors`, `LIMIT` is the page
  size. Continuing a page needs the returned cursor, which only the typed API
  accepts.
- **Exclusions.** There is no sorting, aggregation, OR, negation or write. Those
  stay with the typed API or the application.

## Dashboard graph explorer

The Dashboard's read-only **Graph** workspace (Data → Graph, FOLLOWUP-002,
`src/dashboard/graph.js`) uses the typed REST routes, not the query text. A
server without the `graph` feature or a file without graphs (409
`graphs_not_enabled`) shows an explanation instead of the explorer. Credentials
without `graph_read` or a graph binding see a "no graph access" notice.

It lists graphs, gets a node or edge by ID or key, finds nodes by label and an
equality or JSON conditions, pages neighbors with direction and relationship
filters, and runs bounded traversals and shortest paths. Results show
completeness, truncation, budgets and indexes, a table, and a small SVG
node-link view. **Explain plan** shows the normalized query and effective
limits. Blank limits use server defaults. Values above the ceiling show the
REST error code, for example `graph_limit_exceeds_ceiling`. Admin sessions can
load `GET /v1/admin/graph/stats`. Writes stay with REST, the CLI or MCP.
Signed-in accounts' graph access follows their role and the `graphs` and
`graph_access` fields in Administration → Users & access (see SECURITY.md).

## Status

The query text was added at the repository owner's direction on 2026-10-25.
That direction skipped the EPIC-014 LLM evaluation (GATE-005 experiments were
not run). It is experimental and opt-in, like the rest of the `graph`
feature. MCP exposes it as the read-only `vlite_graph_query` tool (ITEM-052),
and REST as `POST /v1/graphs/{graph}/query`.
