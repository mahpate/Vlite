#![cfg(all(feature = "mcp", feature = "tools"))]

use serde_json::{Value, json};
use std::{
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
    sync::{Mutex, MutexGuard, PoisonError},
    thread,
};
use vlite::Database;

// The documented limits, written out so that a change of either constant fails here.
const MCP_CAP: usize = 64 * 1024 * 1024;
const SHELL_CAP: usize = 16 * 1024 * 1024;
const MCP_TOO_LONG: &str = "request line exceeds 67108864 bytes";
const SHELL_TOO_LONG: &str = "error: invalid_input: line exceeds 16777216 bytes\n";

// Every payload here is tens of MiB. Holding this lock from before a payload is
// built until its process has ended runs one at a time, which bounds the memory
// a loaded runner needs.
static HEAVY: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    HEAVY.lock().unwrap_or_else(PoisonError::into_inner)
}

fn fixture(path: &Path) {
    Database::create(path, 2).unwrap().close().unwrap();
}

// Feeds `input` from a thread while the output is drained, so that neither side
// can stall on a full pipe. The write result is returned too: it fails when the
// process stops reading before the end of its input.
fn run(binary: &str, args: &[&str], input: &[&[u8]]) -> (Output, std::io::Result<()>) {
    let mut child = Command::new(binary)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    thread::scope(|scope| {
        let writer = scope.spawn(move || input.iter().try_for_each(|part| stdin.write_all(part)));
        let output = child.wait_with_output().unwrap();
        (output, writer.join().unwrap())
    })
}

