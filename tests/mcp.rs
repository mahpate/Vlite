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
    let db = Database::open(path).unwrap();
    assert_eq!(db.get(4).unwrap().unwrap().payload, b"added");
    assert_eq!(db.get(5).unwrap().unwrap().payload, [0, 1, 2]);
    assert!(db.get(1).unwrap().is_none());
}
