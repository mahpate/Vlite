#![cfg(feature = "mcp-proxy")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const TOKEN: &str = "memory-cli-test-secret";
const TOKEN_ENV: &str = "VLITE_MEMORY_CLI_TEST_TOKEN";

fn binary() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vlite-memory-cli"));
    cmd.env(TOKEN_ENV, TOKEN)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn command(url: &str, verb: &str) -> Command {
    let mut cmd = binary();
    cmd.args([
        "--url",
        url,
        "--binding",
        "notes",
        "--token-env",
        TOKEN_ENV,
        verb,
        "--request",
        "-",
    ]);
    cmd
}

fn run(mut cmd: Command, input: &[u8]) -> Output {
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).ok();
    let output = child.wait_with_output().unwrap();
    assert!(!String::from_utf8_lossy(&output.stdout).contains(TOKEN));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(TOKEN));
    output
}

fn failure(output: Output, exit: i32, stderr: &str) {
    assert_eq!(output.status.code(), Some(exit), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert_eq!(String::from_utf8(output.stderr).unwrap(), stderr);
}

fn fixtures(name: &str) -> Vec<Value> {
    let text = match name {
        "exchanges" => include_str!("fixtures/memory_cli/exchanges.json"),
        "errors" => include_str!("fixtures/memory_cli/errors.json"),
        "usage" => include_str!("fixtures/memory_cli/usage.json"),
        _ => panic!("unknown fixture"),
    };
    serde_json::from_str(text).unwrap()
}

fn receive(stream: &mut TcpStream) -> (String, Vec<u8>) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut byte = [0];
    while !bytes.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    let headers = String::from_utf8(bytes).unwrap();
    let length: usize = headers
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .map(str::parse)
        })
        .unwrap()
        .unwrap();
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    (headers, body)
}

fn serve(status: u16, response: String) -> (String, thread::JoinHandle<(String, Vec<u8>)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let worker = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let started = Instant::now();
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(started.elapsed() < Duration::from_secs(10), "no request");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        };
        let request = receive(&mut stream);
        write!(
            stream,
            "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len()
        ).unwrap();
        request
    });
    (url, worker)
}

#[test]
fn all_nine_contract_exchanges_and_exact_one_line() {
    let arguments: Value =
        serde_json::from_str(include_str!("fixtures/memory_cli/arguments.json")).unwrap();
    let http: Value = serde_json::from_str(include_str!("fixtures/memory_cli/http.json")).unwrap();
    for fixture in fixtures("exchanges") {
        let verb = fixture["verb"].as_str().unwrap();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("memory_cli")
            .join(format!("{verb}.json"));
        let request = std::fs::read(&path).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&request).unwrap(),
            fixture["request"]
        );
        for from_stdin in [false, true] {
            let stdout = fixture["stdout"].as_str().unwrap();
            let (url, server) = serve(
                fixture["status"].as_u64().unwrap() as u16,
                format!("\n{stdout}\n"),
            );
            let mut cmd = binary();
            cmd.args(arguments[verb].as_array().unwrap().iter().map(|value| {
                match value.as_str().unwrap() {
                    "{url}" => url.clone(),
                    "{request}" if from_stdin => "-".to_owned(),
                    "{request}" => path.to_str().unwrap().to_owned(),
                    arg => arg.to_owned(),
                }
            }));
            let output = run(cmd, if from_stdin { &request } else { b"" });
            assert_eq!(output.status.code(), Some(0), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            assert_eq!(String::from_utf8(output.stdout).unwrap(), stdout);
            let (headers, body) = server.join().unwrap();
            assert_eq!(
                headers.lines().next().unwrap(),
                format!(
                    "{} {} HTTP/1.1",
                    http["method"].as_str().unwrap(),
                    fixture["path"].as_str().unwrap()
                )
            );
            let headers = headers.to_ascii_lowercase();
            for (name, value) in http["headers"].as_object().unwrap() {
                assert!(headers.contains(&format!(
                    "\r\n{name}: {}\r\n",
                    value.as_str().unwrap().to_ascii_lowercase()
                )));
            }
            assert_eq!(body, request);
        }
    }
}

