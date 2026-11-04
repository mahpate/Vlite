#![cfg(feature = "mcp-proxy")]

use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

fn goldens() -> Value {
    serde_json::from_str(include_str!("fixtures/memory_tools/exchanges.json")).unwrap()
}

struct Fake {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
}

impl Fake {
    fn new(status: u16, body: Value, delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/prefix", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let task = thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                let (stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let parts: Vec<_> = line.split_whitespace().collect();
                let method = parts[0].to_owned();
                let path = parts[1].to_owned();
                let mut headers = serde_json::Map::new();
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    let (key, value) = line.split_once(':').unwrap();
                    headers.insert(key.to_ascii_lowercase(), json!(value.trim()));
                }
                let length = headers
                    .get("content-length")
                    .and_then(Value::as_str)
                    .map_or(0, |s| s.parse().unwrap());
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).unwrap();
                let request_body = if bytes.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes).unwrap()
                };
                captured.lock().unwrap().push(json!({
                    "method":method,"path":path,"headers":headers,"body":request_body
                }));
                let (code, response) = if path == "/prefix/v1/graphs" {
                    (
                        403,
                        json!({"error":{"code":"forbidden_scope","message":"No raw access"}}),
                    )
                } else {
                    thread::sleep(delay);
                    (status, body.clone())
                };
                let text = response.to_string();
                let response = format!(
                    "HTTP/1.1 {code} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
                // A timeout deliberately closes the other end first.
                let _ = reader.get_mut().write_all(response.as_bytes());
            }
        });
        Self {
            url,
            requests,
            stop,
            task: Some(task),
        }
    }

    fn memory_requests(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["path"] != "/prefix/v1/graphs")
            .cloned()
            .collect()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let joined = self.task.take().unwrap().join();
        if !thread::panicking() {
            joined.unwrap();
        }
    }
}

fn run(server: &Fake, writable: bool, messages: Vec<Value>) -> Vec<Value> {
    run_command(proxy_command(server, writable), messages)
}

fn proxy_command(server: &Fake, writable: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vlite-mcp"));
    command
        .args([
            "--server",
            &server.url,
            "--token-env",
            "MEMORY_MCP_TEST_TOKEN",
        ])
        .arg(if writable {
            "--writable"
        } else {
            "--read-only"
        })
        .env("MEMORY_MCP_TEST_TOKEN", "memory-test-token");
    command
}

fn run_command(command: Command, messages: Vec<Value>) -> Vec<Value> {
    run_lines(
        command,
        messages
            .into_iter()
            .map(|message| message.to_string())
            .collect(),
    )
}

fn run_lines(mut command: Command, messages: Vec<String>) -> Vec<Value> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut input = child.stdin.take().unwrap();
        for message in messages {
            writeln!(input, "{message}").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}

fn list() -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})
}

fn call(case: &Value) -> Value {
    let mut arguments = case["request"].clone();
    arguments["binding"] = json!("notes");
    json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":case["name"],"arguments":arguments}})
}

