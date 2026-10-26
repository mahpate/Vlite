//! Property-graph tools of `vlite-mcp` (EPIC-009; `graph` feature, source-only).
//!
//! Listed and callable only when the opened file is graph-enabled; on
//! vector-only files `tools/list`, `vlite_info` and calls behave exactly as in
//! builds without `graph`. Read tools never write. Write tools exist only
//! with `--writable`. Inputs bind typed JSON fields and reuse the
//! `vlite-graph-cli` request JSON; `vlite_graph_query` is the one free-text
//! tool and only accepts the read-only query text of EPIC-015, which
//! translates to exactly one typed read. Outputs reuse the compact encoding
//! (u64 IDs as decimal strings, `complete`/`truncation`/`stats` on every
//! read, no vectors) shared with the REST graph routes. Errors are
//! `"<code>: <message>"` with the stable graph error codes.

use serde::Deserialize;
use serde_json::{Map, Value, json};
use vlite::Database;
use vlite::property_graph::{
    EvidenceSpec, GraphLimits, GraphOp, GraphTextError, Pivot, ReadOptions, parse_graph_query,
};

use crate::graph_json::{
    self, Batch, Empty, Named, QueryTextIn, SchemaIn, SchemaQueryIn, batch_output, error_code,
    explanation, info, parse_value, put_schema_output, read_output, read_request, retrieval_output,
    retrieve_request, schema_output, split_graph, text_error,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutSchemaIn {
    graph: String,
    schema: SchemaIn,
}

/// Read tools and the `vlite-graph-cli` query command each one runs.
const QUERY_TOOLS: &[(&str, &str)] = &[
    ("vlite_graph_find_nodes", "find-nodes"),
    ("vlite_graph_neighbors", "neighbors"),
    ("vlite_graph_traverse", "traverse"),
    ("vlite_graph_shortest_path", "shortest-path"),
];
const LIST_TOOL: &str = "vlite_graph_list";
const GET_TOOL: &str = "vlite_graph_get";
/// Wave-2 read tools: declared + observed schema (AC-018), hybrid retrieve
/// (ITEM-038) and read-only query text (ITEM-052).
const SCHEMA_TOOL: &str = "vlite_graph_schema";
const RETRIEVE_TOOL: &str = "vlite_graph_retrieve";
const TEXT_TOOL: &str = "vlite_graph_query";
const PUT_SCHEMA_TOOL: &str = "vlite_graph_put_schema";
const WRITE_TOOLS: &[&str] = &[
    "vlite_graph_create",
    "vlite_graph_drop",
    "vlite_graph_write",
    PUT_SCHEMA_TOOL,
];

fn failure(error: &vlite::Error) -> String {
    format!("{}: {error}", error_code(error))
}

fn invalid(message: impl std::fmt::Display) -> String {
    format!("invalid_request: {message}")
}

pub(crate) fn is_graph_tool(name: &str) -> bool {
    name == LIST_TOOL
        || name == GET_TOOL
        || name == SCHEMA_TOOL
        || name == RETRIEVE_TOOL
        || name == TEXT_TOOL
        || QUERY_TOOLS.iter().any(|(tool, _)| *tool == name)
        || WRITE_TOOLS.contains(&name)
}

/// Adds `graph_enabled` and per-graph counts to `vlite_info`, only on
/// graph-enabled files (REQ-036).
pub(crate) fn extend_info(db: &Database, value: &mut Value) -> Result<(), String> {
    if !db.is_graph_enabled() {
        return Ok(());
    }
    let graphs: Value = db
        .list_graphs()
        .map_err(|error| failure(&error))?
        .iter()
        .map(info)
        .collect();
    if let Some(object) = value.as_object_mut() {
        object.insert("graph_enabled".into(), json!(true));
        object.insert("graphs".into(), graphs);
    }
    Ok(())
}

/// Graph tools for `tools/list`; empty for vector-only files.
pub(crate) fn tool_definitions(db: &Database, read_only: bool) -> Vec<Value> {
    if !db.is_graph_enabled() {
        return Vec::new();
    }
    definitions(read_only)
}

/// The graph tool list of a graph-enabled database (local or proxied).
pub(crate) fn definitions(read_only: bool) -> Vec<Value> {
    let mut tools = read_tool_definitions();
    if !read_only {
        tools.extend(write_tool_definitions());
    }
    tools
}

/// The error a read-only session reports for a write tool.
pub(crate) fn read_only_error(name: &str) -> Option<String> {
    WRITE_TOOLS
        .contains(&name)
        .then(|| format!("read_only: database is read-only; {name} is unavailable"))
}

/// The `vlite-graph-cli` read command of a typed read tool; `None` for other
/// tools.
pub(crate) fn typed_read(
    name: &str,
    args: &Map<String, Value>,
) -> Option<Result<&'static str, String>> {
    if name == GET_TOOL {
        return Some(
            match (args.contains_key("node"), args.contains_key("edge")) {
                (true, false) => Ok("get-node"),
                (false, true) => Ok("get-edge"),
                _ => Err(invalid("provide exactly one of node or edge")),
            },
        );
    }
    QUERY_TOOLS
        .iter()
        .find(|(tool, _)| *tool == name)
        .map(|(_, command)| Ok(*command))
}

