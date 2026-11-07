#![cfg(all(feature = "mcp", feature = "tools"))]

//! The shared key-value dialect goldens replayed against the local MCP server and the shell.
//!
//! The C ABI, Python, Node and WASM bindings replay the same file, so a change to the dialect
//! of any one surface fails a test instead of drifting silently. The `comment` in
//! `fixtures/kv/kv_dialect_goldens.json` describes the format.

use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};
use vlite::{CollectionSettings, CollectionSpec, Database};

const GOLDENS: &str = include_str!("fixtures/kv/kv_dialect_goldens.json");

#[derive(Clone, Copy)]
enum Surface {
    Mcp,
    Shell,
}

impl Surface {
    fn name(self) -> &'static str {
        match self {
            Surface::Mcp => "mcp",
            Surface::Shell => "shell",
        }
    }
}

// In a request or result `$ref:NAME` is the version bound earlier and
// `{"$repeat":[text,count],"$suffix":text}` a long string; a result string `$bind:NAME` or
// `$bind:NAME>OTHER` binds a commit version (greater than OTHER's).

fn expand(template: &Value, bound: &BTreeMap<String, String>) -> Value {
    match template {
        Value::String(text) => match text.strip_prefix("$ref:") {
            Some(name) => Value::String(
                bound
                    .get(name)
                    .unwrap_or_else(|| panic!("`{name}` is not bound"))
                    .clone(),
            ),
            None => template.clone(),
        },
        Value::Array(items) => Value::Array(items.iter().map(|item| expand(item, bound)).collect()),
        Value::Object(map) if map.contains_key("$repeat") => {
            let [text, count] = map["$repeat"].as_array().unwrap().as_slice() else {
                panic!("$repeat takes [text, count]");
            };
            let mut long = text
                .as_str()
                .unwrap()
                .repeat(count.as_u64().unwrap() as usize);
            long.push_str(map.get("$suffix").and_then(Value::as_str).unwrap_or(""));
            Value::String(long)
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(name, member)| (name.clone(), expand(member, bound)))
                .collect(),
        ),
        _ => template.clone(),
    }
}

fn matches(expected: &Value, actual: &Value, bound: &mut BTreeMap<String, String>) -> bool {
    match (expected, actual) {
        (Value::String(pattern), Value::String(text)) if pattern.starts_with("$bind:") => {
            let (name, floor) = match pattern["$bind:".len()..].split_once('>') {
                Some((name, floor)) => (name, Some(floor)),
                None => (&pattern["$bind:".len()..], None),
            };
            let Ok(version) = text.parse::<u64>() else {
                return false;
            };
            if version == 0 || version.to_string() != *text {
                return false;
            }
            if let Some(floor) = floor {
                let floor: u64 = bound[floor].parse().unwrap();
                if version <= floor {
                    return false;
                }
            }
            bound.insert(name.to_owned(), text.clone());
            true
        }
        (Value::Object(expected), Value::Object(actual)) => {
            expected.len() == actual.len()
                && expected.iter().all(|(name, member)| {
                    actual
                        .get(name)
                        .is_some_and(|other| matches(member, other, bound))
                })
        }
        (Value::Array(expected), Value::Array(actual)) => {
            expected.len() == actual.len()
                && expected
                    .iter()
                    .zip(actual)
                    .all(|(member, other)| matches(member, other, bound))
        }
        _ => expected == actual,
    }
}

fn skipped(node: &Value, surface: &str) -> bool {
    node.get("skip")
        .and_then(Value::as_array)
        .is_some_and(|surfaces| surfaces.iter().any(|name| name == surface))
}

fn brief(text: &str) -> String {
    match text.char_indices().nth(300) {
        Some((end, _)) => format!("{}... ({} bytes)", &text[..end], text.len()),
        None => text.to_owned(),
    }
}

/// A golden request as the tools take it: the operation, and the arguments without `op`.
fn split(request: &Value) -> (String, Value) {
    let mut arguments = request.clone();
    let operation = arguments
        .as_object_mut()
        .and_then(|fields| fields.remove("op"))
        .and_then(|operation| operation.as_str().map(str::to_owned))
        .expect("a golden request names its op");
    (operation, arguments)
}

/// A long-lived `vlite-mcp` process answering one tool call per line.
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    id: u64,
}

impl Mcp {
    fn start(path: &Path, read_only: bool) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_vlite-mcp"))
            .args(["--db", path.to_str().unwrap()])
            .arg(if read_only {
                "--read-only"
            } else {
                "--writable"
            })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            id: 0,
        }
    }

    fn call(&mut self, request: &Value) -> Result<Value, String> {
        let (operation, arguments) = split(request);
        self.id += 1;
        let message = json!({"jsonrpc":"2.0","id":self.id,"method":"tools/call","params":{
            "name":format!("vlite_kv_{operation}"),"arguments":arguments}});
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let response: Value =
            serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error}: {}", brief(&line)));
        assert_eq!(response["id"], self.id, "{}", brief(&line));
        let result = &response["result"];
        assert!(result.is_object(), "{}", brief(&line));
        if result["isError"] == true {
            let error = result["structuredContent"]["error"].as_str().unwrap();
            assert_eq!(result["content"][0]["text"], error);
            Err(error.to_owned())
        } else {
            assert_eq!(result["isError"], false, "{}", brief(&line));
            Ok(result["structuredContent"].clone())
        }
    }

    fn finish(mut self) {
        drop(self.stdin.take());
        let status = self.child.wait().unwrap();
        assert!(status.success(), "vlite-mcp exited with {status}");
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// One `vlite-shell` process per command: the shell stops at its first error.
fn shell(path: &Path, read_only: bool, request: &Value) -> Result<Value, String> {
    let (operation, arguments) = split(request);
    let mut command = Command::new(env!("CARGO_BIN_EXE_vlite-shell"));
    command.args(["--db", path.to_str().unwrap()]);
    if read_only {
        command.arg("--read-only");
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        // A broken pipe means the shell stopped reading; its output says why.
        let _ = writeln!(stdin, "kv-{operation} {arguments}");
    }
    let output = child.wait_with_output().unwrap();
    if output.status.success() {
        let text = String::from_utf8(output.stdout).unwrap();
        let mut lines = text.lines();
        let line = lines.next().expect("the shell prints one result line");
        assert!(lines.next().is_none(), "{}", brief(&text));
        Ok(serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {}", brief(line))))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{}", brief(&stderr));
        assert!(output.stdout.is_empty(), "{output:?}");
        let error = stderr
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("error: "))
            .unwrap_or_else(|| panic!("{}", brief(&stderr)));
        Err(error.to_owned())
    }
}

