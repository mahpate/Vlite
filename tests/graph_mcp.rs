//! EPIC-009: `vlite-mcp` property-graph tools (opt-in `graph` feature).
//!
//! Vector-only files keep the golden MCP transcript byte for byte; graph tools
//! appear only on graph-enabled files; write tools only with `--writable`.
//! `tests/fixtures/graph_agent/tasks.json` holds the scripted UC-002..UC-004
//! agent tasks (AC-018).
#![cfg(all(feature = "mcp", feature = "graph"))]

use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

use serde_json::{Value, json};
use vlite::{CreateOptions, Database};

const READ_TOOLS: &[&str] = &[
    "vlite_graph_list",
    "vlite_graph_get",
    "vlite_graph_find_nodes",
    "vlite_graph_neighbors",
    "vlite_graph_traverse",
    "vlite_graph_shortest_path",
    "vlite_graph_schema",
    "vlite_graph_retrieve",
    "vlite_graph_query",
];
const WRITE_TOOLS: &[&str] = &[
    "vlite_graph_create",
    "vlite_graph_drop",
    "vlite_graph_write",
    "vlite_graph_put_schema",
];

fn fixture_text(name: &str) -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name),
    )
    .unwrap()
}

fn vector_fixture(path: &Path) {
    let mut db = Database::create(path, 2).unwrap();
    db.put(1, &[0.0, 0.0], b"origin").unwrap();
    db.put(2, &[1.0, 1.0], b"near").unwrap();
    db.put(3, &[2.0, 2.0], b"far").unwrap();
    db.close().unwrap();
}

fn graph_fixture(path: &Path) {
    let mut db = Database::create_with_graph(path, 2, CreateOptions::default()).unwrap();
    db.put(1, &[0.0, 0.0], b"origin").unwrap();
    db.close().unwrap();
}

/// Raw stdout lines of one `vlite-mcp` session fed `input`.
fn session(path: &Path, writable: bool, input: &str) -> Vec<String> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_vlite-mcp"))
        .args(["--db", path.to_str().unwrap()])
        .arg(if writable {
            "--writable"
        } else {
            "--read-only"
        })
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

fn initialize() -> String {
    json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"graph-mcp","version":"0"}}}).to_string()
}

/// Initializes, then sends `requests` as ids 1.. and returns their responses.
fn run(path: &Path, writable: bool, requests: &[(&str, Value)]) -> Vec<Value> {
    let mut input = initialize();
    input.push('\n');
    for (index, (method, params)) in requests.iter().enumerate() {
        let line = json!({"jsonrpc":"2.0","id":index + 1,"method":method,"params":params});
        input.push_str(&line.to_string());
        input.push('\n');
    }
    let responses: Vec<Value> = session(path, writable, &input)
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), requests.len() + 1);
    responses[1..].to_vec()
}

fn call(name: &str, arguments: Value) -> (&'static str, Value) {
    ("tools/call", json!({"name": name, "arguments": arguments}))
}