fn describe(output: &Output) -> String {
    format!(
        "{:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

// A `ping` request of exactly `size` bytes, padded inside a `params` string.
fn ping(id: u64, size: usize) -> Vec<u8> {
    let mut line =
        format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"ping","params":{{"pad":""#).into_bytes();
    let tail = br#""}}"#;
    let padding = size - line.len() - tail.len();
    line.resize(line.len() + padding, b'x');
    line.extend_from_slice(tail);
    assert_eq!(line.len(), size);
    line
}

fn plain_ping(id: u64) -> Vec<u8> {
    json!({"jsonrpc":"2.0","id":id,"method":"ping"})
        .to_string()
        .into_bytes()
}

fn pong(id: u64) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":{}})
}

fn too_long() -> Value {
    json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":MCP_TOO_LONG}})
}

// Runs `vlite-mcp` over `input` and returns what it wrote to stdout, one JSON
// value per line. The run must end normally with nothing on stderr, having read
// all of its input.
fn mcp(input: &[&[u8]]) -> Vec<Value> {
    let directory = tempfile::tempdir_in(".").unwrap();
    let path = directory.path().join("lines.vlite");
    fixture(&path);
    let (output, written) = run(
        env!("CARGO_BIN_EXE_vlite-mcp"),
        &["--db", path.to_str().unwrap(), "--read-only"],
        input,
    );
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(output.stderr.is_empty(), "{}", describe(&output));
    written.unwrap();
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn mcp_answers_a_request_line_of_exactly_the_cap() {
    let _serial = serial();
    let responses = mcp(&[&ping(1, MCP_CAP), b"\n", &plain_ping(2), b"\n"]);
    assert_eq!(responses, [pong(1), pong(2)]);
}

#[test]
fn mcp_answers_a_crlf_request_line_of_exactly_the_cap() {
    let _serial = serial();
    let responses = mcp(&[&ping(1, MCP_CAP), b"\r\n", &plain_ping(2), b"\r\n"]);
    assert_eq!(responses, [pong(1), pong(2)]);
}

#[test]
fn mcp_refuses_an_overlong_line_and_serves_the_next_one() {
    let _serial = serial();
    let responses = mcp(&[
        &plain_ping(1),
        b"\n",
        &ping(2, MCP_CAP + 1),
        b"\n",
        &plain_ping(3),
        b"\n",
    ]);
    assert_eq!(responses, [pong(1), too_long(), pong(3)]);
}

#[test]
fn mcp_refuses_an_overlong_crlf_line_and_serves_the_next_one() {
    let _serial = serial();
    let responses = mcp(&[
        &plain_ping(1),
        b"\r\n",
        &ping(2, MCP_CAP + 1),
        b"\r\n",
        &plain_ping(3),
        b"\r\n",
    ]);
    assert_eq!(responses, [pong(1), too_long(), pong(3)]);
}

#[test]
fn mcp_serves_an_unterminated_last_line_after_an_overlong_one() {
    let _serial = serial();
    let responses = mcp(&[&ping(1, MCP_CAP + 1), b"\n", &plain_ping(2)]);
    assert_eq!(responses, [too_long(), pong(2)]);
}

#[test]
fn mcp_refuses_an_overlong_last_line_without_a_newline_and_exits_cleanly() {
    let _serial = serial();
    let responses = mcp(&[&plain_ping(1), b"\n", &ping(2, MCP_CAP + 1)]);
    assert_eq!(responses, [pong(1), too_long()]);
}

#[test]
fn mcp_skips_blank_lines_and_answers_garbage_with_a_parse_error() {
    let responses = mcp(&[
        b"\n   \r\n\t\n",
        b"not json\n",
        &plain_ping(1),
        b"\r\n\n",
        &plain_ping(2),
    ]);
    assert_eq!(
        responses,
        [
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}),
            pong(1),
            pong(2),
        ]
    );
}

fn put(id: u64) -> Vec<u8> {
    format!("put {{\"id\":{id},\"vector\":[{id},1]}}\n").into_bytes()
}

// A comment line of `size` bytes, not counting its newline.
fn comment(size: usize) -> Vec<u8> {
    let mut line = vec![b'x'; size];
    line[0] = b'#';
    line.push(b'\n');
    line
}

struct Ran {
    output: Output,
    first_put: bool,
    second_put: bool,
}

// Runs `vlite-shell` on `put 1`, a comment of `size` bytes and `put 2`, given
// as a script file or on stdin.
fn shell(from_script: bool, size: usize) -> Ran {
    let _serial = serial();
    let directory = tempfile::tempdir_in(".").unwrap();
    let path = directory.path().join("lines.vlite");
    fixture(&path);
    let script = [put(1), comment(size), put(2)];
    let script_path = directory.path().join("script.txt");
    let mut args = vec!["--db", path.to_str().unwrap()];
    let input: Vec<&[u8]> = if from_script {
        std::fs::write(&script_path, script.concat()).unwrap();
        args.extend(["--script", script_path.to_str().unwrap()]);
        Vec::new()
    } else {
        script.iter().map(Vec::as_slice).collect()
    };
    // On stdin the shell may stop before the end of the input, so the write
    // result is not checked.
    let (output, _) = run(env!("CARGO_BIN_EXE_vlite-shell"), &args, &input);
    let database = Database::open(&path).unwrap();
    Ran {
        output,
        first_put: database.get(1).unwrap().is_some(),
        second_put: database.get(2).unwrap().is_some(),
    }
}

#[test]
fn shell_ignores_a_comment_line_of_exactly_the_cap_in_a_script_and_on_stdin() {
    for from_script in [true, false] {
        let ran = shell(from_script, SHELL_CAP);
        let output = &ran.output;
        assert_eq!(output.status.code(), Some(0), "{}", describe(output));
        assert_eq!(output.stdout, b"ok\nok\n");
        assert!(output.stderr.is_empty(), "{}", describe(output));
        assert!(ran.first_put && ran.second_put, "script: {from_script}");
    }
}

#[test]
fn shell_fails_on_an_overlong_line_in_a_script_and_on_stdin() {
    for from_script in [true, false] {
        let ran = shell(from_script, SHELL_CAP + 1);
        let output = &ran.output;
        assert_eq!(output.status.code(), Some(1), "{}", describe(output));
        assert_eq!(output.stdout, b"ok\n");
        assert_eq!(String::from_utf8_lossy(&output.stderr), SHELL_TOO_LONG);
        assert!(ran.first_put, "the command before the line ran");
        assert!(!ran.second_put, "the command after the line did not");
    }
}