/// Runs a graph tool; `None` when `name` is not one or the file is not
/// graph-enabled, so the caller reports `unknown tool` as before.
pub(crate) fn call(
    db: &mut Database,
    read_only: bool,
    name: &str,
    args: &Value,
) -> Option<Result<Value, String>> {
    if !is_graph_tool(name) || !db.is_graph_enabled() {
        return None;
    }
    Some(run(db, read_only, name, args))
}

fn run(db: &mut Database, read_only: bool, name: &str, args: &Value) -> Result<Value, String> {
    let Some(object) = args.as_object() else {
        return Err(invalid("tool arguments must be a JSON object"));
    };
    if read_only && let Some(error) = read_only_error(name) {
        return Err(error);
    }
    let text = args.to_string();
    match name {
        LIST_TOOL => {
            let _: Empty = graph_json::parse(&text).map_err(invalid)?;
            let graphs: Value = db
                .list_graphs()
                .map_err(|error| failure(&error))?
                .iter()
                .map(info)
                .collect();
            Ok(json!({"graphs": graphs}))
        }
        "vlite_graph_create" => {
            let request: Named = graph_json::parse(&text).map_err(invalid)?;
            let created = db
                .create_graph(&request.graph)
                .map_err(|error| failure(&error))?;
            Ok(json!({"graph": info(&created)}))
        }
        "vlite_graph_drop" => {
            let request: Named = graph_json::parse(&text).map_err(invalid)?;
            db.drop_graph(&request.graph)
                .map_err(|error| failure(&error))?;
            Ok(json!({"dropped": request.graph}))
        }
        "vlite_graph_write" => {
            let request: Batch = graph_json::parse(&text).map_err(invalid)?;
            let ops: Vec<GraphOp> = request.ops.into_iter().map(Into::into).collect();
            let result = db
                .graph_mut(&request.graph)
                .map_err(|error| failure(&error))?
                .write_batch(&ops)
                .map_err(|error| failure(&error))?;
            batch_output(&request.graph, &result)
        }
        PUT_SCHEMA_TOOL => {
            let request: PutSchemaIn = graph_json::parse(&text).map_err(invalid)?;
            let revision = db
                .graph_mut(&request.graph)
                .map_err(|error| failure(&error))?
                .put_schema(&request.schema.into_schema())
                .map_err(|error| failure(&error))?;
            Ok(put_schema_output(&request.graph, revision))
        }
        SCHEMA_TOOL => {
            let (graph, body) = split_graph(args).map_err(invalid)?;
            let request: SchemaQueryIn = parse_value(body).map_err(invalid)?;
            let found = db.graph(&graph).map_err(|error| failure(&error))?;
            let observed = found
                .observed_schema(request.budget)
                .map_err(|error| failure(&error))?;
            Ok(schema_output(&graph, &found.schema(), &observed))
        }
        RETRIEVE_TOOL => {
            let (graph, body) = split_graph(args).map_err(invalid)?;
            let request = retrieve_request(body).map_err(invalid)?;
            let options = ReadOptions {
                limits: request.limits,
                projection: request.projection,
                cancel: None,
            };
            let retrieval = db
                .graph(&graph)
                .map_err(|error| failure(&error))?
                .retrieve(&request.retrieve, &options)
                .map_err(|error| failure(&error))?;
            let evidence: Vec<_> = retrieval.evidence.iter().collect();
            Ok(retrieval_output(&graph, &retrieval, &evidence))
        }
        TEXT_TOOL => {
            let (graph, body) = split_graph(args).map_err(invalid)?;
            let request: QueryTextIn = parse_value(body).map_err(invalid)?;
            let text_failure = |error: GraphTextError| {
                let (code, message) = text_error(&error);
                format!("{code}: {message}")
            };
            // Text errors are reported before the graph is looked up.
            parse_graph_query(&request.query, &request.params)
                .map_err(|error| text_failure(GraphTextError::Query(error)))?;
            let limits = request.limits.resolve();
            let found = db.graph(&graph).map_err(|error| failure(&error))?;
            if request.explain {
                let plan = found
                    .explain_text(&request.query, &request.params, &limits)
                    .map_err(text_failure)?;
                explanation(&graph, &plan)
            } else {
                let read = found
                    .query_text(&request.query, &request.params, &limits)
                    .map_err(text_failure)?;
                read_output(&graph, &read)
            }
        }
        _ => query(db, name, object),
    }
}