#[test]
fn memory_list_contract_and_writable_gating() {
    let golden = goldens();
    let server = Fake::new(200, json!({}), Duration::ZERO);
    for writable in [false, true] {
        let output = run(&server, writable, vec![list()]);
        let tools = output[0]["result"]["tools"].as_array().unwrap();
        let cases: Vec<_> = golden["cases"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| writable || c["read_only"] == true)
            .collect();
        assert_eq!(tools.len(), cases.len());
        for (tool, case) in tools.iter().zip(cases) {
            assert_eq!(tool["name"], case["name"]);
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert_eq!(tool["inputSchema"]["required"], case["required"]);
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
            let definitions: Value =
                serde_json::from_str(include_str!("fixtures/memory_tools/schemas.json")).unwrap();
            let properties: Value =
                serde_json::from_str(include_str!("fixtures/memory_tools/requests.json")).unwrap();
            let mut expected = properties[case["name"].as_str().unwrap()]
                .as_object()
                .unwrap()
                .clone();
            if case["read_only"] == false {
                expected.extend(properties["mutation"].as_object().unwrap().clone());
            }
            expected.insert("binding".into(), json!({"$ref":"#/$defs/MemoryBinding"}));
            assert_eq!(
                tool["inputSchema"],
                json!({
                    "type":"object","additionalProperties":false,
                    "required":case["required"],"properties":expected,"$defs":definitions
                })
            );
            assert!(tool["inputSchema"]["properties"].get("request").is_none());
            assert!(tool.get("outputSchema").is_none());
            assert_eq!(tool["annotations"]["readOnlyHint"], case["read_only"]);
            if case["read_only"] == false {
                assert_eq!(tool["annotations"]["idempotentHint"], false);
            }
        }
    }
    for case in golden["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["read_only"] == false)
    {
        let output = run(&server, false, vec![call(case)]);
        assert_eq!(output[0]["result"]["isError"], true);
        assert_eq!(
            output[0]["result"]["structuredContent"]["error"],
            format!(
                "read_only: database is read-only; {} is unavailable",
                case["name"].as_str().unwrap()
            )
        );
    }
    assert!(server.memory_requests().is_empty());
}

#[test]
fn memory_routes_token_bodies_and_exact_success() {
    let golden = goldens();
    for case in golden["cases"].as_array().unwrap() {
        for status in [200, 202] {
            let server = Fake::new(status, case["response"].clone(), Duration::ZERO);
            let output = run(&server, true, vec![call(case)]);
            let result = &output[0]["result"];
            assert_eq!(result["isError"], false, "{}", case["name"]);
            assert_eq!(result["structuredContent"], case["response"]);
            let text: Value =
                serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(text, case["response"]);
            let requests = server.memory_requests();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0]["method"], "POST");
            assert_eq!(
                requests[0]["path"],
                format!(
                    "/prefix/v1/memory/notes/{}",
                    case["suffix"].as_str().unwrap()
                )
            );
            assert_eq!(requests[0]["body"], case["request"]);
            for (key, value) in golden["headers"].as_object().unwrap() {
                assert_eq!(&requests[0]["headers"][key], value);
            }
        }
    }
}

#[test]
fn memory_server_failures_are_coded_tool_errors() {
    let golden = goldens();
    for error in golden["errors"].as_array().unwrap() {
        let server = Fake::new(
            error["status"].as_u64().unwrap() as u16,
            error["body"].clone(),
            Duration::ZERO,
        );
        let output = run(&server, true, vec![call(&golden["cases"][0])]);
        let result = &output[0]["result"];
        assert_eq!(result["isError"], true);
        assert_eq!(
            result["structuredContent"],
            json!({"error":error["expected"]})
        );
        assert_eq!(
            result["content"],
            json!([{"type":"text","text":error["expected"]}])
        );
        assert_eq!(server.memory_requests().len(), 1);
    }
}

#[test]
fn memory_unknown_nested_and_wrong_types_never_reach_server() {
    let golden = goldens();
    let server = Fake::new(200, json!({}), Duration::ZERO);
    for invalid in golden["invalid"].as_array().unwrap() {
        let mut message = call(&golden["cases"][0]);
        for (key, value) in invalid["patch"].as_object().unwrap() {
            message["params"]["arguments"][key] = value.clone();
        }
        let output = run(&server, true, vec![message]);
        assert_eq!(output[0]["result"]["isError"], true);
        assert_eq!(
            output[0]["result"]["structuredContent"]["error"],
            invalid["expected"]
        );
    }
    assert!(server.memory_requests().is_empty());
}

#[test]
fn memory_nested_wrapper_is_rejected() {
    let server = Fake::new(200, json!({}), Duration::ZERO);
    let message = json!({
        "jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"vlite_memory_remember",
            "arguments":{"binding":"notes","request":goldens()["cases"][0]["request"]}}
    });
    let output = run(&server, true, vec![message]);
    assert_eq!(output[0]["result"]["isError"], true);
    assert_eq!(
        output[0]["result"]["structuredContent"]["error"],
        "invalid_request: arguments.scope is required"
    );
    assert!(server.memory_requests().is_empty());
}

#[test]
fn memory_timeout_is_unknown_and_never_retried() {
    let server = Fake::new(200, json!({}), Duration::from_secs(6));
    let output = run(&server, true, vec![call(&goldens()["cases"][0])]);
    let result = &output[0]["result"];
    assert_eq!(result["isError"], true);
    let error = result["structuredContent"]["error"].as_str().unwrap();
    assert!(error.contains("UNKNOWN"), "{error}");
    assert!(error.contains("receipt"), "{error}");
    assert_eq!(server.memory_requests().len(), 1);
}