enum Tool<'a> {
    Mcp(Mcp),
    Shell { path: &'a Path, read_only: bool },
}

impl<'a> Tool<'a> {
    fn start(surface: Surface, path: &'a Path, read_only: bool) -> Self {
        match surface {
            Surface::Mcp => Tool::Mcp(Mcp::start(path, read_only)),
            Surface::Shell => Tool::Shell { path, read_only },
        }
    }

    fn call(&mut self, request: &Value) -> Result<Value, String> {
        match self {
            Tool::Mcp(session) => session.call(request),
            Tool::Shell { path, read_only } => shell(path, *read_only, request),
        }
    }

    fn finish(self) {
        if let Tool::Mcp(session) = self {
            session.finish();
        }
    }
}

fn create(path: &Path, sequence: &Value) {
    let mut db = Database::create(path, 1).unwrap();
    for collection in sequence["collections"].as_array().into_iter().flatten() {
        db.create_collection(
            collection["name"].as_str().unwrap(),
            CollectionSpec {
                dimensions: collection["dimensions"].as_u64().unwrap() as usize,
                settings: CollectionSettings::default(),
            },
        )
        .unwrap();
    }
    db.close().unwrap();
}

/// Replays one sequence on a fresh file database; returns the number of steps replayed.
fn replay(sequence: &Value, surface: Surface, directory: &Path) -> usize {
    let name = sequence["name"].as_str().unwrap();
    let read_only = sequence["mode"] == "read_only";
    let path = directory.join(format!("{name}-{}.vlite", surface.name()));
    create(&path, sequence);
    if read_only {
        let mut writer = Tool::start(surface, &path, false);
        for seed in sequence["seed"].as_array().unwrap() {
            let request = expand(seed, &BTreeMap::new());
            if let Err(error) = writer.call(&request) {
                panic!(
                    "{name}: seed {} failed: {error}",
                    brief(&request.to_string())
                );
            }
        }
        writer.finish();
    }
    let before = read_only.then(|| std::fs::read(&path).unwrap());
    let mut tool = Tool::start(surface, &path, read_only);
    let mut bound = BTreeMap::new();
    let mut replayed = 0;
    for step in sequence["steps"].as_array().unwrap() {
        if skipped(step, surface.name()) {
            continue;
        }
        replayed += 1;
        let step_name = step["name"].as_str().unwrap();
        let request = expand(&step["request"], &bound);
        let outcome = tool.call(&request);
        let context = || {
            let shown = match &outcome {
                Ok(result) => result.to_string(),
                Err(error) => error.clone(),
            };
            format!(
                "{} {name}/{step_name}: {} -> {}",
                surface.name(),
                brief(&request.to_string()),
                brief(&shown)
            )
        };
        match (step.get("result"), step.get("error")) {
            (Some(result), None) => {
                let actual = outcome.as_ref().unwrap_or_else(|_| panic!("{}", context()));
                let expected = expand(result, &bound);
                assert!(
                    matches(&expected, actual, &mut bound),
                    "{}; expected {}",
                    context(),
                    brief(&expected.to_string())
                );
            }
            (None, Some(code)) => {
                let error = outcome
                    .as_ref()
                    .err()
                    .unwrap_or_else(|| panic!("{}", context()));
                let (actual, message) = error
                    .split_once(": ")
                    .unwrap_or_else(|| panic!("an error is `code: message`; {}", context()));
                assert_eq!(code, actual, "{}", context());
                assert!(!message.is_empty(), "{}", context());
            }
            _ => panic!("{name}/{step_name} needs exactly one of result and error"),
        }
    }
    tool.finish();
    if let Some(before) = before {
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "{} {name}: a read-only session changed the file",
            surface.name()
        );
    }
    replayed
}

fn replay_all(surface: Surface) {
    let goldens: Value = serde_json::from_str(GOLDENS).unwrap();
    assert_eq!(goldens["format"], 1, "the replay reads format 1");
    let directory = tempfile::tempdir().unwrap();
    let mut replayed = 0;
    for sequence in goldens["sequences"].as_array().unwrap() {
        if !skipped(sequence, surface.name()) {
            replayed += replay(sequence, surface, directory.path());
        }
    }
    assert!(replayed >= 100, "only {replayed} golden steps replayed");
}

#[test]
fn the_mcp_server_replays_the_dialect_goldens() {
    replay_all(Surface::Mcp);
}

#[test]
fn the_shell_replays_the_dialect_goldens() {
    replay_all(Surface::Shell);
}