fn tool_names(response: &Value) -> Vec<String> {
    response["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect()
}

fn ok(response: &Value) -> &Value {
    let result = &response["result"];
    assert_eq!(result["isError"], false, "{response}");
    assert_eq!(
        result["content"][0]["text"].as_str().unwrap(),
        result["structuredContent"].to_string()
    );
    &result["structuredContent"]
}

fn error(response: &Value) -> &str {
    let result = &response["result"];
    assert_eq!(result["isError"], true, "{response}");
    result["structuredContent"]["error"].as_str().unwrap()
}

#[test]
fn vector_only_files_match_the_golden_transcript_byte_for_byte() {
    let requests = fixture_text("graph_baseline/mcp_requests.jsonl");
    for (mode, golden, writable) in [
        ("read-only", "graph_baseline/mcp_read_only.json", false),
        ("writable", "graph_baseline/mcp_writable.json", true),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp.vlite");
        vector_fixture(&path);
        let golden_text = fixture_text(golden);
        let expected: Vec<Value> = serde_json::from_str(&golden_text).unwrap();
        let actual = session(&path, writable, &requests);
        assert_eq!(actual.len(), expected.len(), "{mode}: response count");
        // The golden file is the pretty-printed array of the served lines;
        // compare raw text so number formatting is checked too (parsing
        // would round floats). Only the server version is masked.
        let served = format!("[{}]", actual.join(",")).replace(
            &format!(r#""version":"{}""#, env!("CARGO_PKG_VERSION")),
            r#""version":"0.3.0""#,
        );
        assert_eq!(served, minify(&golden_text), "{mode}: transcript bytes");
    }
}

/// Removes insignificant whitespace from JSON text.
fn minify(text: &str) -> String {
    let (mut out, mut in_string, mut escaped) = (String::new(), false, false);
    for character in text.chars() {
        if in_string {
            out.push(character);
            match (escaped, character) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
        } else if character == '"' {
            in_string = true;
            out.push(character);
        } else if !character.is_whitespace() {
            out.push(character);
        }
    }
    out
}

#[test]
fn vector_only_files_hide_graph_tools_and_info_fields() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("vector.vlite");
    vector_fixture(&path);
    let mut requests = vec![("tools/list", json!({})), call("vlite_info", json!({}))];
    for name in READ_TOOLS.iter().chain(WRITE_TOOLS) {
        requests.push(call(name, json!({"graph": "kb"})));
    }
    let responses = run(&path, true, &requests);
    assert!(
        tool_names(&responses[0])
            .iter()
            .all(|name| !name.starts_with("vlite_graph_"))
    );
    let info = ok(&responses[1]);
    assert!(info.get("graph_enabled").is_none() && info.get("graphs").is_none());
    for (response, name) in responses[2..]
        .iter()
        .zip(READ_TOOLS.iter().chain(WRITE_TOOLS))
    {
        assert_eq!(error(response), format!("unknown tool: {name}"));
    }
}

#[test]
fn graph_enabled_files_list_graph_tools_by_access_mode() {
    let directory = tempfile::tempdir().unwrap();
    let vector = directory.path().join("vector.vlite");
    let graph = directory.path().join("graph.vlite");
    vector_fixture(&vector);
    graph_fixture(&graph);
    for writable in [false, true] {
        let baseline = tool_names(&run(&vector, writable, &[("tools/list", json!({}))])[0]);
        let response = &run(&graph, writable, &[("tools/list", json!({}))])[0];
        let names = tool_names(response);
        // Vector tools keep their order; graph tools follow them.
        assert_eq!(names[..baseline.len()], baseline[..]);
        let mut expected: Vec<&str> = READ_TOOLS.to_vec();
        if writable {
            expected.extend(WRITE_TOOLS);
        }
        assert_eq!(names[baseline.len()..], expected[..], "writable={writable}");
        for tool in &response["result"]["tools"].as_array().unwrap()[baseline.len()..] {
            let name = tool["name"].as_str().unwrap();
            assert_eq!(tool["inputSchema"]["additionalProperties"], false, "{name}");
            let read_only = READ_TOOLS.contains(&name);
            assert_eq!(tool["annotations"]["readOnlyHint"], read_only, "{name}");
        }
        // Limits advertise the engine ceilings.
        let limits = &response["result"]["tools"][baseline.len() + 5]["inputSchema"]["properties"]
            ["limits"]["properties"];
        assert_eq!(limits["max_hops"]["maximum"], 4);
        assert_eq!(limits["max_hops"]["default"], 2);
        assert_eq!(limits["max_visited_nodes"]["maximum"], 10_000);
        assert_eq!(limits["max_output_paths"]["maximum"], 100);
    }
}

#[test]
fn read_only_sessions_reject_writes_and_validate_requests() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.vlite");
    graph_fixture(&path);
    let seeded = run(
        &path,
        true,
        &[
            call("vlite_graph_create", json!({"graph": "kb"})),
            call(
                "vlite_graph_write",
                json!({"graph": "kb", "ops": [
                    {"op": "create_node", "key": "a"},
                    {"op": "create_node", "key": "b"},
                    {"op": "create_edge", "key": "ab", "source": "a", "relationship": "LINKS", "destination": "b"},
                    {"op": "attach", "node": "a", "collection": "default", "record": "1"}
                ]}),
            ),
        ],
    );
    ok(&seeded[0]);
    assert_eq!(ok(&seeded[1])["outcomes"].as_array().unwrap().len(), 4);

    let responses = run(
        &path,
        false,
        &[
            call("vlite_graph_create", json!({"graph": "other"})),
            call("vlite_graph_drop", json!({"graph": "kb"})),
            call(
                "vlite_graph_write",
                json!({"graph": "kb", "ops": [{"op": "delete_node", "key": "a"}]}),
            ),
            call(
                "vlite_graph_get",
                json!({"graph": "kb", "node": {"key": "a"}, "edge": {"key": "ab"}}),
            ),
            call("vlite_graph_get", json!({"graph": "kb"})),
            call(
                "vlite_graph_get",
                json!({"graph": "kb", "node": {"key": "a"}, "extra": 1}),
            ),
            call(
                "vlite_graph_get",
                json!({"graph": "kb", "node": {"key": "a"}, "explain": "yes"}),
            ),
            call("vlite_graph_list", json!({"graph": "kb"})),
            call(
                "vlite_graph_neighbors",
                json!({"graph": "kb", "node": {"key": "a"}, "limits": {"max_edges_per_node": 10_001}}),
            ),
            call(
                "vlite_graph_traverse",
                json!({"graph": "kb", "start": [{"key": "a"}], "limits": {"max_visited_nodes": 0}}),
            ),
            call(
                "vlite_graph_get",
                json!({"graph": "kb", "node": {"id": "0"}}),
            ),
            call(
                "vlite_graph_neighbors",
                json!({"graph": "kb", "node": {"key": "a"}, "explain": true}),
            ),
            call("vlite_graph_list", json!({})),
            (
                "tools/call",
                json!({"name": "vlite_graph_list", "arguments": []}),
            ),
        ],
    );
    for (index, tool) in WRITE_TOOLS.iter().take(3).enumerate() {
        assert_eq!(
            error(&responses[index]),
            format!("read_only: database is read-only; {tool} is unavailable")
        );
    }
    assert!(
        error(&responses[3]).starts_with("invalid_request: provide exactly one of node or edge")
    );
    assert!(error(&responses[4]).starts_with("invalid_request: provide exactly one"));
    assert!(error(&responses[5]).starts_with("invalid_request: "));
    assert!(error(&responses[5]).contains("extra"));
    assert_eq!(
        error(&responses[6]),
        "invalid_request: explain must be a boolean"
    );
    assert!(error(&responses[7]).starts_with("invalid_request: "));
    assert!(error(&responses[8]).starts_with("graph_limit_exceeds_ceiling: "));
    assert_eq!(
        error(&responses[9]),
        "invalid_input: invalid query: max_visited_nodes must be at least 1"
    );
    let node = ok(&responses[10]);
    assert_eq!(node["node"]["key"], "a");
    assert_eq!(node["node"]["id"], "0");
    assert_eq!(node["complete"], true);
    let plan = ok(&responses[11]);
    assert_eq!(plan["graph"], "kb");
    assert_eq!(plan["explain"]["query"]["operation"], "neighbors");
    assert_eq!(plan["explain"]["limits"]["max_hops"], 2);
    assert!(plan.get("edges").is_none(), "explain must not execute");
    assert_eq!(
        ok(&responses[12])["graphs"],
        json!([{"name": "kb", "node_count": 2, "edge_count": 1}])
    );
    // Non-object arguments stay a JSON-RPC error, as for vector tools.
    assert_eq!(responses[13]["error"]["code"], -32602);

    // The read-only session wrote nothing.
    let after = run(&path, false, &[call("vlite_graph_list", json!({}))]);
    assert_eq!(
        ok(&after[0])["graphs"],
        json!([{"name": "kb", "node_count": 2, "edge_count": 1}])
    );
}

