use std::{
    io::Write,
    process::{Command, Stdio},
};

use serde_json::{Value, json};
use vlite::Database;

fn fixture(path: &std::path::Path) {
    let mut db = Database::create(path, 2).unwrap();
    db.put(1, &[0.0, 0.0], b"origin").unwrap();
    db.put(2, &[1.0, 1.0], b"near").unwrap();
    db.put(3, &[2.0, 2.0], b"far").unwrap();
    db.close().unwrap();
}

fn run(path: &std::path::Path, writable: bool, lines: &[&str]) -> Vec<Value> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vlite-mcp"));
    command.args(["--db", path.to_str().unwrap()]);
    command.arg(if writable {
        "--writable"
    } else {
        "--read-only"
    });
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        for line in lines {
            writeln!(stdin, "{line}").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn request(id: usize, method: &str, params: Value) -> String {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string()
}

fn response(responses: &[Value], id: usize) -> &Value {
    responses.iter().find(|value| value["id"] == id).unwrap()
}

#[test]
fn mcp_stdio_lifecycle_tools_and_protocol_errors() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mcp.vlite");
    fixture(&path);
    let requests = [
        request(
            1,
            "initialize",
            json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1"}}),
        ),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string(),
        request(2, "ping", json!({})),
        request(3, "tools/list", json!({})),
        request(
            4,
            "tools/call",
            json!({"name":"vlite_search","arguments":{"vector":[0.9,0.9],"k":2}}),
        ),
        request(
            5,
            "tools/call",
            json!({"name":"vlite_get","arguments":{"id":"2","include_vector":true}}),
        ),
        request(
            8,
            "tools/call",
            json!({"name":"vlite_build_index","arguments":{}}),
        ),
        request(
            9,
            "tools/call",
            json!({"name":"vlite_index_status","arguments":{}}),
        ),
        "{".into(),
        request(6, "not/a/method", json!({})),
        request(
            7,
            "tools/call",
            json!({"name":"vlite_search","arguments":{"vector":[1]}}),
        ),
    ];
    let lines: Vec<&str> = requests.iter().map(String::as_str).collect();
    let responses = run(&path, false, &lines);
    assert_eq!(
        response(&responses, 1)["result"]["protocolVersion"],
        "2025-03-26"
    );
    assert_eq!(response(&responses, 2)["result"], json!({}));
    let names: Vec<&str> = response(&responses, 3)["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(!names.contains(&"vlite_upsert"));
    assert!(!names.contains(&"vlite_delete"));
    let search = response(&responses, 4)["result"]["structuredContent"].clone();
    let ids: Vec<&str> = search["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["2", "1"]);
    let get = &response(&responses, 5)["result"]["structuredContent"];
    assert_eq!(get["record"]["id"], "2");
    assert_eq!(get["record"]["payload"], "near");
    assert_eq!(get["record"]["vector"], json!([1.0, 1.0]));
    assert_eq!(
        response(&responses, 8)["result"]["structuredContent"]["built"],
        true
    );
    assert_eq!(
        response(&responses, 9)["result"]["structuredContent"]["ready"],
        true
    );
    assert_eq!(
        responses
            .iter()
            .find(|value| value["error"]["code"] == -32700)
            .unwrap()["error"]["code"],
        -32700
    );
    assert_eq!(response(&responses, 6)["error"]["code"], -32601);
    assert_eq!(response(&responses, 7)["result"]["isError"], true);

    // Raw requests preserve repeated keys, which Value would collapse before sending.
    let responses = run(
        &path,
        true,
        &[
            r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"vlite_get","arguments":{"id":"1","id":"2"}}}"#,
            r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"vlite_graph_create","arguments":{"graph":"first","graph":"last"}}}"#,
            &request(
                12,
                "tools/call",
                json!({"name":"vlite_graph_list","arguments":{}}),
            ),
        ],
    );
    assert_eq!(response(&responses, 10)["result"]["isError"], false);
    assert_eq!(
        response(&responses, 10)["result"]["structuredContent"],
        json!({"record":{"id":"2","metadata":{},"payload":"near"}})
    );
    assert_eq!(response(&responses, 11)["result"]["isError"], false);
    assert_eq!(
        response(&responses, 11)["result"]["structuredContent"],
        json!({"graph":{"name":"last","node_count":0,"edge_count":0}})
    );
    assert_eq!(response(&responses, 12)["result"]["isError"], false);
    assert_eq!(
        response(&responses, 12)["result"]["structuredContent"],
        json!({"graphs":[{"name":"last","node_count":0,"edge_count":0}]})
    );
}

