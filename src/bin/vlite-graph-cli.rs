//! `vlite-graph-cli`: one typed graph operation per invocation (FOLLOWUP-001).
//!
//! It requires the `json` feature (the property graph is compiled into every
//! build since v0.5.1; release archives ship it since v0.5.0). Every command maps onto an
//! existing `vlite::property_graph` call; the `query` command parses one
//! read-only graph query text (docs/GRAPH-QUERY.md) into exactly one typed
//! read and runs it with `GraphRef::query_text`. Reads
//! open the file with `Database::open_read_only` (shared lock, never writes);
//! writes need `--writable` and take the exclusive writer lock for the one
//! command. Success prints one JSON object on stdout; failure prints
//! `{"error":{"code","message"}}` on stderr and exits nonzero. Node, edge and
//! record IDs, revisions and sequences are printed as decimal strings and
//! accepted as strings or JSON numbers, so u64 values never lose precision.

// The agent-surface encodings (schema, retrieve) are used by vlite-mcp only.
#[allow(dead_code)]
#[path = "tool_support/graph_json.rs"]
mod graph_json;

use std::io::{self, Read};
use std::process::ExitCode;

use graph_json::{
    Batch, Empty, LimitsIn, Named, batch_output, de_properties, error_code, explanation, info,
    read_output, read_request,
};
use serde::Deserialize;
use serde_json::{Value, json};
use vlite::property_graph::{
    GraphOp, GraphQueryError, GraphQueryErrorKind, GraphTextError, ReadOptions, parse_graph_query,
};
use vlite::{CreateOptions, Database, Error, Metadata};

const HELP: &str = r#"vlite-graph-cli --db PATH [--writable] [--explain] COMMAND [REQUEST]
Runs one typed graph operation on a graph-enabled VLite file and prints one
JSON object. REQUEST is a JSON object; "-" reads it from stdin; omitted is {}.

Reads (shared lock, never write; --explain plans a query without running it):
  graphs
  get-node      {"graph":"g","node":{"key":"a"}}            node: {"key"} | {"id"}
  get-edge      {"graph":"g","edge":{"id":"0"}}             edge: {"key"} | {"id"}
  find-nodes    {"graph":"g","label":"L","conditions":[],"limit":N,"cursor":"hex"}
  neighbors     {"graph":"g","node":{...},"direction":"out|in|both","types":["T"],
                 "edge_conditions":[],"limit":N,"cursor":"hex"}
  traverse      {"graph":"g","start":[{...}],"direction":"out","types":["T"],
                 "expand_filter":{"labels":[],"conditions":[]},"return_filter":{...},
                 "edge_filter":[],"include_paths":false}
  shortest-path {"graph":"g","source":{...},"destination":{...},"direction":"out","types":["T"]}
  Every query also accepts "limits":{"max_hops":2,...} (above the ceiling is an
  error, never clamped) and "projection":"all"|"none"|{"only":["field"]}.
  query         {"graph":"g","query":"MATCH (a KEY $k)-[:T*1..3]->(b:L) RETURN b LIMIT 20",
                 "params":{"k":{"type":"string","value":"a"}},"limits":{...}}
                one read-only graph query text, translated to exactly one of the
                reads above (docs/GRAPH-QUERY.md); RETURN sets the projection, a
                *1..N range max_hops and a traversal LIMIT max_output_nodes.
                Text errors: graph_query_syntax|unsupported|parameter|limit.

Writes (require --writable; exclusive lock):
  init          {"dimensions":N}   creates a NEW graph-enabled file; never converts
  create-graph  {"graph":"g"}
  drop-graph    {"graph":"g"}      empty graphs only
  batch         {"graph":"g","ops":[...]}   one atomic frame; nothing is written on error
    ops: create_node, upsert_node, patch_node, delete_node, create_edge,
         patch_edge, delete_edge, attach, detach, put_vector, delete_vector