fn query(db: &Database, name: &str, args: &Map<String, Value>) -> Result<Value, String> {
    let mut args = args.clone();
    let explain = match args.remove("explain") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| invalid("explain must be a boolean"))?,
    };
    let command =
        typed_read(name, &args).unwrap_or_else(|| Err(format!("unknown tool: {name}")))?;
    let request = read_request(command, &Value::Object(args).to_string()).map_err(invalid)?;
    let graph = db.graph(&request.graph).map_err(|error| failure(&error))?;
    let options = ReadOptions {
        limits: request.limits,
        projection: request.projection,
        cancel: None,
    };
    if explain {
        let plan = graph
            .explain(&request.query, &options)
            .map_err(|error| failure(&error))?;
        explanation(&request.graph, &plan)
    } else {
        let read = graph
            .query(&request.query, &options)
            .map_err(|error| failure(&error))?;
        read_output(&request.graph, &read)
    }
}

// Tool definitions. Every object schema sets `additionalProperties: false`.

fn annotations(read_only: bool, destructive: bool, idempotent: bool) -> Value {
    json!({"readOnlyHint":read_only,"destructiveHint":destructive,"idempotentHint":idempotent})
}

fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    hints: Value,
) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
        "annotations": hints,
    })
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

fn u64_schema() -> Value {
    json!({"oneOf":[{"type":"string","pattern":"^(0|[1-9][0-9]*)$"},{"type":"integer","minimum":0}]})
}

fn graph_schema() -> Value {
    json!({"type":"string","minLength":1,"maxLength":63,"description":"Graph name (vlite_graph_list)."})
}

fn selector_schema() -> Value {
    json!({"oneOf":[
        object(json!({"key":{"type":"string","minLength":1}}), &["key"]),
        object(json!({"id":u64_schema()}), &["id"]),
    ]})
}

fn properties_schema() -> Value {
    json!({"type":"object","additionalProperties":crate::metadata_value_schema(),"description":"Typed values, as vlite_upsert metadata."})
}

fn expected_schema() -> Value {
    json!({"oneOf":[
        {"type":"string","enum":["any","absent"]},
        object(json!({"revision":u64_schema()}), &["revision"]),
    ],"default":"any"})
}

fn filter_schema() -> Value {
    object(
        json!({"labels":{"type":"array","items":{"type":"string"}},"conditions":crate::filter_schema()}),
        &[],
    )
}

fn common_read_properties() -> Map<String, Value> {
    let ceiling = GraphLimits::CEILING;
    let default = GraphLimits::DEFAULT;
    let limit = |max: u64, default: u64| json!({"type":"integer","minimum":0,"maximum":max,"default":default});
    let mut limits = object(
        json!({
            "max_hops": limit(ceiling.max_hops, default.max_hops),
            "max_visited_nodes": limit(ceiling.max_visited_nodes, default.max_visited_nodes),
            "max_edges_examined": limit(ceiling.max_edges_examined, default.max_edges_examined),
            "max_edges_per_node": limit(ceiling.max_edges_per_node, default.max_edges_per_node),
            "deadline_ms": limit(ceiling.deadline_ms, default.deadline_ms),
            "max_output_nodes": limit(ceiling.max_output_nodes, default.max_output_nodes),
            "max_output_edges": limit(ceiling.max_output_edges, default.max_output_edges),
            "max_output_paths": limit(ceiling.max_output_paths, default.max_output_paths),
        }),
        &[],
    );
    limits["description"] = json!(
        "Per-call bounds; unset fields use the default. Values above the maximum are rejected (graph_limit_exceeds_ceiling), never clamped."
    );
    let mut properties = Map::new();
    properties.insert("graph".into(), graph_schema());
    properties.insert("limits".into(), limits);
    properties.insert(
        "projection".into(),
        json!({"oneOf":[{"type":"string","enum":["all","none"]},object(json!({"only":{"type":"array","items":{"type":"string"}}}), &["only"])],"default":"all","description":"Which node/edge properties to return."}),
    );
    properties.insert(
        "explain".into(),
        json!({"type":"boolean","default":false,"description":"Return the normalized query, effective limits and planned indexes without executing."}),
    );
    properties
}