#[test]
fn read_only_mutation_fails_and_writable_changes_persist() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mcp.vlite");
    fixture(&path);
    let responses = run(
        &path,
        false,
        &[&request(
            1,
            "tools/call",
            json!({"name":"vlite_upsert","arguments":{"id":"4","vector":[4,4]}}),
        )],
    );
    assert_eq!(response(&responses, 1)["result"]["isError"], true);

    let responses = run(
        &path,
        true,
        &[
            &request(2, "tools/list", json!({})),
            &request(
                3,
                "tools/call",
                json!({"name":"vlite_upsert","arguments":{"id":"4","vector":[4,4],"payload":"added"}}),
            ),
            &request(
                4,
                "tools/call",
                json!({"name":"vlite_delete","arguments":{"id":"1"}}),
            ),
            &request(
                5,
                "tools/call",
                json!({"name":"vlite_upsert","arguments":{"id":"5","vector":[5,5],"payload_base64":"AAEC"}}),
            ),
            &request(
                6,
                "tools/call",
                json!({"name":"vlite_upsert","arguments":{"id":"6","vector":[6,6],"payload_base64":"AP/+gA=="}}),
            ),
        ],
    );
    let names: Vec<&str> = response(&responses, 2)["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"vlite_upsert"));
    assert!(names.contains(&"vlite_delete"));
    assert_eq!(response(&responses, 3)["result"]["isError"], false);
    assert_eq!(response(&responses, 4)["result"]["isError"], false);
    assert_eq!(response(&responses, 5)["result"]["isError"], false);
    assert_eq!(response(&responses, 6)["result"]["isError"], false);

    for writable in [false, true] {
        let responses = run(
            &path,
            writable,
            &[
                &request(
                    7,
                    "tools/call",
                    json!({"name":"vlite_get","arguments":{"id":"6","include_vector":true}}),
                ),
                &request(
                    8,
                    "tools/call",
                    json!({"name":"vlite_list","arguments":{"after":"5","limit":1}}),
                ),
                &request(
                    9,
                    "tools/call",
                    json!({"name":"vlite_search","arguments":{"vector":[6,6],"k":1}}),
                ),
            ],
        );
        for id in 7..=9 {
            assert_eq!(response(&responses, id)["result"]["isError"], false);
        }
        assert_eq!(
            response(&responses, 7)["result"]["structuredContent"],
            json!({"record":{"id":"6","metadata":{},"payload_base64":"AP/+gA==","vector":[6.0,6.0]}})
        );
        assert_eq!(
            response(&responses, 8)["result"]["structuredContent"],
            json!({"records":[{"id":"6","metadata":{},"payload_base64":"AP/+gA=="}],"next_after":null})
        );
        // Search has always returned only IDs and distances, not record payloads.
        assert_eq!(
            response(&responses, 9)["result"]["structuredContent"],
            json!({"hits":[{"id":"6","distance":0.0}]})
        );
    }

    for (payload, message) in [
        ("AA-_", "invalid base64 payload"),
        ("AA", "payload_base64 must use padded standard base64"),
        ("AA=A", "invalid base64 padding"),
        ("AA==AAAA", "invalid base64 padding"),
        ("AB==", "payload_base64 must be canonical padded base64"),
        ("AAF=", "payload_base64 must be canonical padded base64"),
    ] {
        let responses = run(
            &path,
            true,
            &[&request(
                10,
                "tools/call",
                json!({"name":"vlite_upsert","arguments":{"id":"6","vector":[6,6],"payload_base64":payload}}),
            )],
        );
        assert_eq!(
            response(&responses, 10)["result"],
            json!({"isError":true,"content":[{"type":"text","text":message}],"structuredContent":{"error":message}}),
            "payload_base64={payload:?}"
        );
    }
    let db = Database::open(path).unwrap();
    assert_eq!(db.get(4).unwrap().unwrap().payload, b"added");
    assert_eq!(db.get(5).unwrap().unwrap().payload, [0, 1, 2]);
    assert_eq!(db.get(6).unwrap().unwrap().payload, [0, 255, 254, 128]);
    assert!(db.get(1).unwrap().is_none());
}