#[test]
fn writable_sessions_create_drop_and_write_atomically() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.vlite");
    graph_fixture(&path);
    let responses = run(
        &path,
        true,
        &[
            call("vlite_graph_create", json!({"graph": "scratch"})),
            call("vlite_graph_create", json!({"graph": "scratch"})),
            call(
                "vlite_graph_write",
                json!({"graph": "scratch", "ops": [
                    {"op": "create_node", "key": "x"},
                    {"op": "create_edge", "source": "x", "relationship": "LINKS", "destination": "missing"}
                ]}),
            ),
            call("vlite_graph_list", json!({})),
            call(
                "vlite_graph_write",
                json!({"graph": "scratch", "ops": [{"op": "create_node", "key": "x"}]}),
            ),
            call("vlite_graph_drop", json!({"graph": "scratch"})),
            call(
                "vlite_graph_write",
                json!({"graph": "scratch", "ops": [{"op": "delete_node", "key": "x"}]}),
            ),
            call("vlite_graph_drop", json!({"graph": "scratch"})),
            call("vlite_graph_list", json!({})),
            call(
                "vlite_graph_write",
                json!({"graph": "scratch", "ops": [{"op": "bogus"}]}),
            ),
        ],
    );
    assert_eq!(
        ok(&responses[0]),
        &json!({"graph": {"name": "scratch", "node_count": 0, "edge_count": 0}})
    );
    assert!(
        error(&responses[1]).starts_with("graph_exists: "),
        "{}",
        error(&responses[1])
    );
    // The failed batch wrote nothing.
    assert!(
        error(&responses[2]).starts_with("graph_node_not_found: "),
        "{}",
        error(&responses[2])
    );
    assert_eq!(ok(&responses[3])["graphs"][0]["node_count"], 0);
    assert_eq!(ok(&responses[4])["outcomes"][0]["node"]["created"], true);
    assert!(
        error(&responses[5]).starts_with("graph_not_empty: "),
        "{}",
        error(&responses[5])
    );
    assert_eq!(ok(&responses[6])["outcomes"][0]["node_deleted"]["id"], "0");
    assert_eq!(ok(&responses[7]), &json!({"dropped": "scratch"}));
    assert_eq!(ok(&responses[8]), &json!({"graphs": []}));
    assert!(error(&responses[9]).starts_with("invalid_request: "));
}