fn with(mut base: Map<String, Value>, extra: Value) -> Value {
    if let Value::Object(extra) = extra {
        base.extend(extra);
    }
    Value::Object(base)
}

fn read_tool_definitions() -> Vec<Value> {
    let hints = || annotations(true, false, true);
    let direction = || json!({"type":"string","enum":["out","in","both"],"default":"out"});
    let types = || json!({"type":"array","items":{"type":"string"},"description":"Relationship types to follow; omitted follows all."});
    let cursor = || json!({"type":"string","pattern":"^([0-9a-f]{2})*$","description":"The cursor a previous page returned; stale after a graph write."});
    let page_limit = || json!({"type":"integer","minimum":0});
    vec![
        tool(
            LIST_TOOL,
            "List the property graphs in this graph-enabled file with node and edge counts.",
            json!({}),
            &[],
            hints(),
        ),
        tool(
            GET_TOOL,
            "Fetch one node or one edge by key or ID. A null result means it does not exist.",
            with(
                common_read_properties(),
                json!({"node":selector_schema(),"edge":selector_schema()}),
            ),
            &["graph"],
            hints(),
        ),
        tool(
            "vlite_graph_find_nodes",
            "Find nodes by label and typed property conditions in ascending node ID order, with cursor pagination.",
            with(
                common_read_properties(),
                json!({"label":{"type":"string"},"conditions":crate::filter_schema(),"limit":page_limit(),"cursor":cursor()}),
            ),
            &["graph"],
            hints(),
        ),
        tool(
            "vlite_graph_neighbors",
            "List a node's incident edges and adjacent nodes, ordered by relationship type then edge ID.",
            with(
                common_read_properties(),
                json!({"node":selector_schema(),"direction":direction(),"types":types(),"edge_conditions":crate::filter_schema(),"limit":page_limit(),"cursor":cursor()}),
            ),
            &["graph", "node"],
            hints(),
        ),
        tool(
            "vlite_graph_traverse",
            "Bounded breadth-first traversal from start nodes; returns deduplicated nodes with hop depth, edges and optional paths. Check complete and truncation.",
            with(
                common_read_properties(),
                json!({"start":{"type":"array","items":selector_schema(),"minItems":1},"direction":direction(),"types":types(),"expand_filter":filter_schema(),"return_filter":filter_schema(),"edge_filter":crate::filter_schema(),"include_paths":{"type":"boolean","default":false}}),
            ),
            &["graph", "start"],
            hints(),
        ),
        tool(
            "vlite_graph_shortest_path",
            "One unweighted shortest path between two nodes. Outcome found, no_path_within_hops (fully explored) or exhausted (limits hit; absence unknown).",
            with(
                common_read_properties(),
                json!({"source":selector_schema(),"destination":selector_schema(),"direction":direction(),"types":types()}),
            ),
            &["graph", "source", "destination"],
            hints(),
        ),
        tool(
            SCHEMA_TOOL,
            "Declared schema (labels, relationship types, endpoints, typed properties, indexes) plus the observed summary with counts and origin declared/observed/both. Call before writing or querying unfamiliar graphs.",
            json!({
                "graph": graph_schema(),
                "budget": {"type":"integer","minimum":1,"maximum":1_000_000,"default":100_000,"description":"Entities (nodes then edges) the observed summary scans; observed.complete is false when it stops early."},
            }),
            &["graph"],
            hints(),
        ),
        tool(
            RETRIEVE_TOOL,
            "Hybrid retrieval: one vector and/or BM25 text pivot search over a collection, pivot records mapped to attached nodes, bounded expansion, and evidence records attached to returned nodes (metadata and payload excerpts; never vectors). Check complete, truncation and unmapped_pivots.",
            with(
                common_retrieve_properties(),
                json!({
                    "pivot": object(json!({
                        "collection": {"type":"string","description":"Collection searched for pivot records."},
                        "vector": {"type":"array","items":{"type":"number"},"minItems":1},
                        "text": {"type":"string","description":"BM25 text leg; with vector both legs are fused (RRF)."},
                        "k": {"type":"integer","minimum":1,"maximum":Pivot::MAX_K,"default":Pivot::DEFAULT_K},
                        "filter": crate::filter_schema(),
                        "metric": {"type":"string","enum":["squared_l2","cosine","inner_product"],"default":"squared_l2"},
                        "max_exact_scan_records": {"type":"integer","minimum":1,"maximum":Pivot::MAX_EXACT_SCAN_RECORDS,"default":Pivot::DEFAULT_MAX_EXACT_SCAN_RECORDS,"description":"Larger collections fail graph_pivot_too_expensive."},
                        "candidates": {"type":"integer","minimum":1},
                        "rrf_k": {"type":"integer","minimum":1,"default":60},
                        "vector_weight": {"type":"number","minimum":0,"default":1.0},
                        "text_weight": {"type":"number","minimum":0,"default":1.0},
                    }), &["collection"]),
                    "expansion": object(json!({
                        "direction": direction(),
                        "types": types(),
                        "expand_filter": filter_schema(),
                        "return_filter": filter_schema(),
                        "edge_filter": crate::filter_schema(),
                        "include_paths": {"type":"boolean","default":false},
                    }), &[]),
                    "evidence": object(json!({
                        "collections": {"type":"array","items":{"type":"string"},"maxItems":64,"description":"Collections whose attached records are returned as evidence; omitted returns none."},
                        "fields": projection_schema(),
                        "max_excerpt_bytes": {"type":"integer","minimum":0,"maximum":EvidenceSpec::MAX_EXCERPT_BYTES,"default":EvidenceSpec::DEFAULT_EXCERPT_BYTES},
                        "max_items": {"type":"integer","minimum":0,"maximum":EvidenceSpec::MAX_ITEMS,"default":EvidenceSpec::DEFAULT_ITEMS},
                    }), &[]),
                }),
            ),
            &["graph", "pivot"],
            hints(),
        ),
        tool(
            TEXT_TOOL,
            "Run one read-only graph query text (MATCH ... RETURN ... LIMIT; see GRAPH-QUERY.md) that translates to exactly one typed read; values bind as typed $params. Same output as the typed tool it maps to.",
            json!({
                "graph": graph_schema(),
                "query": {"type":"string","minLength":1,"maxLength":4096},
                "params": properties_schema(),
                "limits": common_read_properties()["limits"].clone(),
                "explain": common_read_properties()["explain"].clone(),
            }),
            &["graph", "query"],
            hints(),
        ),
    ]
}