#[test]
fn contract_error_envelopes_and_exit_codes() {
    for fixture in fixtures("errors") {
        let (url, server) = serve(
            fixture["status"].as_u64().unwrap() as u16,
            fixture["body"].to_string(),
        );
        failure(
            run(
                command(&url, "receipt"),
                br#"{"scope":{"agent":"a"},"operation":"forget","idempotency_key":"k1"}"#,
            ),
            fixture["exit"].as_i64().unwrap() as i32,
            fixture["stderr"].as_str().unwrap(),
        );
        server.join().unwrap();
    }
}

#[test]
fn usage_goldens_and_required_flags() {
    for fixture in fixtures("usage") {
        let mut cmd = binary();
        cmd.args(
            fixture["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap()),
        );
        failure(run(cmd, b""), 2, fixture["stderr"].as_str().unwrap());
    }
    let args = [
        "--url",
        "http://127.0.0.1",
        "--binding",
        "notes",
        "--token-env",
        TOKEN_ENV,
        "recall",
        "--request",
        "-",
    ];
    for (index, expected) in [
        (
            0,
            "{\"error\":{\"code\":\"usage\",\"message\":\"missing --url\"}}\n",
        ),
        (
            2,
            "{\"error\":{\"code\":\"usage\",\"message\":\"missing --binding\"}}\n",
        ),
        (
            4,
            "{\"error\":{\"code\":\"usage\",\"message\":\"missing --token-env\"}}\n",
        ),
        (
            7,
            "{\"error\":{\"code\":\"usage\",\"message\":\"missing --request\"}}\n",
        ),
    ] {
        let mut cmd = binary();
        cmd.args(&args[..index]).args(&args[index + 2..]);
        failure(run(cmd, b"{}"), 2, expected);
    }
    failure(
        run(command("http://127.0.0.1", "unknown"), b"{}"),
        2,
        "{\"error\":{\"code\":\"usage\",\"message\":\"unknown verb\"}}\n",
    );
}

#[test]
fn unset_empty_and_invalid_header_tokens_are_local_errors() {
    let oversized = "x".repeat(4097);
    for token in [
        None,
        Some(""),
        Some("bad\ntoken"),
        Some("has space"),
        Some(" leading"),
        Some("trailing "),
        Some("nonascii-\u{e9}"),
        Some(oversized.as_str()),
    ] {
        let mut cmd = command("http://127.0.0.1", "recall");
        match token {
            Some(value) => {
                cmd.env(TOKEN_ENV, value);
            }
            None => {
                cmd.env_remove(TOKEN_ENV);
            }
        }
        failure(
            run(cmd, b"{}"),
            2,
            "{\"error\":{\"code\":\"usage\",\"message\":\"token variable must contain 1..4096 visible ASCII bytes\"}}\n",
        );
    }
}

#[test]
fn invalid_json_and_oversize_file_or_stdin_never_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    for input in [
        b"not JSON".as_slice(),
        b"[]",
        b"null",
        br#"{"scope":{},"scope":{}}"#,
    ] {
        failure(
            run(command(&url, "recall"), input),
            2,
            "{\"error\":{\"code\":\"invalid_json\",\"message\":\"request must be a JSON object without duplicate keys\"}}\n",
        );
    }
    let mut too_large = b"{}".to_vec();
    too_large.resize(8_388_609, b' ');
    failure(
        run(command(&url, "recall"), &too_large),
        2,
        "{\"error\":{\"code\":\"invalid_json\",\"message\":\"request exceeds 8388608 bytes\"}}\n",
    );
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), too_large).unwrap();
    let mut cmd = binary();
    cmd.args([
        "--url",
        &url,
        "--binding",
        "notes",
        "--token-env",
        TOKEN_ENV,
        "recall",
        "--request",
    ])
    .arg(file.path());
    failure(
        run(cmd, b""),
        2,
        "{\"error\":{\"code\":\"invalid_json\",\"message\":\"request exceeds 8388608 bytes\"}}\n",
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn timeout_is_ambiguity_without_mutation_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let worker = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        let mut count = 0;
        let mut streams = Vec::new();
        while Instant::now() < until {
            if let Ok((mut stream, _)) = listener.accept() {
                receive(&mut stream);
                streams.push(stream);
                count += 1;
            }
            thread::sleep(Duration::from_millis(5));
        }
        count
    });
    let mut cmd = command(&url, "remember");
    cmd.args(["--timeout-ms", "100"]);
    failure(
        run(
            cmd,
            fixtures("exchanges")[0]["request"].to_string().as_bytes(),
        ),
        6,
        "{\"error\":{\"code\":\"transport_error\",\"message\":\"HTTP request timed out; reconcile mutations by receipt\"}}\n",
    );
    assert_eq!(worker.join().unwrap(), 1);
}

