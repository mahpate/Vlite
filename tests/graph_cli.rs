//! FOLLOWUP-001: the opt-in `vlite-graph-cli` binary (`graph` + `json`).
//!
//! Every test drives the real binary over real files and checks the
//! machine-readable contract: one JSON object on stdout for success, one
//! `{"error":{"code","message"}}` object on stderr and a nonzero exit for
//! failure, u64 identifiers as decimal strings, read commands that never write
//! and writes that need `--writable`.
#![cfg(all(feature = "graph", feature = "json"))]

mod cli_support;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use cli_support::Fixture;
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_vlite-graph-cli");

struct Run {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl Run {
    fn value(&self) -> Value {
        assert!(
            self.ok,
            "expected success; stdout={} stderr={}",
            self.stdout, self.stderr
        );
        assert!(self.stderr.is_empty(), "stderr={}", self.stderr);
        let lines: Vec<&str> = self.stdout.lines().collect();
        assert_eq!(lines.len(), 1, "one JSON line expected: {}", self.stdout);
        serde_json::from_str(lines[0]).unwrap()
    }

    fn error_code(&self) -> String {
        assert!(!self.ok, "expected failure; stdout={}", self.stdout);
        assert!(self.stdout.is_empty(), "stdout={}", self.stdout);
        let lines: Vec<&str> = self.stderr.lines().collect();
        assert_eq!(lines.len(), 1, "one JSON line expected: {}", self.stderr);
        let value: Value = serde_json::from_str(lines[0]).unwrap();
        let error = value["error"].as_object().expect("error object");
        assert!(error["message"].as_str().is_some_and(|m| !m.is_empty()));
        assert_eq!(error.len(), 2, "{value}");
        error["code"].as_str().unwrap().to_owned()
    }
}

fn run_with(db: &Path, args: &[&str], stdin: Option<&str>) -> Run {
    let mut command = Command::new(BIN);
    command
        .arg("--db")
        .arg(db)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    if let Some(text) = stdin {
        input.write_all(text.as_bytes()).unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    Run {
        ok: output.status.success(),
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

fn run(db: &Path, args: &[&str]) -> Run {
    run_with(db, args, None)
}

fn request(value: Value) -> String {
    value.to_string()
}

/// A graph-enabled file with graph `kb`: nodes a -> b -> c (edge keys ab and
/// bc), d isolated, and record u64::MAX attached to `a`.
fn seeded(fixture: &Fixture) -> std::path::PathBuf {
    let db = fixture.path("graph.vlite");
    let created = run(
        &db,
        &["--writable", "init", &request(json!({"dimensions": 2}))],
    )
    .value();
    assert_eq!(created, json!({"dimensions": 2, "graph_enabled": true}));
    let graph = run(
        &db,
        &[
            "--writable",
            "create-graph",
            &request(json!({"graph": "kb"})),
        ],
    )
    .value();
    assert_eq!(
        graph,
        json!({"graph": {"name": "kb", "node_count": 0, "edge_count": 0}})
    );
    let batch = json!({"graph": "kb", "ops": [
        {"op": "create_node", "key": "a", "labels": ["Service"],
         "properties": {"tier": {"type": "integer", "value": "1"}}},
        {"op": "create_node", "key": "b", "labels": ["Service"]},
        {"op": "create_node", "key": "c", "labels": ["Database"]},
        {"op": "upsert_node", "key": "d", "labels": ["Team"], "expected": "absent"},
        {"op": "create_edge", "key": "ab", "source": "a", "relationship": "CALLS",
         "destination": "b", "properties": {"weight": {"type": "float", "value": 0.5}}},
        {"op": "create_edge", "key": "bc", "source": "b", "relationship": "READS",
         "destination": "c"},
        {"op": "put_vector", "collection": "default", "id": "18446744073709551615",
         "vector": [1.0, 2.0], "payload": "evidence",
         "metadata": {"kind": {"type": "string", "value": "doc"}}},
        {"op": "attach", "node": "a", "collection": "default",
         "record": "18446744073709551615"}
    ]});
    let result = run(&db, &["--writable", "batch", &request(batch)]).value();
    assert_eq!(result["graph"], "kb");
    let revision = result["revision"].as_str().expect("revision string");
    let outcomes = result["outcomes"].as_array().unwrap();
    assert_eq!(outcomes.len(), 8);
    assert_eq!(
        outcomes[0],
        json!({"node": {"id": "0", "created": true, "revision": revision}})
    );
    assert_eq!(outcomes[4]["edge"]["created"], true);
    assert_eq!(outcomes[6], json!("vector"));
    assert_eq!(outcomes[7], json!({"attached": {"node": "0"}}));
    db
}

#[test]
fn writes_and_reads_round_trip_through_typed_json() {
    let fixture = Fixture::new();
    let db = seeded(&fixture);

    let listed = run(&db, &["graphs"]).value();
    assert_eq!(
        listed,
        json!({"graphs": [{"name": "kb", "node_count": 4, "edge_count": 2}]})
    );

    let node = run(
        &db,
        &[
            "get-node",
            &request(json!({"graph": "kb", "node": {"key": "a"}})),
        ],
    )
    .value();
    assert_eq!(node["graph"], "kb");
    assert_eq!(node["complete"], true);
    assert_eq!(node["truncation"], json!([]));
    assert!(node["view_sequence"].is_string());
    assert_eq!(node["cursor"], Value::Null);
    assert_eq!(node["node"]["id"], "0");
    assert_eq!(node["node"]["key"], "a");
    assert_eq!(node["node"]["labels"], json!(["Service"]));
    assert_eq!(
        node["node"]["properties"],
        json!({"tier": {"type": "integer", "value": "1"}})
    );
    assert!(node["node"]["revision"].is_string());
    assert!(node["stats"]["indexes"].is_array());

    // IDs accept a canonical decimal string or a JSON number.
    let by_id = run(
        &db,
        &[
            "get-node",
            &request(json!({"graph": "kb", "node": {"id": "1"}})),
        ],
    )
    .value();
    assert_eq!(by_id["node"]["key"], "b");
    let missing = run(
        &db,
        &[
            "get-node",
            &request(json!({"graph": "kb", "node": {"id": 99}})),
        ],
    )
    .value();
    assert_eq!(missing["node"], Value::Null);

    let edge = run(
        &db,
        &[
            "get-edge",
            &request(json!({"graph": "kb", "edge": {"key": "ab"}, "projection": "none"})),
        ],
    )
    .value();
    assert_eq!(edge["edge"]["key"], "ab");
    assert_eq!(edge["edge"]["relationship"], "CALLS");
    assert_eq!(edge["edge"]["source"], "0");
    assert_eq!(edge["edge"]["source_key"], "a");
    assert_eq!(edge["edge"]["target_key"], "b");
    assert_eq!(edge["edge"]["properties"], json!({}));

    // Paged find-nodes: a two-node page with a cursor, then the rest.
    let first = run(
        &db,
        &[
            "find-nodes",
            &request(json!({"graph": "kb", "label": "Service", "limit": 1})),
        ],
    )
    .value();
    assert_eq!(first["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(first["complete"], false);
    assert_eq!(first["truncation"], json!(["output_limit"]));
    let cursor = first["cursor"].as_str().expect("hex cursor").to_owned();
    let rest = run(
        &db,
        &[
            "find-nodes",
            &request(json!({"graph": "kb", "label": "Service", "limit": 1, "cursor": cursor})),
        ],
    )
    .value();
    assert_eq!(rest["nodes"][0]["key"], "b");
    let filtered = run(
        &db,
        &[
            "find-nodes",
            &request(json!({"graph": "kb", "conditions": [
                {"op": "eq", "field": "tier", "value": {"type": "integer", "value": "1"}}
            ], "projection": {"only": ["missing"]}})),
        ],
    )
    .value();
    assert_eq!(filtered["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["nodes"][0]["properties"], json!({}));

    let neighbors = run(
        &db,
        &[
            "neighbors",
            &request(json!({"graph": "kb", "node": {"key": "b"}, "direction": "both"})),
        ],
    )
    .value();
    let keys: Vec<&str> = neighbors["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["a", "c"]);
    assert_eq!(neighbors["edges"].as_array().unwrap().len(), 2);

    let traversal = run(
        &db,
        &[
            "traverse",
            &request(
                json!({"graph": "kb", "start": [{"key": "a"}], "include_paths": true,
                            "limits": {"max_hops": 2}}),
            ),
        ],
    )
    .value();
    let depths: Vec<(String, u64)> = traversal["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["node"]["key"].as_str().unwrap().to_owned(),
                item["depth"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(depths, [("a".into(), 0), ("b".into(), 1), ("c".into(), 2)]);
    assert!(
        traversal["paths"]
            .as_array()
            .unwrap()
            .contains(&json!({"nodes": ["0", "1", "2"], "edges": ["0", "1"]}))
    );

    let path = run(
        &db,
        &[
            "shortest-path",
            &request(json!({"graph": "kb", "source": {"key": "a"}, "destination": {"key": "c"}})),
        ],
    )
    .value();
    assert_eq!(path["shortest_path"]["outcome"], "found");
    assert_eq!(
        path["shortest_path"]["path"],
        json!({"nodes": ["0", "1", "2"], "edges": ["0", "1"]})
    );
    let none = run(
        &db,
        &[
            "shortest-path",
            &request(json!({"graph": "kb", "source": {"key": "a"}, "destination": {"key": "d"}})),
        ],
    )
    .value();
    assert_eq!(
        none["shortest_path"],
        json!({"outcome": "no_path_within_hops"})
    );

    // --explain plans the same request without executing it.
    let explained = run(
        &db,
        &[
            "--explain",
            "neighbors",
            &request(json!({"graph": "kb", "node": {"key": "a"}, "types": ["CALLS"]})),
        ],
    )
    .value();
    assert_eq!(explained["graph"], "kb");
    let explain = &explained["explain"];
    assert_eq!(explain["query"]["operation"], "neighbors");
    assert_eq!(explain["query"]["types"], json!(["CALLS"]));
    assert_eq!(explain["query"]["limit"], 200);
    assert_eq!(explain["limits"]["max_hops"], 2);
    assert_eq!(explain["projection"], "all");
    assert!(
        explain["indexes"]
            .as_array()
            .unwrap()
            .contains(&json!({"adjacency_range_scan": {"direction": "out", "typed": true}}))
    );
}

#[test]
fn batch_vector_mutations_and_detach_keep_u64_identifiers_exact() {
    let fixture = Fixture::new();
    let db = seeded(&fixture);
    let batch = json!({"graph": "kb", "ops": [
        {"op": "patch_node", "key": "a", "add_labels": ["Critical"],
         "set": {"owner": {"type": "string", "value": "ops"}}, "unset": ["tier"]},
        {"op": "patch_edge", "edge": {"key": "ab"}, "unset": ["weight"]},
        {"op": "detach", "node": "a", "collection": "default"},
        {"op": "delete_vector", "collection": "default", "id": "18446744073709551615"},
        {"op": "delete_edge", "edge": {"id": "1"}},
        {"op": "delete_node", "key": "d"}
    ]});
    let result = run(&db, &["--writable", "batch", &request(batch)]).value();
    let outcomes = result["outcomes"].as_array().unwrap();
    assert_eq!(
        outcomes[2],
        json!({"detached": {"node": "0", "record": "18446744073709551615"}})
    );
    assert_eq!(outcomes[3], json!("vector"));
    assert_eq!(outcomes[4], json!({"edge_deleted": {"id": "1"}}));
    assert_eq!(outcomes[5], json!({"node_deleted": {"id": "3"}}));

    let node = run(
        &db,
        &[
            "get-node",
            &request(json!({"graph": "kb", "node": {"key": "a"}})),
        ],
    )
    .value();
    assert_eq!(node["node"]["labels"], json!(["Critical", "Service"]));
    assert_eq!(
        node["node"]["properties"],
        json!({"owner": {"type": "string", "value": "ops"}})
    );
    let revision = node["node"]["revision"].as_str().unwrap().to_owned();

    // Compare-and-set preconditions round-trip the revision string.
    let stale = json!({"graph": "kb", "ops": [
        {"op": "delete_edge", "edge": {"key": "ab"}, "expected": {"revision": "1"}}
    ]});
    let failed = run(&db, &["--writable", "batch", &request(stale)]);
    assert_eq!(failed.error_code(), "precondition_failed");
    let cas = json!({"graph": "kb", "ops": [
        {"op": "upsert_node", "key": "a", "labels": ["Service"],
         "expected": {"revision": revision}}
    ]});
    let updated = run(&db, &["--writable", "batch", &request(cas)]).value();
    assert_eq!(updated["outcomes"][0]["node"]["created"], false);

    // The vector-only collection state matches what the batch committed.
    let db = vlite::Database::open_read_only(&db).unwrap();
    assert!(db.get(u64::MAX).unwrap().is_none());
}

#[test]
fn failed_batches_and_requests_write_nothing() {
    let fixture = Fixture::new();
    let db = seeded(&fixture);
    let before = std::fs::read(&db).unwrap();

    let bad_reference = json!({"graph": "kb", "ops": [
        {"op": "create_node", "key": "e"},
        {"op": "create_edge", "source": "e", "relationship": "CALLS", "destination": "zz"}
    ]});
    let failed = run(&db, &["--writable", "batch", &request(bad_reference)]);
    assert_eq!(failed.error_code(), "graph_node_not_found");

    let attached = json!({"graph": "kb", "ops": [
        {"op": "delete_vector", "collection": "default", "id": "18446744073709551615"}
    ]});
    let failed = run(&db, &["--writable", "batch", &request(attached)]);
    assert_eq!(failed.error_code(), "graph_attached_record");

    // Unknown fields and unknown operations are rejected, never dropped.
    for invalid in [
        json!({"graph": "kb", "ops": [{"op": "create_node", "key": "e", "colour": "red"}]}),
        json!({"graph": "kb", "ops": [{"op": "merge_node", "key": "e"}]}),
        json!({"graph": "kb", "ops": [], "dry_run": true}),
        json!({"graph": "kb", "ops": [{"op": "attach", "node": "a", "collection": "default",
                                        "record": "018446744073709551615"}]}),
        json!({"graph": "kb", "ops": [{"op": "attach", "node": "a", "collection": "default",
                                        "record": -1}]}),
    ] {
        let failed = run(&db, &["--writable", "batch", &request(invalid)]);
        assert_eq!(failed.error_code(), "invalid_request");
    }
    let failed = run(&db, &["--writable", "batch", "{not json"]);
    assert_eq!(failed.error_code(), "invalid_request");
    // Repeated property or metadata keys are rejected, not last-wins.
    let integer = r#"{"type":"integer","value":"1"}"#;
    for duplicate in [
        format!(
            r#"{{"graph":"kb","ops":[{{"op":"create_node","key":"e","properties":{{"x":{integer},"x":{integer}}}}}]}}"#
        ),
        format!(
            r#"{{"graph":"kb","ops":[{{"op":"patch_node","key":"a","set":{{"x":{integer},"x":{integer}}}}}]}}"#
        ),
        format!(
            r#"{{"graph":"kb","ops":[{{"op":"put_vector","collection":"default","id":"9","vector":[1,0],"metadata":{{"x":{integer},"x":{integer}}}}}]}}"#
        ),
    ] {
        let failed = run(&db, &["--writable", "batch", &duplicate]);
        assert_eq!(failed.error_code(), "invalid_request", "{duplicate}");
    }

    // Limits above the ceiling fail; they are never clamped.
    let failed = run(
        &db,
        &[
            "traverse",
            &request(json!({"graph": "kb", "start": [{"key": "a"}], "limits": {"max_hops": 5}})),
        ],
    );
    assert_eq!(failed.error_code(), "graph_limit_exceeds_ceiling");
    let failed = run(
        &db,
        &[
            "get-node",
            &request(json!({"graph": "nope", "node": {"key": "a"}})),
        ],
    );
    assert_eq!(failed.error_code(), "graph_not_found");
    let failed = run(
        &db,
        &[
            "find-nodes",
            &request(json!({"graph": "kb", "cursor": "zz"})),
        ],
    );
    assert_eq!(failed.error_code(), "invalid_request");

    assert_eq!(before, std::fs::read(&db).unwrap());
}

#[test]
fn access_mode_is_explicit_and_reads_never_write() {
    let fixture = Fixture::new();
    let db = seeded(&fixture);
    let before = std::fs::read(&db).unwrap();

    // Writes need --writable; reads refuse it.
    let failed = run(&db, &["create-graph", &request(json!({"graph": "other"}))]);
    assert_eq!(failed.error_code(), "usage");
    let failed = run(&db, &["--writable", "graphs"]);
    assert_eq!(failed.error_code(), "usage");
    let failed = run(
        &db,
        &[
            "--explain",
            "batch",
            &request(json!({"graph": "kb", "ops": []})),
        ],
    );
    assert_eq!(failed.error_code(), "usage");
    for args in [
        &["frobnicate"][..],
        &[][..],
        &["graphs", "{}", "extra"][..],
        &["--bogus", "graphs"][..],
    ] {
        assert_eq!(run(&db, args).error_code(), "usage", "{args:?}");
    }

    // Reads take the shared lock: they coexist with another reader and are
    // refused while an exclusive writer owns the file.
    let reader = vlite::Database::open_read_only(&db).unwrap();
    assert_eq!(run(&db, &["graphs"]).value()["graphs"][0]["name"], "kb");
    let failed = run(
        &db,
        &[
            "--writable",
            "create-graph",
            &request(json!({"graph": "x"})),
        ],
    );
    assert_eq!(failed.error_code(), "locked");
    drop(reader);
    let writer = vlite::Database::open(&db).unwrap();
    assert_eq!(run(&db, &["graphs"]).error_code(), "locked");
    drop(writer);

    // The request may come from stdin.
    let piped = run_with(
        &db,
        &["get-node", "-"],
        Some(r#"{"graph":"kb","node":{"key":"c"}}"#),
    )
    .value();
    assert_eq!(piped["node"]["labels"], json!(["Database"]));

    assert_eq!(before, std::fs::read(&db).unwrap());
}

#[test]
fn files_are_never_graph_enabled_implicitly() {
    let fixture = Fixture::new();
    let vector = fixture.path("vector.vlite");
    cli_support::database(&vector);
    let before = std::fs::read(&vector).unwrap();
    assert_eq!(run(&vector, &["graphs"]).error_code(), "graphs_not_enabled");
    let failed = run(
        &vector,
        &[
            "--writable",
            "create-graph",
            &request(json!({"graph": "kb"})),
        ],
    );
    assert_eq!(failed.error_code(), "graphs_not_enabled");
    let failed = run(
        &vector,
        &[
            "--writable",
            "batch",
            &request(json!({"graph": "kb", "ops": []})),
        ],
    );
    assert_eq!(failed.error_code(), "graphs_not_enabled");
    // init never converts or overwrites an existing file.
    let failed = run(
        &vector,
        &["--writable", "init", &request(json!({"dimensions": 2}))],
    );
    assert_eq!(failed.error_code(), "io_error");
    assert_eq!(before, std::fs::read(&vector).unwrap());

    // Graph lifecycle on a new file: create, refuse a non-empty drop, drop.
    let db = fixture.path("graph.vlite");
    run(
        &db,
        &["--writable", "init", &request(json!({"dimensions": 3}))],
    )
    .value();
    run(
        &db,
        &[
            "--writable",
            "create-graph",
            &request(json!({"graph": "g"})),
        ],
    )
    .value();
    let failed = run(
        &db,
        &[
            "--writable",
            "create-graph",
            &request(json!({"graph": "g"})),
        ],
    );
    assert_eq!(failed.error_code(), "graph_exists");
    let failed = run(
        &db,
        &[
            "--writable",
            "create-graph",
            &request(json!({"graph": "Bad"})),
        ],
    );
    assert_eq!(failed.error_code(), "invalid_graph_name");
    run(
        &db,
        &[
            "--writable",
            "batch",
            &request(json!({"graph": "g", "ops": [{"op": "create_node", "key": "n"}]})),
        ],
    )
    .value();
    let failed = run(
        &db,
        &["--writable", "drop-graph", &request(json!({"graph": "g"}))],
    );
    assert_eq!(failed.error_code(), "graph_not_empty");
    run(
        &db,
        &[
            "--writable",
            "batch",
            &request(json!({"graph": "g", "ops": [{"op": "delete_node", "key": "n"}]})),
        ],
    )
    .value();
    let dropped = run(
        &db,
        &["--writable", "drop-graph", &request(json!({"graph": "g"}))],
    )
    .value();
    assert_eq!(dropped, json!({"dropped": "g"}));
    assert_eq!(run(&db, &["graphs"]).value(), json!({"graphs": []}));
    let empty = run(
        &db,
        &[
            "--writable",
            "batch",
            &request(json!({"graph": "kb", "ops": []})),
        ],
    );
    assert_eq!(empty.error_code(), "graph_not_found");
}

#[test]
fn query_text_runs_exactly_the_equivalent_typed_read() {
    let fixture = Fixture::new();
    let db = seeded(&fixture);
    let pairs = [
        (
            json!({"graph": "kb", "query": "MATCH (n KEY $k) RETURN n",
                   "params": {"k": {"type": "string", "value": "a"}}}),
            "get-node",
            json!({"graph": "kb", "node": {"key": "a"}}),
        ),
        (
            json!({"graph": "kb", "query": "MATCH ()-[r KEY \"ab\"]-() RETURN r.weight"}),
            "get-edge",
            json!({"graph": "kb", "edge": {"key": "ab"}, "projection": {"only": ["weight"]}}),
        ),
        (
            json!({"graph": "kb", "query": "MATCH (n:Service) WHERE n.tier >= $t RETURN n LIMIT 5",
                   "params": {"t": {"type": "integer", "value": "1"}}}),
            "find-nodes",
            json!({"graph": "kb", "label": "Service", "limit": 5, "conditions": [
                {"op": "range", "field": "tier", "min": {"type": "integer", "value": "1"}}
            ]}),
        ),
        (
            json!({"graph": "kb", "query": "MATCH (a KEY \"b\")-[r]-(b) RETURN r, b"}),
            "neighbors",
            json!({"graph": "kb", "node": {"key": "b"}, "direction": "both"}),
        ),
        (
            json!({"graph": "kb", "query": "MATCH p = (a KEY \"a\")-[:CALLS|READS*1..3]->(b) RETURN b, p LIMIT 10"}),
            "traverse",
            json!({"graph": "kb", "start": [{"key": "a"}], "types": ["CALLS", "READS"],
                   "include_paths": true,
                   "limits": {"max_hops": 3, "max_output_nodes": 10}}),
        ),
        (
            json!({"graph": "kb", "query": "MATCH p = SHORTEST PATH (a KEY \"a\")-[*..2]->(b KEY \"c\") RETURN p"}),
            "shortest-path",
            json!({"graph": "kb", "source": {"key": "a"}, "destination": {"key": "c"}}),
        ),
    ];
    for (text, command, typed) in pairs {
        let expected = run(&db, &[command, &request(typed.clone())]).value();
        let actual = run(&db, &["query", &request(text.clone())]).value();
        assert_eq!(actual, expected, "{text}");
        let expected = run(&db, &["--explain", command, &request(typed)]).value();
        let actual = run(&db, &["--explain", "query", &request(text.clone())]).value();
        assert_eq!(actual, expected, "{text}");
    }
    let path = run(
        &db,
        &[
            "query",
            &request(json!({"graph": "kb",
                "query": "MATCH p = SHORTEST PATH (a KEY \"a\")-[*..2]->(b KEY \"c\") RETURN p"})),
        ],
    )
    .value();
    assert_eq!(
        path["shortest_path"]["path"],
        json!({"nodes": ["0", "1", "2"], "edges": ["0", "1"]})
    );

    // Typed failures: text errors before the file is touched, then the
    // ordinary read errors.
    let failures = [
        (
            json!({"graph": "kb", "query": "MATCH (n"}),
            "graph_query_syntax",
        ),
        (
            json!({"graph": "kb", "query": "SEARCH docs NEAR $q"}),
            "graph_query_unsupported",
        ),
        (
            json!({"graph": "kb", "query": "MATCH (n KEY $k) RETURN n"}),
            "graph_query_parameter",
        ),
        (
            json!({"graph": "kb", "query": "MATCH (a KEY \"a\")-[*1..9]->(b) RETURN b"}),
            "graph_query_limit",
        ),
        (
            json!({"graph": "nope", "query": "MATCH (n KEY \"a\") RETURN n"}),
            "graph_not_found",
        ),
        (
            json!({"graph": "kb", "query": "MATCH (n KEY \"a\") RETURN n",
                   "limits": {"max_hops": 9}}),
            "graph_limit_exceeds_ceiling",
        ),
        (
            json!({"graph": "kb", "query": "MATCH (n KEY \"a\") RETURN n", "projection": "none"}),
            "invalid_request",
        ),
    ];
    for (text, code) in failures {
        assert_eq!(
            run(&db, &["query", &request(text.clone())]).error_code(),
            code,
            "{text}"
        );
    }
    let missing = fixture.path("missing.vlite");
    assert_eq!(
        run(
            &missing,
            &[
                "query",
                &request(json!({"graph": "kb", "query": "MATCH (n"}))
            ]
        )
        .error_code(),
        "graph_query_syntax"
    );
    assert!(!missing.exists());
    assert_eq!(
        run(
            &db,
            &[
                "--writable",
                "query",
                &request(json!({"graph": "kb", "query": "MATCH (n KEY \"a\") RETURN n"}))
            ]
        )
        .error_code(),
        "usage"
    );
}

#[test]
fn help_lists_the_contract() {
    let output = Command::new(BIN).arg("--help").output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    for word in [
        "--writable",
        "--explain",
        "init",
        "graphs",
        "create-graph",
        "drop-graph",
        "batch",
        "get-node",
        "get-edge",
        "find-nodes",
        "neighbors",
        "traverse",
        "shortest-path",
        "query",
    ] {
        assert!(text.contains(word), "{word}");
    }
}
