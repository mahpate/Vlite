#![cfg(all(feature = "mcp", feature = "tools"))]

use serde_json::{Value, json};
use std::{
    fs::File,
    io::{self, Write},
    path::Path,
    process::{Command, Output, Stdio},
    sync::{Mutex, MutexGuard, PoisonError},
    thread,
};
use vlite::Database;

// The documented limits, written out so that a change of either constant fails here.
const MCP_CAP: usize = 128 * 1024 * 1024;
const SHELL_CAP: usize = 16 * 1024 * 1024;
const MCP_TOO_LONG: &str = "request line exceeds 134217728 bytes";
const SHELL_TOO_LONG: &str = "error: invalid_input: line exceeds 16777216 bytes\n";

// Some inputs here are hundreds of MiB long. Holding this lock while one is written and its
// process runs makes them run one at a time, which bounds what a loaded runner needs.
static HEAVY: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    HEAVY.lock().unwrap_or_else(PoisonError::into_inner)
}

fn fixture(path: &Path) {
    Database::create(path, 2).unwrap().close().unwrap();
}

// A piece of what a process reads. A `Fill` is streamed from a small buffer, so that a
// line as long as the cap is never held in memory.
enum Part {
    Bytes(Vec<u8>),
    Fill(u8, usize),
}

#[derive(Default)]
struct Input(Vec<Part>);

impl Input {
    fn bytes(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.0.push(Part::Bytes(bytes.into()));
        self
    }

    fn fill(mut self, byte: u8, count: usize) -> Self {
        self.0.push(Part::Fill(byte, count));
        self
    }

    // A request and its line terminator.
    fn line(self, request: Vec<u8>, terminator: &str) -> Self {
        self.bytes(request).bytes(terminator)
    }

    // A `ping` of exactly `size` bytes, padded with the insignificant whitespace JSON
    // allows after a value, and its line terminator.
    fn padded_ping(self, id: u64, size: usize, terminator: &str) -> Self {
        let request = plain_ping(id);
        let padding = size - request.len();
        self.bytes(request).fill(b' ', padding).bytes(terminator)
    }

    fn write_to(&self, out: &mut impl Write) -> io::Result<()> {
        for part in &self.0 {
            match part {
                Part::Bytes(bytes) => out.write_all(bytes)?,
                Part::Fill(byte, count) => {
                    let chunk = vec![*byte; (*count).min(1 << 20)];
                    let mut left = *count;
                    while left > 0 {
                        let size = left.min(chunk.len());
                        out.write_all(&chunk[..size])?;
                        left -= size;
                    }
                }
            }
        }
        Ok(())
    }
}