#[test]
fn timeout_bounds_and_deadline_body_are_independent() {
    for timeout in ["1", "5000"] {
        let mut cmd = command("http://127.0.0.1:1", "recall");
        cmd.args(["--timeout-ms", timeout]);
        let output = run(cmd, br#"{"scope":{"agent":"a"},"text":"x"}"#);
        assert_eq!(output.status.code(), Some(6), "{output:?}");
    }
    let body = br#"{ "scope":{"agent":"a"}, "text":"x", "limits":{"deadline_ms":17}, "as_of_ms":-9223372036854775808 }"#;
    let (url, server) = serve(
        200,
        fixtures("exchanges")[1]["stdout"].as_str().unwrap().into(),
    );
    let mut cmd = command(&url, "recall");
    cmd.args(["--timeout-ms", "4999"]);
    assert!(run(cmd, body).status.success());
    assert_eq!(server.join().unwrap().1, body);
    let body = br#"{ "scope":{"agent":"a"}, "idempotency_key":"deadline", "items":[{"id":"e1","kind":"entity","content":"Ada","observed_at_ms":90,"source":{"source_id":"s1","source_kind":"caller"}}], "deadline_ms":17 }"#;
    let (url, server) = serve(
        200,
        fixtures("exchanges")[0]["stdout"].as_str().unwrap().into(),
    );
    let mut cmd = command(&url, "remember");
    cmd.args(["--timeout-ms", "4999"]);
    assert!(run(cmd, body).status.success());
    assert_eq!(server.join().unwrap().1, body);
}

#[test]
fn redacts_token_in_server_errors_and_refuses_credential_success() {
    let (url, server) = serve(
        500,
        format!(r#"{{"error":{{"code":"{TOKEN}","message":"bad {TOKEN}\nline"}}}}"#),
    );
    failure(
        run(command(&url, "recall"), b"{}"),
        1,
        "{\"error\":{\"code\":\"[REDACTED]\",\"message\":\"bad [REDACTED] line\"}}\n",
    );
    server.join().unwrap();
    let (url, server) = serve(200, format!(r#"{{"value":"{TOKEN}"}}"#));
    let output = run(command(&url, "recall"), b"{}");
    failure(
        output,
        6,
        "{\"error\":{\"code\":\"credential_in_response\",\"message\":\"response contained the bearer credential and was not printed; reconcile mutations by receipt\"}}\n",
    );
    server.join().unwrap();
}

#[test]
fn rejects_unsafe_urls_bindings_redirects_and_bad_responses() {
    for url in [
        "file:///tmp/x",
        "http://user:pass@localhost",
        "http://localhost/?x=y",
        "http://localhost/#x",
    ] {
        failure(
            run(command(url, "recall"), b"{}"),
            2,
            "{\"error\":{\"code\":\"usage\",\"message\":\"--url must be an HTTP(S) endpoint without credentials, query or fragment\"}}\n",
        );
    }
    for binding in ["../other", "Notes", "a/b", ""] {
        let mut cmd = command("http://127.0.0.1", "recall");
        // Build afresh to distinguish an invalid name from a duplicate flag.
        cmd.args(["--binding", binding]);
        failure(
            run(cmd, b"{}"),
            2,
            "{\"error\":{\"code\":\"usage\",\"message\":\"duplicate option\"}}\n",
        );
        let mut cmd = binary();
        cmd.args([
            "--url",
            "http://127.0.0.1",
            "--binding",
            binding,
            "--token-env",
            TOKEN_ENV,
            "recall",
            "--request",
            "-",
        ]);
        failure(
            run(cmd, b"{}"),
            2,
            "{\"error\":{\"code\":\"usage\",\"message\":\"invalid binding name\"}}\n",
        );
    }
    for (status, body, exit, expected) in [
        (
            200,
            "not json",
            6,
            "{\"error\":{\"code\":\"invalid_response\",\"message\":\"server response is not a JSON object; reconcile mutations by receipt\"}}\n",
        ),
        (
            500,
            "not json",
            6,
            "{\"error\":{\"code\":\"invalid_response\",\"message\":\"server response is not a valid error envelope; reconcile mutations by receipt\"}}\n",
        ),
        (
            302,
            "{}",
            6,
            "{\"error\":{\"code\":\"invalid_response\",\"message\":\"server response is not a valid error envelope; reconcile mutations by receipt\"}}\n",
        ),
    ] {
        let (url, server) = serve(status, body.into());
        failure(run(command(&url, "recall"), b"{}"), exit, expected);
        server.join().unwrap();
    }
}

#[test]
fn response_limit_is_checked_before_output() {
    let (url, server) = serve(200, " ".repeat(8_388_609));
    failure(
        run(command(&url, "recall"), b"{}"),
        6,
        "{\"error\":{\"code\":\"response_too_large\",\"message\":\"response exceeds 8388608 bytes; reconcile mutations by receipt\"}}\n",
    );
    server.join().unwrap();
}

#[test]
fn exact_request_and_response_byte_boundaries_are_accepted() {
    let mut body = br#"{"scope":{"agent":"a"},"text":"x"}"#.to_vec();
    body.resize(8_388_608, b' ');
    let response = format!(
        "{}{}",
        fixtures("exchanges")[1]["stdout"].as_str().unwrap(),
        " ".repeat(8_388_608 - fixtures("exchanges")[1]["stdout"].as_str().unwrap().len())
    );
    let (url, server) = serve(200, response);
    let output = run(command(&url, "recall"), &body);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        fixtures("exchanges")[1]["stdout"].as_str().unwrap()
    );
    assert_eq!(server.join().unwrap().1, body);
}

#[test]
fn redirect_does_not_forward_credentials_or_issue_second_request() {
    let destination = TcpListener::bind("127.0.0.1:0").unwrap();
    destination.set_nonblocking(true).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let location = format!("http://{}/stolen", destination.local_addr().unwrap());
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        receive(&mut stream);
        write!(stream, "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").unwrap();
    });
    failure(
        run(
            command(&url, "recall"),
            br#"{"scope":{"agent":"a"},"text":"x"}"#,
        ),
        6,
        "{\"error\":{\"code\":\"invalid_response\",\"message\":\"server response is not a valid error envelope; reconcile mutations by receipt\"}}\n",
    );
    worker.join().unwrap();
    assert_eq!(
        destination.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn pending_forget_and_replayed_responses_are_successes() {
    for (status, response, verb) in [
        (
            202,
            fixtures("exchanges")[8]["stdout"]
                .as_str()
                .unwrap()
                .replace("resume_erasure", "forget"),
            "forget",
        ),
        (
            200,
            fixtures("exchanges")[0]["stdout"]
                .as_str()
                .unwrap()
                .replace("\"replayed\":false", "\"replayed\":true"),
            "remember",
        ),
    ] {
        let (url, server) = serve(status, response.clone());
        let request = if verb == "forget" { 5 } else { 0 };
        let output = run(
            command(&url, verb),
            fixtures("exchanges")[request]["request"]
                .to_string()
                .as_bytes(),
        );
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert!(output.stderr.is_empty());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), response);
        server.join().unwrap();
    }
}

#[cfg(windows)]
#[test]
fn non_utf8_token_variable_is_rejected() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    let mut cmd = command("http://127.0.0.1", "recall");
    cmd.env(TOKEN_ENV, OsString::from_wide(&[0xD800]));
    failure(
        run(cmd, b"{}"),
        2,
        "{\"error\":{\"code\":\"usage\",\"message\":\"token variable must contain 1..4096 visible ASCII bytes\"}}\n",
    );
}