/// `graph`, `limits` and `projection` of the read tools (no `explain`).
fn common_retrieve_properties() -> Map<String, Value> {
    let mut properties = common_read_properties();
    properties.remove("explain");
    properties
}

fn projection_schema() -> Value {
    json!({"oneOf":[{"type":"string","enum":["all","none"]},object(json!({"only":{"type":"array","items":{"type":"string"}}}), &["only"])],"default":"all","description":"Which properties to return."})
}

/// The declared schema shape `vlite_graph_schema` returns as `declared`.
fn declared_schema_schema() -> Value {
    let property = object(
        json!({
            "type": {"type":"string","enum":["string","integer","float","boolean","geopoint"]},
            "required": {"type":"boolean","default":false},
            "unique": {"type":"boolean","default":false},
            "index": {"type":["string","null"],"enum":["exact","range",null],"description":"Exact postings for eq, range postings for numeric ranges."},
        }),
        &["type"],
    );
    let properties = json!({"type":"object","additionalProperties":property});
    let description = json!({"type":"string","maxLength":1024});
    let label = object(
        json!({"description":description,"properties":properties}),
        &[],
    );
    let relationship = object(
        json!({
            "description": description,
            "endpoints": {"type":"array","maxItems":64,"items":object(json!({"source":{"type":"string"},"destination":{"type":"string"}}), &["source","destination"])},
            "multiplicity": {"type":"string","enum":["multi","unique_triple"],"default":"multi","description":"unique_triple allows ensure_edge."},
            "properties": properties,
        }),
        &[],
    );
    let mut schema = object(
        json!({
            "labels": {"type":"object","additionalProperties":label,"maxProperties":256},
            "relationships": {"type":"object","additionalProperties":relationship,"maxProperties":256},
        }),
        &[],
    );
    schema["description"] = json!(
        "Replaces the whole declared schema; {} removes it. Existing data must conform (graph_schema_violation)."
    );
    schema
}