Conditions and metadata use the engine JSON of vlite-shell. Vector-only files
are refused (graphs_not_enabled); convert a copy with vlite-maintain enable-graph.
"#;

/// Requests read from stdin are bounded; the batch frame itself is limited
/// to 4,096 operations and 16 MiB by the library.
const MAX_REQUEST_BYTES: u64 = 64 * 1024 * 1024;

const READS: &[&str] = &[
    "graphs",
    "get-node",
    "get-edge",
    "find-nodes",
    "neighbors",
    "traverse",
    "shortest-path",
    "query",
];
const WRITES: &[&str] = &["init", "create-graph", "drop-graph", "batch"];

struct Failure {
    code: &'static str,
    message: String,
}

impl Failure {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            code: "usage",
            message: message.into(),
        }
    }

    fn request(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_request",
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            code: "internal_error",
            message: message.into(),
        }
    }
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self {
            code: error_code(&error),
            message: error.to_string(),
        }
    }
}

impl From<GraphQueryError> for Failure {
    fn from(error: GraphQueryError) -> Self {
        Self {
            code: match error.kind {
                GraphQueryErrorKind::Syntax => "graph_query_syntax",
                GraphQueryErrorKind::Unsupported => "graph_query_unsupported",
                GraphQueryErrorKind::Parameter => "graph_query_parameter",
                GraphQueryErrorKind::Limit => "graph_query_limit",
            },
            message: error.to_string(),
        }
    }
}

impl From<GraphTextError> for Failure {
    fn from(error: GraphTextError) -> Self {
        match error {
            GraphTextError::Query(error) => error.into(),
            GraphTextError::Read(error) => error.into(),
            _ => Failure::internal("unknown graph query error"),
        }
    }
}

// Command line.

struct Cli {
    db: String,
    writable: bool,
    explain: bool,
    command: String,
    request: Option<String>,
}

fn parse_args(args: Vec<String>) -> Result<Cli, Failure> {
    let (mut db, mut writable, mut explain) = (None, false, false);
    let mut positional = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let seen = match arg.as_str() {
            "--db" => {
                let value = args
                    .next()
                    .ok_or_else(|| Failure::usage("missing value for --db"))?;
                db.replace(value).is_some()
            }
            "--writable" => std::mem::replace(&mut writable, true),
            "--explain" => std::mem::replace(&mut explain, true),
            option if option.starts_with("--") => {
                return Err(Failure::usage(format!(
                    "unknown option {option}; use --help"
                )));
            }
            _ => {
                positional.push(arg);
                false
            }
        };
        if seen {
            return Err(Failure::usage("duplicate option"));
        }
    }
    let db = db.ok_or_else(|| Failure::usage("missing --db; use --help"))?;
    let mut positional = positional.into_iter();
    let command = positional
        .next()
        .ok_or_else(|| Failure::usage("missing COMMAND; use --help"))?;
    let request = positional.next();
    if positional.next().is_some() {
        return Err(Failure::usage("expected COMMAND and at most one REQUEST"));
    }
    let is_read = READS.contains(&command.as_str());
    if !is_read && !WRITES.contains(&command.as_str()) {
        return Err(Failure::usage(format!(
            "unknown command {command}; use --help"
        )));
    }
    if !is_read && !writable {
        return Err(Failure::usage(format!("{command} writes; pass --writable")));
    }
    if is_read && writable {
        return Err(Failure::usage(format!(
            "{command} only reads; --writable is for write commands"
        )));
    }
    if explain && (!is_read || command == "graphs") {
        return Err(Failure::usage("--explain applies to query commands only"));
    }
    Ok(Cli {
        db,
        writable,
        explain,
        command,
        request,
    })
}