// What `dispatch` answers before any tool runs: the error codes, the id each error echoes
// (even an invalid one), the defaults for absent params and arguments, and the silence of
// notifications.
#[test]
fn protocol_edge_cases_keep_their_answers() {
    const INVALID: &str = "Invalid Request";
    let error = |id: Value, code: i32, message: &str| {
        Some(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}))
    };
    let result = |id: Value, result: Value| Some(json!({"jsonrpc":"2.0","id":id,"result":result}));
    let call_error = "tools/call requires a tool name and object arguments";
    // Each request with its answer; `None` is a request that must stay unanswered.
    let cases: Vec<(&str, Option<Value>)> = vec![
        // The message itself.
        ("[1,2]", error(Value::Null, -32600, INVALID)),
        ("7", error(Value::Null, -32600, INVALID)),
        ("null", error(Value::Null, -32600, INVALID)),
        // jsonrpc and method.
        (
            r#"{"id":1,"method":"ping"}"#,
            error(json!(1), -32600, INVALID),
        ),
        (
            r#"{"jsonrpc":"1.0","id":"a","method":"ping"}"#,
            error(json!("a"), -32600, INVALID),
        ),
        (
            r#"{"jsonrpc":"2.0","id":2}"#,
            error(json!(2), -32600, INVALID),
        ),
        (
            r#"{"jsonrpc":"2.0","id":3,"method":5}"#,
            error(json!(3), -32600, INVALID),
        ),
        (r#"{"jsonrpc":"2.0","method":"ping","params":[1]}"#, None),
        // An invalid id is refused and echoed.
        (
            r#"{"jsonrpc":"2.0","id":1.5,"method":"ping"}"#,
            error(json!(1.5), -32600, INVALID),
        ),
        (
            r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
            error(Value::Null, -32600, INVALID),
        ),
        (
            r#"{"jsonrpc":"2.0","id":true,"method":"ping"}"#,
            error(json!(true), -32600, INVALID),
        ),
        (
            r#"{"jsonrpc":"2.0","id":[1,"x"],"method":"ping"}"#,
            error(json!([1, "x"]), -32600, INVALID),
        ),
        (
            r#"{"jsonrpc":"2.0","id":{"a":1},"method":"ping"}"#,
            error(json!({"a": 1}), -32600, INVALID),
        ),
        // Valid ids: strings, negative and the largest unsigned integers.
        (
            r#"{"jsonrpc":"2.0","id":"s","method":"ping"}"#,
            result(json!("s"), json!({})),
        ),
        (
            r#"{"jsonrpc":"2.0","id":-5,"method":"ping"}"#,
            result(json!(-5), json!({})),
        ),
        (
            r#"{"jsonrpc":"2.0","id":18446744073709551615,"method":"ping"}"#,
            result(json!(18446744073709551615u64), json!({})),
        ),
        // Params must be an object when present, and default to one when absent.
        (
            r#"{"jsonrpc":"2.0","id":4,"method":"ping","params":[1]}"#,
            error(json!(4), -32602, "Invalid params"),
        ),
        (
            r#"{"jsonrpc":"2.0","id":5,"method":"ping","params":null}"#,
            error(json!(5), -32602, "Invalid params"),
        ),
        (
            r#"{"jsonrpc":"2.0","id":6,"method":"ping","params":"text"}"#,
            error(json!(6), -32602, "Invalid params"),
        ),
        (
            r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#,
            result(json!(7), json!({})),
        ),
        // tools/call needs a name and object arguments.
        (
            r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"arguments":{}}}"#,
            error(json!(8), -32602, call_error),
        ),
        (
            r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":5,"arguments":{}}}"#,
            error(json!(9), -32602, call_error),
        ),
        (
            r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"vlite_info","arguments":null}}"#,
            error(json!(10), -32602, call_error),
        ),
        (
            r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"vlite_info","arguments":[1]}}"#,
            error(json!(11), -32602, call_error),
        ),
        (
            r#"{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"vlite_info","arguments":"x"}}"#,
            error(json!(12), -32602, call_error),
        ),
        // Methods that are unknown, and notifications.
        (
            r#"{"jsonrpc":"2.0","id":13,"method":"nothing/here"}"#,
            error(json!(13), -32601, "Method not found"),
        ),
        (r#"{"jsonrpc":"2.0","method":"nothing/here"}"#, None),
        (
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            None,
        ),
        (
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#,
            None,
        ),
        (
            r#"{"jsonrpc":"2.0","method":"notifications/progress"}"#,
            None,
        ),
        (
            r#"{"jsonrpc":"2.0","id":14,"method":"notifications/initialized"}"#,
            result(json!(14), Value::Null),
        ),
        // A ping that is answered last proves that nothing above ended the session.
        (
            r#"{"jsonrpc":"2.0","id":15,"method":"ping"}"#,
            result(json!(15), json!({})),
        ),
    ];
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("edges.vlite");
    fixture(&path);
    let lines: Vec<&str> = cases.iter().map(|(line, _)| *line).collect();
    let responses = run(&path, false, &lines);
    let answered: Vec<_> = cases
        .iter()
        .filter_map(|(line, expected)| expected.as_ref().map(|expected| (*line, expected)))
        .collect();
    assert_eq!(responses.len(), answered.len(), "{responses:#?}");
    for (response, (line, expected)) in responses.iter().zip(answered) {
        assert_eq!(response, expected, "{line}");
    }

    // Absent arguments are an empty object; the version is negotiated from params if any.
    let responses = run(
        &path,
        false,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"vlite_info"}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize"}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
            r#"{"jsonrpc":"2.0","id":4,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
        ],
    );
    let info = &response(&responses, 1)["result"];
    assert_eq!(info["isError"], false);
    assert_eq!(info["structuredContent"]["read_only"], true);
    assert_eq!(info["structuredContent"]["count"], 3);
    for (id, version) in [(2, "2025-06-18"), (3, "2025-06-18"), (4, "2024-11-05")] {
        assert_eq!(
            response(&responses, id)["result"]["protocolVersion"],
            version
        );
    }
}