#[test]
fn ambient_proxies_are_never_contacted() {
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    let (url, server) = serve(200, "{}".into());
    let mut cmd = command(&url, "recall");
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        cmd.env(name, &proxy_url);
    }
    cmd.env_remove("NO_PROXY").env_remove("no_proxy");
    cmd.args(["--timeout-ms", "500"]);
    let output = run(cmd, b"{}");
    assert_eq!(
        proxy.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "CLI contacted the ambient proxy"
    );
    assert!(output.status.success(), "{output:?}");
    server.join().unwrap();
}

#[test]
fn nonloopback_http_is_refused_before_token_lookup() {
    for url in [
        "http://example.com",
        "http://192.0.2.1",
        "http://[::2]",
        "http://localhost.example.com",
        "http://[::ffff:127.0.0.1]",
    ] {
        let mut cmd = command(url, "remember");
        cmd.env_remove(TOKEN_ENV);
        failure(
            run(cmd, b"{}"),
            2,
            "{\"error\":{\"code\":\"insecure_url\",\"message\":\"HTTP is permitted only for loopback hosts; use HTTPS\"}}\n",
        );
    }
}

#[test]
fn credential_collisions_fail_closed_and_diagnostics_never_leak() {
    for (token, body) in [
        ("secret", r#"{"nested":[{"\u0073ecret-key":"ok"}]}"#),
        ("secret", r#"{"nested":[{"value":"prefix-\u0073ecret"}]}"#),
        ("x", r#"{"x":1,"[REDACTED]":2}"#),
        ("1", r#"{"value":1}"#),
        ("true", r#"{"value":true}"#),
        (r"\n", r#"{"value":"line\nbreak"}"#),
        ("\"", r#"{"value":0}"#),
        ("\\", r#"{"value":"line\nbreak"}"#),
    ] {
        let (url, server) = serve(200, body.into());
        let mut cmd = command(&url, "remember");
        cmd.env(TOKEN_ENV, token);
        let output = run(cmd, b"{}");
        assert_eq!(output.status.code(), Some(6), "{token:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(!String::from_utf8_lossy(&output.stderr).contains(token));
        server.join().unwrap();
    }
    for token in ["error", "REDACTED", "\"", "\\", "e", "x"] {
        let body =
            serde_json::json!({"error":{"code":token,"message":format!("bad {token}\nline")}});
        let (url, server) = serve(500, body.to_string());
        let mut cmd = command(&url, "remember");
        cmd.env(TOKEN_ENV, token);
        let output = run(cmd, b"{}");
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stderr.contains(token), "{token:?}: {stderr}");
        if ["error", "\"", "e"].contains(&token) {
            assert!(
                stderr.is_empty(),
                "even the fixed JSON fallback contains this token"
            );
        } else {
            assert!(stderr.ends_with('\n') && stderr.lines().count() == 1);
            serde_json::from_str::<Value>(&stderr).unwrap();
        }
        server.join().unwrap();
    }
}

#[test]
fn malformed_error_and_success_bodies_are_response_integrity_failures() {
    for status in [200, 400, 401, 409, 500, 504] {
        for (body, valid_object) in [
            ("not JSON", false),
            ("[]", false),
            ("null", false),
            (r#"{"a":1,"a":2}"#, false),
            (
                r#"{"error":{"code":"a","code":"b","message":"bad"}}"#,
                false,
            ),
            (r#"{"error":{"code":1,"message":"bad"}}"#, true),
            ("{}", true),
            (r#"{"error":{"code":"bad"}}"#, true),
        ] {
            if status == 200 && valid_object {
                continue;
            }
            let (url, server) = serve(status, body.into());
            let output = run(command(&url, "remember"), b"{}");
            assert_eq!(output.status.code(), Some(6), "{status} {body}: {output:?}");
            assert!(output.stdout.is_empty());
            let error: Value = serde_json::from_slice(&output.stderr).unwrap();
            assert_eq!(error["error"]["code"], "invalid_response");
            assert!(
                error["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("reconcile mutations by receipt")
            );
            server.join().unwrap();
        }
    }
}

#[test]
fn invalid_utf8_and_truncated_bodies_are_response_integrity_failures() {
    for (body, length) in [(b"{\"x\":\"\xff\"}".as_slice(), 9), (b"{".as_slice(), 50)] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            receive(&mut stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            stream.write_all(body).unwrap();
        });
        let output = run(command(&url, "remember"), b"{}");
        assert_eq!(output.status.code(), Some(6), "{output:?}");
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["error"]["code"], "invalid_response");
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("reconcile mutations by receipt")
        );
        worker.join().unwrap();
    }
}

#[test]
fn first_slice_capability_refusals_map_to_conflict() {
    for (verb, feature) in [
        ("facts", "facts"),
        ("correct-facts", "correct_facts"),
        ("erasure", "erasure"),
        ("resume-erasure", "resume_erasure"),
        ("remember", "facts"),
        ("recall", "facts"),
        ("forget", "erasure"),
        ("forget", "facts"),
        ("consolidate", "facts"),
        ("consolidate", "consolidation_disposition"),
        ("recall", "temporal_expansion"),
    ] {
        let body = serde_json::json!({"error":{"code":"memory_capability_unavailable",
            "message":"capability unavailable","details":{"feature":feature}}});
        let (url, server) = serve(409, body.to_string());
        failure(
            run(command(&url, verb), b"{}"),
            4,
            "{\"error\":{\"code\":\"memory_capability_unavailable\",\"message\":\"capability unavailable\"}}\n",
        );
        server.join().unwrap();
    }
}

#[test]
fn visible_ascii_token_boundaries_are_accepted() {
    for token in ["z".to_owned(), "z".repeat(4096)] {
        let (url, server) = serve(200, "{}".into());
        let mut cmd = command(&url, "recall");
        cmd.env(TOKEN_ENV, &token);
        assert!(run(cmd, b"{}").status.success());
        let headers = server.join().unwrap().0;
        assert!(headers.contains(&format!("Bearer {token}\r\n")));
    }
}

#[test]
fn loopback_http_and_remote_https_pass_url_validation() {
    for url in [
        "http://localhost:1",
        "http://127.1.2.3:1",
        "http://[::1]:1",
        "https://example.com",
    ] {
        let mut cmd = command(url, "recall");
        cmd.env_remove(TOKEN_ENV);
        failure(
            run(cmd, b"{}"),
            2,
            "{\"error\":{\"code\":\"usage\",\"message\":\"token variable must contain 1..4096 visible ASCII bytes\"}}\n",
        );
    }
}

#[test]
fn output_error_diagnostics_use_the_credential_safety_check() {
    for token in ["output", "REDACTED", "\"", "\\", "e"] {
        let (url, server) = serve(200, "{}".into());
        let mut cmd = command(&url, "remember");
        cmd.env(TOKEN_ENV, token);
        let mut child = cmd.spawn().unwrap();
        drop(child.stdout.take());
        child.stdin.take().unwrap().write_all(b"{}").unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(6), "{output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stderr.contains(token), "{token:?}: {stderr}");
        if !stderr.is_empty() {
            let error: Value = serde_json::from_str(&stderr).unwrap();
            assert_eq!(
                error["error"]["code"],
                if token == "output" {
                    "[REDACTED]_error"
                } else {
                    "output_error"
                }
            );
        }
        server.join().unwrap();
    }
}

#[cfg(windows)]
#[test]
fn request_paths_accept_unpaired_surrogates() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory
        .path()
        .join(OsString::from_wide(&[0xD800, 0x2e, 0x6a, 0x73, 0x6f, 0x6e]));
    std::fs::write(&path, b"{}").unwrap();
    let (url, server) = serve(200, "{}".into());
    let mut cmd = binary();
    cmd.args([
        "--url",
        &url,
        "--binding",
        "notes",
        "--token-env",
        TOKEN_ENV,
        "recall",
        "--request",
    ])
    .arg(&path);
    let output = run(cmd, b"");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(server.join().unwrap().1, b"{}");
}