// Feeds `input` from a thread while the output is drained, so that neither side can
// stall on a full pipe. The write result is returned too: it fails when the process
// stops reading before the end of its input.
fn run(binary: &str, args: &[&str], input: &Input) -> (Output, io::Result<()>) {
    let mut child = Command::new(binary)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    thread::scope(|scope| {
        let writer = scope.spawn(move || input.write_to(&mut stdin));
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

// Runs `vlite-mcp` over `input`, which it may leave unread, and returns the process output.
fn mcp_run(input: &Input) -> (Output, io::Result<()>) {
    let directory = tempfile::tempdir_in(".").unwrap();
    let path = directory.path().join("lines.vlite");
    fixture(&path);
    run(
        env!("CARGO_BIN_EXE_vlite-mcp"),
        &["--db", path.to_str().unwrap(), "--read-only"],
        input,
    )
}

// What `vlite-mcp` wrote to stdout, one JSON value per line.
fn answers(output: &Output) -> Vec<Value> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

// Runs `vlite-mcp` over `input` and returns what it wrote to stdout. The run must end
// normally with nothing on stderr, having read all of its input.
fn mcp(input: &Input) -> Vec<Value> {
    let (output, written) = mcp_run(input);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(output.stderr.is_empty(), "{}", describe(&output));
    written.unwrap();
    answers(&output)
}

// The one test at the real cap. About 256 MiB are streamed and none is held in memory:
// the lines of 128 MiB are mostly whitespace written from a small buffer.
#[test]
fn mcp_serves_a_line_of_exactly_the_cap_and_refuses_one_byte_more() {
    let _serial = serial();
    let input = Input::default()
        .line(plain_ping(1), "\n")
        // The CR of a CRLF is not content, so this line is exactly the cap.
        .padded_ping(2, MCP_CAP, "\r\n")
        .padded_ping(3, MCP_CAP + 1, "\n")
        .line(plain_ping(4), "\n")
        // The last line needs no newline.
        .bytes(plain_ping(5));
    assert_eq!(
        mcp(&input),
        [pong(1), pong(2), too_long(), pong(4), pong(5)]
    );
}

// Lines that are longer than the cap by some distance, which have to be drained, with
// CRLF terminators and as the last line without any terminator.
#[test]
fn mcp_drains_longer_lines_with_crlf_and_at_the_end_of_input() {
    let _serial = serial();
    let input = Input::default()
        .line(plain_ping(1), "\r\n")
        .padded_ping(2, MCP_CAP + 4096, "\r\n")
        .line(plain_ping(3), "\r\n")
        .padded_ping(4, MCP_CAP + 9000, "");
    assert_eq!(mcp(&input), [pong(1), too_long(), pong(3), too_long()]);
}

#[test]
fn mcp_skips_blank_lines_and_answers_garbage_with_a_parse_error() {
    let input = Input::default()
        .bytes("\n   \r\n\t\n")
        .bytes("not json\n")
        .line(plain_ping(1), "\n")
        .bytes("\r\n\n")
        .line(plain_ping(2), "\n");
    assert_eq!(
        mcp(&input),
        [
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}),
            pong(1),
            pong(2),
        ]
    );
}

// A line that fits but is not UTF-8 is a read error, as it was when std read the lines: the
// run ends with exit code 1 and that error, after the requests before the line were answered.
#[test]
fn mcp_exits_with_the_utf8_error_of_a_line_that_fits_after_answering_earlier_requests() {
    let inputs = [
        // Invalid bytes alone on a line, between two requests.
        Input::default()
            .line(plain_ping(1), "\n")
            .bytes(b"\xff\xfe\n".to_vec())
            .line(plain_ping(3), "\n"),
        // A cut-off character inside a request, which ends the input without a newline.
        Input::default()
            .line(plain_ping(1), "\r\n")
            .bytes(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\xc3".to_vec()),
    ];
    for input in inputs {
        let (output, _) = mcp_run(&input);
        assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
        assert_eq!(answers(&output), [pong(1)], "{}", describe(&output));
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "vlite-mcp: stream did not contain valid UTF-8\n"
        );
    }
}

fn put(id: u64) -> String {
    format!("put {{\"id\":{id},\"vector\":[{id},1]}}\n")
}

struct Ran {
    output: Output,
    first_put: bool,
    second_put: bool,
}

// Runs `vlite-shell` on `script`, given as a script file or on stdin, against a database in
// which the script may store the ids 1 and 2.
fn shell(from_script: bool, script: Input) -> Ran {
    let _serial = serial();
    let directory = tempfile::tempdir_in(".").unwrap();
    let path = directory.path().join("lines.vlite");
    fixture(&path);
    let script_path = directory.path().join("script.txt");
    let mut args = vec!["--db", path.to_str().unwrap()];
    let input = if from_script {
        script
            .write_to(&mut File::create(&script_path).unwrap())
            .unwrap();
        args.extend(["--script", script_path.to_str().unwrap()]);
        Input::default()
    } else {
        script
    };
    // On stdin the shell may stop before the end of the input, so the write result is
    // not checked.
    let (output, _) = run(env!("CARGO_BIN_EXE_vlite-shell"), &args, &input);
    let database = Database::open(&path).unwrap();
    Ran {
        output,
        first_put: database.get(1).unwrap().is_some(),
        second_put: database.get(2).unwrap().is_some(),
    }
}

// `put 1`, a comment line of `size` bytes and `put 2`.
fn around_a_comment(from_script: bool, size: usize) -> Ran {
    shell(
        from_script,
        Input::default()
            .bytes(put(1))
            .bytes("#")
            .fill(b'x', size - 1)
            .bytes("\n")
            .bytes(put(2)),
    )
}

#[test]
fn shell_ignores_a_comment_line_of_exactly_the_cap_in_a_script_and_on_stdin() {
    for from_script in [true, false] {
        let ran = around_a_comment(from_script, SHELL_CAP);
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
        let ran = around_a_comment(from_script, SHELL_CAP + 1);
        let output = &ran.output;
        assert_eq!(output.status.code(), Some(1), "{}", describe(output));
        assert_eq!(output.stdout, b"ok\n");
        assert_eq!(String::from_utf8_lossy(&output.stderr), SHELL_TOO_LONG);
        assert!(ran.first_put, "the command before the line ran");
        assert!(!ran.second_put, "the command after the line did not");
    }
}

// A line that fits but is not UTF-8 is a read error, as it was when std read the line: the run
// ends with exit code 1 and that error, the commands before the line having run. A comment is
// read, and so checked, like any other line.
#[test]
fn shell_exits_with_the_utf8_error_of_a_line_that_fits_in_a_script_and_on_stdin() {
    for from_script in [true, false] {
        for bad in [&b"put \xff\n"[..], &b"# \xff\n"[..]] {
            let ran = shell(
                from_script,
                Input::default()
                    .bytes(put(1))
                    .bytes(bad.to_vec())
                    .bytes(put(2)),
            );
            let output = &ran.output;
            assert_eq!(output.status.code(), Some(1), "{}", describe(output));
            assert_eq!(output.stdout, b"ok\n");
            assert_eq!(
                String::from_utf8_lossy(&output.stderr),
                "error: stream did not contain valid UTF-8\n"
            );
            assert!(ran.first_put, "the command before the line ran");
            assert!(!ran.second_put, "the command after the line did not");
        }
    }
}