fn check_step(task: &str, index: usize, step: &Value, response: &Value) {
    let context = format!("{task} step {index} ({})", step["tool"]);
    if let Some(code) = step.get("expect_error") {
        let code = code.as_str().unwrap();
        let message = error(response);
        assert!(
            message.starts_with(&format!("{code}: ")),
            "{context}: expected {code}, got {message}"
        );
        return;
    }
    let output = ok(response);
    for (pointer, expected) in step["expect"].as_object().into_iter().flatten() {
        assert_eq!(
            output.pointer(pointer),
            Some(expected),
            "{context}: {pointer} in {output}"
        );
    }
    for (pointer, expected) in step["expect_len"].as_object().into_iter().flatten() {
        let found = output.pointer(pointer).and_then(Value::as_array);
        assert_eq!(
            found.map(Vec::len),
            expected.as_u64().map(|length| length as usize),
            "{context}: length of {pointer} in {output}"
        );
    }
}

fn script(steps: &Value) -> Vec<(&'static str, Value)> {
    steps
        .as_array()
        .unwrap()
        .iter()
        .map(|step| call(step["tool"].as_str().unwrap(), step["arguments"].clone()))
        .collect()
}

#[test]
fn agent_tasks_fixture_runs_deterministically() {
    let fixture: Value = serde_json::from_str(&fixture_text("graph_agent/tasks.json")).unwrap();
    let tasks = fixture["tasks"].as_array().unwrap();
    let use_cases: Vec<&str> = tasks
        .iter()
        .map(|task| task["use_case"].as_str().unwrap())
        .collect();
    assert_eq!(use_cases, ["UC-001", "UC-002", "UC-003", "UC-004"]);
    for task in tasks {
        let id = task["id"].as_str().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("agent.vlite");
        graph_fixture(&path);
        if !task["seed"].is_null() {
            let names: Vec<&Value> = match &task["seed"] {
                Value::Array(names) => names.iter().collect(),
                name => vec![name],
            };
            for name in names {
                let seed = &fixture["seeds"][name.as_str().unwrap()];
                assert!(seed.is_array(), "{id}: unknown seed {name}");
                for (index, response) in run(&path, true, &script(seed)).iter().enumerate() {
                    assert_eq!(
                        response["result"]["isError"], false,
                        "{id} seed {name} {index}: {response}"
                    );
                }
            }
        }
        let writable = task["writable"].as_bool().unwrap();
        // Run twice: identical outputs on a fresh copy prove determinism.
        let mut transcripts = Vec::new();
        for _ in 0..2 {
            let copy = directory.path().join("copy.vlite");
            std::fs::copy(&path, &copy).unwrap();
            let responses = run(&copy, writable, &script(&task["steps"]));
            for (index, (step, response)) in task["steps"]
                .as_array()
                .unwrap()
                .iter()
                .zip(&responses)
                .enumerate()
            {
                check_step(id, index, step, response);
            }
            transcripts.push(
                responses
                    .into_iter()
                    .map(|mut response| {
                        // Elapsed time is the only nondeterministic field.
                        mask_elapsed(&mut response);
                        response
                    })
                    .collect::<Vec<_>>(),
            );
            std::fs::remove_file(&copy).unwrap();
        }
        assert_eq!(
            transcripts[0], transcripts[1],
            "{id}: nondeterministic output"
        );
    }
}