fn request_text(request: Option<&str>) -> Result<String, Failure> {
    match request {
        None => Ok("{}".into()),
        Some("-") => {
            let mut text = String::new();
            io::stdin()
                .take(MAX_REQUEST_BYTES + 1)
                .read_to_string(&mut text)
                .map_err(|error| Failure::request(format!("cannot read stdin: {error}")))?;
            if text.len() as u64 > MAX_REQUEST_BYTES {
                return Err(Failure::request("request exceeds 64 MiB"));
            }
            Ok(text)
        }
        Some(text) => Ok(text.into()),
    }
}

fn parse<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T, Failure> {
    graph_json::parse(text).map_err(Failure::request)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Init {
    dimensions: usize,
}

/// A graph query text (EPIC-015 ITEM-051); see docs/GRAPH-QUERY.md. The
/// projection comes from its RETURN clause.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryIn {
    graph: String,
    query: String,
    #[serde(default, deserialize_with = "de_properties")]
    params: Metadata,
    #[serde(default)]
    limits: LimitsIn,
}

// Commands. Requests are parsed before the file is opened.

fn execute(cli: &Cli) -> Result<Value, Failure> {
    let text = request_text(cli.request.as_deref())?;
    debug_assert_eq!(cli.writable, WRITES.contains(&cli.command.as_str()));
    match cli.command.as_str() {
        "init" => {
            let request: Init = parse(&text)?;
            Database::create_with_graph(&cli.db, request.dimensions, CreateOptions::default())?
                .close()?;
            Ok(json!({"dimensions": request.dimensions, "graph_enabled": true}))
        }
        "create-graph" => {
            let request: Named = parse(&text)?;
            let mut db = Database::open(&cli.db)?;
            let created = db.create_graph(&request.graph)?;
            db.close()?;
            Ok(json!({"graph": info(&created)}))
        }
        "drop-graph" => {
            let request: Named = parse(&text)?;
            let mut db = Database::open(&cli.db)?;
            db.drop_graph(&request.graph)?;
            db.close()?;
            Ok(json!({"dropped": request.graph}))
        }
        "batch" => {
            let request: Batch = parse(&text)?;
            let ops: Vec<GraphOp> = request.ops.into_iter().map(Into::into).collect();
            let mut db = Database::open(&cli.db)?;
            let result = db.graph_mut(&request.graph)?.write_batch(&ops)?;
            db.close()?;
            batch_output(&request.graph, &result).map_err(Failure::internal)
        }
        "graphs" => {
            let _: Empty = parse(&text)?;
            let db = Database::open_read_only(&cli.db)?;
            let graphs: Value = db.list_graphs()?.iter().map(info).collect();
            Ok(json!({"graphs": graphs}))
        }
        "query" => {
            let request: QueryIn = parse(&text)?;
            // Parsed here too so text errors are reported before the file opens.
            parse_graph_query(&request.query, &request.params)?;
            let limits = request.limits.resolve();
            let db = Database::open_read_only(&cli.db)?;
            let graph = db.graph(&request.graph)?;
            if cli.explain {
                let plan = graph.explain_text(&request.query, &request.params, &limits)?;
                explanation(&request.graph, &plan).map_err(Failure::internal)
            } else {
                let read = graph.query_text(&request.query, &request.params, &limits)?;
                read_output(&request.graph, &read).map_err(Failure::internal)
            }
        }
        command => {
            let request = read_request(command, &text).map_err(Failure::request)?;
            let db = Database::open_read_only(&cli.db)?;
            let graph = db.graph(&request.graph)?;
            let options = ReadOptions {
                limits: request.limits,
                projection: request.projection,
                cancel: None,
            };
            if cli.explain {
                explanation(&request.graph, &graph.explain(&request.query, &options)?)
                    .map_err(Failure::internal)
            } else {
                read_output(&request.graph, &graph.query(&request.query, &options)?)
                    .map_err(Failure::internal)
            }
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help") {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    match parse_args(args).and_then(|cli| execute(&cli)) {
        Ok(value) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Err(failure) => {
            eprintln!(
                "{}",
                json!({"error": {"code": failure.code, "message": failure.message}})
            );
            ExitCode::FAILURE
        }
    }
}