#[test]
fn memory_local_mode_keeps_the_baseline_inventory() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("memory-local.vlite");
    vlite::Database::create_with_format(&path, 2, vlite::FormatVersion::V3)
        .unwrap()
        .close()
        .unwrap();
    for writable in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_vlite-mcp"));
        command.arg("--db").arg(&path).arg(if writable {
            "--writable"
        } else {
            "--read-only"
        });
        let output = run_command(command, vec![list(), call(&goldens()["cases"][0])]);
        let tools = output[0]["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), if writable { 13 } else { 9 });
        assert!(
            tools
                .iter()
                .all(|tool| !tool["name"].as_str().unwrap().starts_with("vlite_memory_"))
        );
        assert_eq!(
            output[1]["result"]["structuredContent"]["error"],
            "unknown tool: vlite_memory_remember"
        );
    }
}

#[test]
fn memory_missing_and_duplicate_fields_never_reach_server() {
    let server = Fake::new(200, json!({}), Duration::ZERO);
    for case in goldens()["cases"].as_array().unwrap() {
        for field in case["required"].as_array().unwrap() {
            let mut message = call(case);
            let field = field.as_str().unwrap();
            message["params"]["arguments"]
                .as_object_mut()
                .unwrap()
                .remove(field);
            let output = run(&server, true, vec![message]);
            assert_eq!(output[0]["result"]["isError"], true);
            assert_eq!(
                output[0]["result"]["structuredContent"]["error"],
                format!("invalid_request: arguments.{field} is required")
            );
        }
    }
    let duplicate = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"vlite_memory_recall","arguments":{"binding":"notes","scope":{"agent":"a","agent":"b"},"text":"hello"}}}"#;
    let output = run_lines(proxy_command(&server, true), vec![duplicate.into()]);
    assert_eq!(output[0]["result"]["isError"], true);
    assert!(
        output[0]["result"]["structuredContent"]["error"]
            .as_str()
            .unwrap()
            .starts_with("invalid_request: duplicate argument field")
    );
    assert!(server.memory_requests().is_empty());
}

#[test]
fn memory_integer_inputs_and_server_owned_semantics_are_unchanged() {
    let server = Fake::new(
        200,
        json!({"opaque":{"sequence":"18446744073709551615","at_ms":"-9223372036854775808"}}),
        Duration::ZERO,
    );
    let mut message = call(&goldens()["cases"][0]);
    message["params"]["arguments"]["items"][0]["observed_at_ms"] = json!(i64::MIN);
    message["params"]["arguments"]["items"][0]["expires_at_ms"] = json!(u64::MAX);
    message["params"]["arguments"]["items"][0]["content"] = json!("");
    message["params"]["arguments"]["items"][0]["metadata"] = json!({
        "counter":{"type":"integer","value":"18446744073709551615"},
        "flag":{"type":"boolean","value":false}
    });
    message["params"]["arguments"]["deadline_ms"] = json!(6000);
    let mut expected = message["params"]["arguments"].clone();
    expected.as_object_mut().unwrap().remove("binding");
    let output = run(&server, true, vec![message]);
    assert_eq!(output[0]["result"]["isError"], false);
    assert_eq!(server.memory_requests()[0]["body"], expected);
    assert_eq!(
        output[0]["result"]["structuredContent"],
        json!({"opaque":{"sequence":"18446744073709551615","at_ms":"-9223372036854775808"}})
    );
}

#[test]
fn memory_typed_metadata_rejects_mismatched_values() {
    let server = Fake::new(200, json!({}), Duration::ZERO);
    let mut message = call(&goldens()["cases"][0]);
    message["params"]["arguments"]["items"][0]["metadata"] =
        json!({"flag":{"type":"boolean","value":"not a boolean"}});
    let output = run(&server, true, vec![message]);
    assert_eq!(output[0]["result"]["isError"], true);
    assert!(server.memory_requests().is_empty());
}