fn op_schemas() -> Vec<Value> {
    let op = |name: &str, properties: Value, required: &[&str]| {
        let mut all = Map::new();
        all.insert("op".into(), json!({"const": name}));
        if let Value::Object(properties) = properties {
            all.extend(properties);
        }
        let mut required: Vec<&str> = required.to_vec();
        required.insert(0, "op");
        object(Value::Object(all), &required)
    };
    let labels = json!({"type":"array","items":{"type":"string"}});
    let names = json!({"type":"array","items":{"type":"string"}});
    let edge = selector_schema();
    vec![
        op(
            "create_node",
            json!({"key":{"type":"string"},"labels":labels,"properties":properties_schema()}),
            &["key"],
        ),
        op(
            "upsert_node",
            json!({"key":{"type":"string"},"labels":labels,"properties":properties_schema(),"expected":expected_schema()}),
            &["key"],
        ),
        op(
            "patch_node",
            json!({"key":{"type":"string"},"add_labels":labels,"remove_labels":labels,"set":properties_schema(),"unset":names,"expected":expected_schema()}),
            &["key"],
        ),
        op(
            "delete_node",
            json!({"key":{"type":"string"},"expected":expected_schema()}),
            &["key"],
        ),
        op(
            "create_edge",
            json!({"key":{"type":"string","description":"Optional idempotency key; a different edge under an existing key fails graph_key_conflict."},"source":{"type":"string"},"relationship":{"type":"string","pattern":"^[A-Za-z0-9_]{1,63}$"},"destination":{"type":"string"},"properties":properties_schema()}),
            &["source", "relationship", "destination"],
        ),
        op(
            "patch_edge",
            json!({"edge":edge,"set":properties_schema(),"unset":names,"expected":expected_schema()}),
            &["edge"],
        ),
        op(
            "delete_edge",
            json!({"edge":edge,"expected":expected_schema()}),
            &["edge"],
        ),
        op(
            "attach",
            json!({"node":{"type":"string"},"collection":{"type":"string"},"record":u64_schema()}),
            &["node", "collection", "record"],
        ),
        op(
            "detach",
            json!({"node":{"type":"string"},"collection":{"type":"string"}}),
            &["node", "collection"],
        ),
        op(
            "put_vector",
            json!({"collection":{"type":"string"},"id":u64_schema(),"vector":{"type":"array","items":{"type":"number"},"minItems":1},"payload":{"type":"string"},"metadata":properties_schema()}),
            &["collection", "id", "vector"],
        ),
        op(
            "delete_vector",
            json!({"collection":{"type":"string"},"id":u64_schema()}),
            &["collection", "id"],
        ),
        op(
            "ensure_edge",
            json!({"source":{"type":"string"},"relationship":{"type":"string","pattern":"^[A-Za-z0-9_]{1,63}$","description":"Must be declared multiplicity unique_triple."},"destination":{"type":"string"},"properties":properties_schema()}),
            &["source", "relationship", "destination"],
        ),
        op(
            "put_schema",
            json!({"schema":declared_schema_schema()}),
            &["schema"],
        ),
    ]
}

fn write_tool_definitions() -> Vec<Value> {
    let named = json!({"graph": graph_schema()});
    vec![
        tool(
            "vlite_graph_create",
            "Create an empty named graph.",
            named.clone(),
            &["graph"],
            annotations(false, false, false),
        ),
        tool(
            "vlite_graph_drop",
            "Drop an empty graph; fails graph_not_empty while it has nodes.",
            named,
            &["graph"],
            annotations(false, true, false),
        ),
        tool(
            "vlite_graph_write",
            "Apply node, edge, ensure_edge, schema, attachment and vector operations atomically in one commit, in order; nothing is written on error.",
            json!({"graph":graph_schema(),"ops":{"type":"array","items":{"oneOf":op_schemas()},"maxItems":4096}}),
            &["graph", "ops"],
            annotations(false, true, false),
        ),
        tool(
            PUT_SCHEMA_TOOL,
            "Replace the graph's declared schema (labels, relationship types, endpoints, typed properties, unique/indexed properties) in one commit; existing data must conform or nothing changes (graph_schema_violation).",
            json!({"graph":graph_schema(),"schema":declared_schema_schema()}),
            &["graph", "schema"],
            annotations(false, true, true),
        ),
    ]
}