fn mask_elapsed(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for field in ["elapsed_ms", "pivot_ms", "expand_ms"] {
                if map.contains_key(field) {
                    map.insert(field.into(), json!(0));
                }
            }
            if let Some(Value::String(text)) = map.get("text")
                && let Ok(mut inner) = serde_json::from_str::<Value>(text)
            {
                mask_elapsed(&mut inner);
                map.insert("text".into(), json!(inner.to_string()));
            }
            for (_, child) in map.iter_mut() {
                mask_elapsed(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(mask_elapsed),
        _ => {}
    }
}

/// ITEM-052: each query text returns exactly what the typed tool it
/// translates to returns (output and plan), on the same data.
#[test]
fn query_text_matches_the_typed_tools() {
    let fixture: Value = serde_json::from_str(&fixture_text("graph_agent/tasks.json")).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("equivalence.vlite");
    graph_fixture(&path);
    for response in run(&path, true, &script(&fixture["seeds"]["services"])) {
        ok(&response);
    }
    let core = json!({"tier": {"type": "string", "value": "core"}});
    let pairs = [
        (
            json!({"query": "MATCH (n KEY \"api\") RETURN n"}),
            "vlite_graph_get",
            json!({"node": {"key": "api"}}),
        ),
        (
            json!({"query": "MATCH ()-[r KEY \"api-auth\"]-() RETURN r"}),
            "vlite_graph_get",
            json!({"edge": {"key": "api-auth"}}),
        ),
        (
            json!({"query": "MATCH (n:Service) WHERE n.tier = $tier RETURN n LIMIT 10", "params": core}),
            "vlite_graph_find_nodes",
            json!({"label": "Service", "conditions": [{"op": "eq", "field": "tier", "value": {"type": "string", "value": "core"}}], "limit": 10}),
        ),
        (
            json!({"query": "MATCH (a KEY \"db\")<-[r:DEPENDS_ON]-(b) RETURN r, b"}),
            "vlite_graph_neighbors",
            json!({"node": {"key": "db"}, "direction": "in", "types": ["DEPENDS_ON"]}),
        ),
        (
            json!({"query": "MATCH p = (a KEY \"api\")-[:CALLS|DEPENDS_ON*1..2]->(b:Database) RETURN b, p"}),
            "vlite_graph_traverse",
            json!({"start": [{"key": "api"}], "types": ["CALLS", "DEPENDS_ON"], "return_filter": {"labels": ["Database"]}, "include_paths": true, "limits": {"max_hops": 2}}),
        ),
        (
            json!({"query": "MATCH p = SHORTEST PATH (a KEY \"api\")-[*1..2]->(b KEY \"db\") RETURN p"}),
            "vlite_graph_shortest_path",
            json!({"source": {"key": "api"}, "destination": {"key": "db"}, "limits": {"max_hops": 2}}),
        ),
    ];
    let mut requests = Vec::new();
    for (text, tool, typed) in &pairs {
        for explain in [false, true] {
            let mut text = text.clone();
            text["graph"] = json!("services");
            text["explain"] = json!(explain);
            let mut typed = typed.clone();
            typed["graph"] = json!("services");
            typed["explain"] = json!(explain);
            requests.push(call("vlite_graph_query", text));
            requests.push(call(tool, typed));
        }
    }
    let mut responses = run(&path, false, &requests);
    responses.iter_mut().for_each(mask_elapsed);
    for (index, pair) in responses.chunks(2).enumerate() {
        let (text, typed) = (ok(&pair[0]), ok(&pair[1]));
        assert_eq!(text, typed, "pair {} explain={}", index / 2, index % 2 == 1);
        assert_eq!(text["graph"], "services");
    }
    // The non-explain outputs are real results, not empty reads.
    assert_eq!(ok(&responses[0])["node"]["key"], "api");
    assert_eq!(ok(&responses[8])["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(ok(&responses[20])["shortest_path"]["outcome"], "found");
}

/// AC-018: schema writes through `vlite_graph_put_schema` and the batch
/// `put_schema` / `ensure_edge` ops.
#[test]
fn writable_sessions_declare_schemas_and_ensure_edges() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.vlite");
    graph_fixture(&path);
    let owns = json!({"labels": {"Team": {}, "Service": {}},
                      "relationships": {"OWNS": {"endpoints": [{"source": "Team", "destination": "Service"}], "multiplicity": "unique_triple"}}});
    let responses = run(
        &path,
        true,
        &[
            call("vlite_graph_create", json!({"graph": "kb"})),
            call(
                "vlite_graph_write",
                json!({"graph": "kb", "ops": [
                    {"op": "create_node", "key": "t", "labels": ["Team"]},
                    {"op": "create_node", "key": "s", "labels": ["Service"]}
                ]}),
            ),
            call(
                "vlite_graph_write",
                json!({"graph": "kb", "ops": [
                    {"op": "ensure_edge", "source": "t", "relationship": "OWNS", "destination": "s"}
                ]}),
            ),
            call(
                "vlite_graph_put_schema",
                json!({"graph": "kb", "schema": owns}),
            ),
            call(
                "vlite_graph_write",
                json!({"graph": "kb", "ops": [
                    {"op": "ensure_edge", "source": "t", "relationship": "OWNS", "destination": "s"},
                    {"op": "ensure_edge", "source": "t", "relationship": "OWNS", "destination": "s"}
                ]}),
            ),
            call(
                "vlite_graph_write",
                json!({"graph": "kb", "ops": [{"op": "put_schema", "schema": {"labels": {"Team": {"properties": {"name": {"type": "string", "required": true}}}}}}]}),
            ),
            call(
                "vlite_graph_write",
                json!({"graph": "kb", "ops": [{"op": "put_schema", "schema": {}}]}),
            ),
            call(
                "vlite_graph_put_schema",
                json!({"graph": "kb", "schema": {"labels": {"Team": {"properties": {"name": {"type": "text"}}}}}}),
            ),
            call(
                "vlite_graph_put_schema",
                json!({"graph": "missing", "schema": {}}),
            ),
            call("vlite_graph_schema", json!({"graph": "kb"})),
        ],
    );
    ok(&responses[0]);
    ok(&responses[1]);
    // Index as if the graph creation step were absent.
    let responses = &responses[1..];
    assert!(
        error(&responses[1]).starts_with("invalid_input: ")
            && error(&responses[1]).contains("UniqueTriple"),
        "ensure_edge needs a unique_triple declaration: {}",
        error(&responses[1])
    );
    let revision = ok(&responses[2]);
    assert_eq!(revision["graph"], "kb");
    assert!(
        revision["revision"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .is_ok()
    );
    let ensured = ok(&responses[3]);
    assert_eq!(ensured["outcomes"][0]["edge"]["created"], true);
    assert_eq!(ensured["outcomes"][1]["edge"]["created"], false);
    assert_eq!(
        ensured["outcomes"][0]["edge"]["id"],
        ensured["outcomes"][1]["edge"]["id"]
    );
    assert!(
        error(&responses[4]).starts_with("graph_schema_violation: "),
        "{}",
        error(&responses[4])
    );
    assert_eq!(ok(&responses[5])["outcomes"], json!(["schema"]));
    assert!(error(&responses[6]).starts_with("invalid_request: "));
    assert!(error(&responses[7]).starts_with("graph_not_found: "));
    let schema = ok(&responses[8]);
    assert_eq!(
        schema["declared"],
        json!({"labels": {}, "relationships": {}})
    );
    assert_eq!(schema["observed"]["relationships"][0]["name"], "OWNS");
    assert_eq!(schema["observed"]["relationships"][0]["count"], 1);
}