#[test]
fn memory_request_size_boundary_and_tool_result_size_boundary() {
    const CAP: usize = 8_388_608;
    let server = Fake::new(200, json!({"ok":true}), Duration::ZERO);
    let mut message = call(&goldens()["cases"][0]);
    message["params"]["arguments"]["items"][0]["content"] = json!("");
    let overhead = message["params"]["arguments"].to_string().len();
    for extra in [0, 1] {
        message["params"]["arguments"]["items"][0]["content"] =
            json!("x".repeat(CAP - overhead + extra));
        let output = run(&server, true, vec![message.clone()]);
        assert_eq!(output[0]["result"]["isError"], extra == 1);
        if extra == 1 {
            assert_eq!(
                output[0]["result"]["structuredContent"]["error"],
                "invalid_json: memory request exceeds 8388608 bytes"
            );
        }
    }
    assert_eq!(server.memory_requests().len(), 1);
    let empty = json!({"blob":""});
    let expected_empty_result = json!({
        "content":[{"type":"text","text":empty.to_string()}],
        "structuredContent":empty,"isError":false
    });
    // Both representations grow by one byte for each ASCII payload byte.
    let maximum = (CAP - expected_empty_result.to_string().len()) / 2;
    for extra in [0, 1] {
        let body = json!({"blob":"x".repeat(maximum+extra)});
        let server = Fake::new(200, body.clone(), Duration::ZERO);
        let output = run(&server, true, vec![call(&goldens()["cases"][1])]);
        let result = &output[0]["result"];
        assert_eq!(result["isError"], extra == 1);
        if extra == 0 {
            assert_eq!(result["structuredContent"], body);
            assert!(result.to_string().len() <= CAP);
            assert!(result.to_string().len() >= CAP - 1);
        } else {
            assert_eq!(
                result["structuredContent"]["error"],
                "response_too_large: memory tool result exceeds 8388608 bytes"
            );
        }
    }
}

#[test]
fn memory_oversized_mutation_responses_remain_unknown_errors() {
    for (status, body) in [
        (200, json!({"blob":"x".repeat(8_388_608)})),
        (
            403,
            json!({"error":{"code":"forbidden_scope","message":"x".repeat(4_194_304)}}),
        ),
    ] {
        let server = Fake::new(status, body, Duration::ZERO);
        let output = run(&server, true, vec![call(&goldens()["cases"][0])]);
        assert_eq!(output[0]["result"]["isError"], true);
        let error = output[0]["result"]["structuredContent"]["error"]
            .as_str()
            .unwrap();
        assert!(error.starts_with("response_too_large:"), "{error}");
        assert!(error.contains("UNKNOWN"), "{error}");
        assert!(error.contains("receipt"), "{error}");
        assert_eq!(server.memory_requests().len(), 1);
    }
}

#[test]
fn memory_error_envelopes_never_become_success_even_with_http_200() {
    let server = Fake::new(
        200,
        json!({"error":{"code":"internal_error","message":"Bad response status"}}),
        Duration::ZERO,
    );
    let output = run(&server, true, vec![call(&goldens()["cases"][0])]);
    assert_eq!(output[0]["result"]["isError"], true);
    assert_eq!(
        output[0]["result"]["structuredContent"]["error"],
        "internal_error: Bad response status"
    );
}

#[test]
fn memory_binding_is_one_inert_path_segment() {
    let server = Fake::new(
        409,
        json!({"error":{"code":"memory_not_configured","message":"No binding"}}),
        Duration::ZERO,
    );
    let mut message = call(&goldens()["cases"][1]);
    message["params"]["arguments"]["binding"] = json!("notes/../../admin?token=hidden#fragment");
    let output = run(&server, true, vec![message]);
    assert_eq!(output[0]["result"]["isError"], true);
    assert_eq!(
        server.memory_requests()[0]["path"],
        "/prefix/v1/memory/notes%2F%2E%2E%2F%2E%2E%2Fadmin%3Ftoken%3Dhidden%23fragment/recall"
    );
    for binding in ["", ".", ".."] {
        let mut message = call(&goldens()["cases"][1]);
        message["params"]["arguments"]["binding"] = json!(binding);
        let output = run(&server, true, vec![message]);
        assert_eq!(output[0]["result"]["isError"], true);
        assert_eq!(
            output[0]["result"]["structuredContent"]["error"],
            "invalid_request: binding must not be empty or dot-only"
        );
    }
    assert_eq!(server.memory_requests().len(), 1);
}
