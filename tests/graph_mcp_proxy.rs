//! ITEM-043 (feature-agent-graph EPIC-009/011; `mcp-proxy`, opt-in):
//! `vlite-mcp --server URL [--token-env VAR]` forwards the graph tools to a
//! served `vlite-server` without opening any file.
//!
//! - Tool parity: the proxy lists exactly the graph tools of a local
//!   graph-enabled session and returns the same structured results for the
//!   same data (timings masked; retrieve adds the server's `redacted` count).
//! - Authorization is the server's, per graph: a credential bound to one
//!   graph sees and reads only it, cannot write without `graph_write`, has
//!   evidence from unreadable collections redacted, and a credential without
//!   graph scope gets no graph tools.
//! - Graph tools are listed only when the server reports a graph-enabled
//!   database (`GET /v1/graphs`; 409 `graphs_not_enabled` lists none).
//! - The token comes from the named environment variable and is never
//!   echoed in errors or diagnostics.
//! - Concurrent agents share the single server-side owner.

use std::{
    io::{BufRead, BufReader, Write},
    net::SocketAddr,
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use vlite::{
    CollectionSettings, CollectionSpec, CreateOptions, Database, FormatVersion,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "graph_proxy_admin_token_0123456789abcdef";
const READ_ONLY: &str = "graph_proxy_read_only_token_0123456789ab";
const BIN: &str = env!("CARGO_BIN_EXE_vlite-mcp");

/// One newline-delimited JSON-RPC session with a `vlite-mcp` child.
struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Session {
    fn spawn(args: &[&str], env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(BIN);
        command.args(args);
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut session = Self {
            child,
            stdin,
            stdout,
            next: 0,
        };
        let init = session.request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "proxy-test", "version": "0"}}),
        );
        assert!(init["result"]["serverInfo"].is_object(), "{init}");
        session
    }

    fn proxy(url: &str, token: Option<&str>, writable: bool) -> Self {
        let mode = if writable {
            "--writable"
        } else {
            "--read-only"
        };
        match token {
            Some(token) => Self::spawn(
                &["--server", url, "--token-env", "PROXY_TEST_TOKEN", mode],
                &[("PROXY_TEST_TOKEN", token)],
            ),
            None => Self::spawn(&["--server", url, mode], &[]),
        }
    }

    fn local(path: &Path, writable: bool) -> Self {
        let mode = if writable {
            "--writable"
        } else {
            "--read-only"
        };
        Self::spawn(&["--db", path.to_str().unwrap(), mode], &[])
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let message =
            json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params});
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("{line}"));
        assert_eq!(response["id"], self.next, "{response}");
        response
    }

    fn tools(&mut self) -> Vec<Value> {
        let response = self.request("tools/list", json!({}));
        response["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("{response}"))
            .clone()
    }

    /// The structured result, or the error text.
    fn call(&mut self, name: &str, arguments: Value) -> Result<Value, String> {
        let response = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        let result = &response["result"];
        assert!(result["isError"].is_boolean(), "{response}");
        if result["isError"] == true {
            Err(result["content"][0]["text"].as_str().unwrap().to_owned())
        } else {
            assert_eq!(
                result["content"][0]["text"].as_str().unwrap(),
                result["structuredContent"].to_string()
            );
            Ok(result["structuredContent"].clone())
        }
    }

    fn ok(&mut self, name: &str, arguments: Value) -> Value {
        self.call(name, arguments.clone())
            .unwrap_or_else(|error| panic!("{name} {arguments}: {error}"))
    }

    fn err(&mut self, name: &str, arguments: Value) -> String {
        match self.call(name, arguments.clone()) {
            Ok(value) => panic!("{name} {arguments} succeeded: {value}"),
            Err(error) => error,
        }
    }

    /// Closes stdin and returns stderr after a clean exit.
    fn finish(self) -> String {
        let Self { child, stdin, .. } = self;
        drop(stdin);
        let output = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{stderr}");
        stderr
    }
}

/// Records 1..=4 in `default` and record 1 in `secret`; graph "g" has nodes
/// n1..n4 (record n attached to node n, secret record 1 to n2) and edges
/// n1-LINKS->n2-LINKS->n3; graph "h" has one node. Built through a local
/// writable MCP session, so the file is identical for every copy.
fn build(path: &Path) {
    let mut db = Database::create_with_graph(path, 2, CreateOptions::default()).unwrap();
    for id in 1..=4u64 {
        db.put(id, &[id as f32, 1.0], format!("r{id}").as_bytes())
            .unwrap();
    }
    db.create_collection(
        "secret",
        CollectionSpec {
            dimensions: 2,
            settings: CollectionSettings::default(),
        },
    )
    .unwrap();
    db.collection_mut("secret")
        .unwrap()
        .put(1, &[2.0, 1.0], b"classified")
        .unwrap();
    db.close().unwrap();
    let mut local = Session::local(path, true);
    for graph in ["g", "h"] {
        local.ok("vlite_graph_create", json!({"graph": graph}));
    }
    let mut ops = Vec::new();
    for id in 1..=4u64 {
        ops.push(
            json!({"op": "create_node", "key": format!("n{id}"), "labels": ["Doc"],
            "properties": {"rank": {"type": "integer", "value": id.to_string()}}}),
        );
        ops.push(
            json!({"op": "attach", "node": format!("n{id}"), "collection": "default",
            "record": id.to_string()}),
        );
    }
    ops.push(json!({"op": "attach", "node": "n2", "collection": "secret", "record": "1"}));
    ops.push(json!({"op": "create_edge", "key": "e12", "source": "n1",
        "relationship": "LINKS", "destination": "n2"}));
    ops.push(
        json!({"op": "create_edge", "source": "n2", "relationship": "LINKS",
        "destination": "n3"}),
    );
    local.ok("vlite_graph_write", json!({"graph": "g", "ops": ops}));
    local.ok(
        "vlite_graph_write",
        json!({"graph": "h", "ops": [{"op": "create_node", "key": "x"}]}),
    );
    local.finish();
}

/// A `vlite-server` router over `path`, served on 127.0.0.1.
struct Server {
    url: String,
    runtime: tokio::runtime::Runtime,
}

impl Server {
    fn start(path: &Path, admin_directory: &Path) -> Self {
        std::fs::create_dir_all(admin_directory).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let service = Service::new(Database::open(path).unwrap());
        let options = ServerOptions {
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            read_only_token: Some(BearerToken::new(READ_ONLY).unwrap()),
            admin_directory: Some(admin_directory.to_path_buf()),
            ..ServerOptions::default()
        };
        let app = {
            let _guard = runtime.enter();
            configured_admin_router(service, options).unwrap()
        };
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let address: SocketAddr = listener.local_addr().unwrap();
        runtime.spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            url: format!("http://{address}"),
            runtime,
        }
    }

    /// A scoped credential's token.
    fn credential(&self, request: Value) -> String {
        let url = format!("{}/v1/admin/credentials", self.url);
        let mut request = request;
        request["name"] = json!("agent");
        request["expires_unix_ms"] = json!(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
                + 3_600_000
        );
        let body: Value = self.runtime.block_on(async move {
            let response = reqwest::Client::new()
                .post(url)
                .bearer_auth(ADMIN)
                .json(&request)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 201);
            response.json().await.unwrap()
        });
        body["token"].as_str().unwrap().to_owned()
    }
}

struct Fixture {
    directory: TempDir,
    server: Server,
}

impl Fixture {
    /// `served.vlite` behind the server and an identical `local.vlite`.
    fn new() -> Self {
        let directory = tempdir().unwrap();
        let served = directory.path().join("served.vlite");
        build(&served);
        std::fs::copy(&served, directory.path().join("local.vlite")).unwrap();
        let server = Server::start(&served, &directory.path().join("admin"));
        Self { directory, server }
    }

    fn local_path(&self) -> std::path::PathBuf {
        self.directory.path().join("local.vlite")
    }
}

/// Zeroes timings so reads of the same data compare equal.
fn mask(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, field) in object.iter_mut() {
                if key.ends_with("_ms") && field.is_number() && key != "deadline_ms" {
                    *field = json!(0);
                } else {
                    mask(field);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(mask),
        _ => {}
    }
}

fn names(tools: &[Value]) -> Vec<&str> {
    tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect()
}

/// Read calls covering every read tool, in both local and proxied sessions.
fn read_calls() -> Vec<(&'static str, Value)> {
    vec![
        ("vlite_graph_list", json!({})),
        (
            "vlite_graph_get",
            json!({"graph": "g", "node": {"key": "n2"}}),
        ),
        (
            "vlite_graph_get",
            json!({"graph": "g", "edge": {"key": "e12"}}),
        ),
        (
            "vlite_graph_find_nodes",
            json!({"graph": "g", "label": "Doc", "limit": 3}),
        ),
        (
            "vlite_graph_neighbors",
            json!({"graph": "g", "node": {"key": "n2"}, "direction": "both"}),
        ),
        (
            "vlite_graph_traverse",
            json!({"graph": "g", "start": [{"key": "n1"}], "include_paths": true,
                "limits": {"max_hops": 2}}),
        ),
        (
            "vlite_graph_shortest_path",
            json!({"graph": "g", "source": {"key": "n1"}, "destination": {"key": "n3"}}),
        ),
        (
            "vlite_graph_traverse",
            json!({"graph": "g", "start": [{"key": "n1"}], "explain": true}),
        ),
        ("vlite_graph_schema", json!({"graph": "g"})),
        ("vlite_graph_schema", json!({"graph": "g", "budget": 1})),
        (
            "vlite_graph_retrieve",
            json!({"graph": "g",
                "pivot": {"collection": "default", "vector": [1.0, 1.0], "k": 1},
                "expansion": {"direction": "out"}, "limits": {"max_hops": 2},
                "evidence": {"collections": ["default", "secret"]}}),
        ),
        (
            "vlite_graph_query",
            json!({"graph": "g", "query": "MATCH (n:Doc) WHERE n.rank >= 2 RETURN n LIMIT 10"}),
        ),
        (
            "vlite_graph_query",
            json!({"graph": "g", "query": "MATCH (n:Doc) RETURN n LIMIT 2", "explain": true}),
        ),
    ]
}

#[test]
fn proxied_tools_match_local_mode_on_the_same_data() {
    let fixture = Fixture::new();
    let local_path = fixture.local_path();
    for writable in [false, true] {
        let mut local = Session::local(&local_path, writable);
        let mut proxy = Session::proxy(&fixture.server.url, Some(ADMIN), writable);
        let local_graph_tools: Vec<Value> = local
            .tools()
            .into_iter()
            .filter(|tool| tool["name"].as_str().unwrap().starts_with("vlite_graph_"))
            .collect();
        let proxied = proxy.tools();
        assert_eq!(proxied, local_graph_tools, "writable={writable}");
        assert_eq!(
            names(&proxied).contains(&"vlite_graph_put_schema"),
            writable
        );
        local.finish();
        proxy.finish();
    }

    let mut local = Session::local(&local_path, false);
    let mut proxy = Session::proxy(&fixture.server.url, Some(READ_ONLY), false);
    for (name, arguments) in read_calls() {
        let mut expected = local.ok(name, arguments.clone());
        let mut actual = proxy.ok(name, arguments.clone());
        if name == "vlite_graph_retrieve" {
            assert_eq!(
                actual["redacted"],
                json!({"count": 0, "collections": []}),
                "{actual}"
            );
            actual.as_object_mut().unwrap().remove("redacted");
            assert_eq!(
                expected["evidence"].as_array().unwrap().len(),
                4,
                "{expected}"
            );
        }
        mask(&mut expected);
        mask(&mut actual);
        assert_eq!(actual, expected, "{name} {arguments}");
    }
    // Errors keep their stable codes.
    for (name, arguments, code) in [
        (
            "vlite_graph_get",
            json!({"graph": "missing", "node": {"key": "n1"}}),
            "graph_not_found",
        ),
        (
            "vlite_graph_query",
            json!({"graph": "g", "query": "MATCH (n) DELETE n"}),
            "graph_query_unsupported",
        ),
        (
            "vlite_graph_get",
            json!({"graph": "g", "node": {"key": "n1"}, "edge": {"key": "e12"}}),
            "invalid_request",
        ),
    ] {
        assert!(local.err(name, arguments.clone()).starts_with(code));
        let error = proxy.err(name, arguments);
        assert!(error.starts_with(&format!("{code}: ")), "{error}");
    }
    // Read-only sessions refuse write tools before contacting the server;
    // vector tools are not proxied.
    let error = proxy.err("vlite_graph_create", json!({"graph": "z"}));
    assert!(error.starts_with("read_only: "), "{error}");
    assert_eq!(
        proxy.err("vlite_info", json!({})),
        "unknown tool: vlite_info"
    );
    local.finish();
    proxy.finish();

    // The same writes give the same results through both surfaces.
    let mut local = Session::local(&local_path, true);
    let mut proxy = Session::proxy(&fixture.server.url, Some(ADMIN), true);
    let writes = [
        (
            "vlite_graph_put_schema",
            json!({"graph": "g", "schema": {
                "labels": {"Doc": {"properties": {"rank": {"type": "integer", "index": "range"}}}},
                "relationships": {"LINKS": {"endpoints": [{"source": "Doc", "destination": "Doc"}],
                    "multiplicity": "unique_triple"}}}}),
        ),
        (
            "vlite_graph_write",
            json!({"graph": "g", "ops": [
                {"op": "ensure_edge", "source": "n1", "relationship": "LINKS", "destination": "n2"},
                {"op": "create_node", "key": "n5", "labels": ["Doc"]}]}),
        ),
        ("vlite_graph_create", json!({"graph": "k"})),
        ("vlite_graph_drop", json!({"graph": "k"})),
        ("vlite_graph_schema", json!({"graph": "g"})),
        (
            "vlite_graph_query",
            json!({"graph": "g", "query": "MATCH (n:Doc) WHERE n.rank >= 3 RETURN n LIMIT 10"}),
        ),
    ];
    for (name, arguments) in writes {
        let mut expected = local.ok(name, arguments.clone());
        let mut actual = proxy.ok(name, arguments.clone());
        mask(&mut expected);
        mask(&mut actual);
        assert_eq!(actual, expected, "{name} {arguments}");
    }
    local.finish();
    proxy.finish();
}

#[test]
fn authorization_is_per_graph_and_the_token_is_never_echoed() {
    let fixture = Fixture::new();
    let server = &fixture.server;
    let reader_g = server.credential(json!({"collections": ["default"],
        "operations": ["graph_read", "read"], "graphs": ["g"]}));
    let mut proxy = Session::proxy(&server.url, Some(&reader_g), true);
    // Every graph tool is listed; the server decides per call.
    assert!(names(&proxy.tools()).contains(&"vlite_graph_write"));
    let listed = proxy.ok("vlite_graph_list", json!({}));
    assert_eq!(listed["graphs"].as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(listed["graphs"][0]["name"], "g");
    proxy.ok(
        "vlite_graph_get",
        json!({"graph": "g", "node": {"key": "n1"}}),
    );
    for (name, arguments) in [
        (
            "vlite_graph_get",
            json!({"graph": "h", "node": {"key": "x"}}),
        ),
        ("vlite_graph_schema", json!({"graph": "h"})),
        (
            "vlite_graph_write",
            json!({"graph": "g", "ops": [{"op": "upsert_node", "key": "n9"}]}),
        ),
        (
            "vlite_graph_put_schema",
            json!({"graph": "g", "schema": {}}),
        ),
    ] {
        let error = proxy.err(name, arguments);
        assert!(error.starts_with("forbidden_scope: "), "{name}: {error}");
        assert!(!error.contains(&reader_g));
    }
    // Evidence from collections outside the binding is redacted, not leaked.
    let retrieved = proxy.ok(
        "vlite_graph_retrieve",
        json!({"graph": "g",
            "pivot": {"collection": "default", "vector": [1.0, 1.0], "k": 1},
            "expansion": {"direction": "out"}, "limits": {"max_hops": 2},
            "evidence": {"collections": ["default", "secret"]}}),
    );
    assert_eq!(
        retrieved["redacted"],
        json!({"count": 1, "collections": ["secret"]})
    );
    assert!(
        retrieved["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["collection"] == "default"),
        "{retrieved}"
    );
    let stderr = proxy.finish();
    assert!(!stderr.contains(&reader_g));

    // No graph scope: no graph tools.
    let vectors_only = server.credential(json!({"collections": ["*"],
        "operations": ["read", "search"]}));
    let mut proxy = Session::proxy(&server.url, Some(&vectors_only), false);
    assert!(proxy.tools().is_empty());
    proxy.finish();

    // A wrong token fails the probe without echoing the token.
    let wrong = "graph_proxy_wrong_token_0123456789abcdef";
    let mut proxy = Session::proxy(&server.url, Some(wrong), false);
    let response = proxy.request("tools/list", json!({}));
    let message = response["error"]["message"].as_str().unwrap();
    assert!(message.starts_with("unauthorized"), "{response}");
    assert!(!response.to_string().contains(wrong));
    let error = proxy.err("vlite_graph_list", json!({}));
    assert!(error.starts_with("unauthorized"), "{error}");
    assert!(!error.contains(wrong));
    let stderr = proxy.finish();
    assert!(!stderr.contains(wrong));

    // A missing token variable fails at startup, naming only the variable.
    let output = Command::new(BIN)
        .args([
            "--server",
            &server.url,
            "--token-env",
            "PROXY_TEST_UNSET_TOKEN",
        ])
        .env_remove("PROXY_TEST_UNSET_TOKEN")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("PROXY_TEST_UNSET_TOKEN"));
    // Proxy mode never opens a file and rejects file options and URLs with
    // embedded credentials.
    for args in [
        vec!["--server", &server.url, "--db", "x.vlite"],
        vec!["--server", "ftp://127.0.0.1/"],
        vec!["--server", "http://user:pass@127.0.0.1/"],
        vec!["--token-env", "PROXY_TEST_TOKEN", "--db", "x.vlite"],
    ] {
        let output = Command::new(BIN)
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?}");
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("pass"),
            "{args:?}"
        );
    }
    assert!(!fixture.directory.path().join("x.vlite").exists());
}

#[test]
fn vector_only_servers_list_no_graph_tools() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("vectors.vlite");
    let mut db = Database::create_with_format(&path, 2, FormatVersion::V5).unwrap();
    db.put(1, &[1.0, 1.0], b"r1").unwrap();
    db.close().unwrap();
    let server = Server::start(&path, &directory.path().join("admin"));
    let mut proxy = Session::proxy(&server.url, Some(ADMIN), true);
    assert!(proxy.tools().is_empty());
    let error = proxy.err("vlite_graph_list", json!({}));
    assert!(error.starts_with("graphs_not_enabled: "), "{error}");
    proxy.finish();
    // An unreachable server is a tool error, not a crash.
    let mut proxy = Session::proxy("http://127.0.0.1:9", None, false);
    let response = proxy.request("tools/list", json!({}));
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("server_unavailable: "),
        "{response}"
    );
    proxy.finish();
}

#[test]
fn concurrent_agents_share_the_server() {
    let fixture = Fixture::new();
    let url = fixture.server.url.clone();
    let mut admin = Session::proxy(&url, Some(ADMIN), true);
    admin.ok(
        "vlite_graph_put_schema",
        json!({"graph": "g", "schema": {"relationships": {"SEES": {
            "endpoints": [{"source": "Agent", "destination": "Doc"}],
            "multiplicity": "unique_triple"}}}}),
    );
    admin.finish();
    let agents: Vec<_> = (0..6)
        .map(|agent| {
            let url = url.clone();
            std::thread::spawn(move || {
                let writer = agent % 2 == 0;
                let mut proxy = Session::proxy(&url, Some(ADMIN), writer);
                for round in 0..10 {
                    if writer {
                        proxy.ok(
                            "vlite_graph_write",
                            json!({"graph": "g", "ops": [
                                {"op": "create_node", "key": format!("a{agent}r{round}"), "labels": ["Agent"]},
                                {"op": "ensure_edge", "source": format!("a{agent}r{round}"),
                                    "relationship": "SEES", "destination": "n1"}]}),
                        );
                    } else {
                        let found = proxy.ok(
                            "vlite_graph_query",
                            json!({"graph": "g", "query": "MATCH (n:Doc) RETURN n LIMIT 10"}),
                        );
                        assert_eq!(found["nodes"].as_array().unwrap().len(), 4, "{found}");
                        proxy.ok(
                            "vlite_graph_neighbors",
                            json!({"graph": "g", "node": {"key": "n1"}, "direction": "in"}),
                        );
                    }
                }
                proxy.finish();
            })
        })
        .collect();
    for agent in agents {
        agent.join().unwrap();
    }
    let mut proxy = Session::proxy(&url, Some(READ_ONLY), false);
    let listed = proxy.ok("vlite_graph_list", json!({}));
    let g = &listed["graphs"].as_array().unwrap()[0];
    assert_eq!(g["name"], "g");
    assert_eq!(g["node_count"], 4 + 30);
    assert_eq!(g["edge_count"], 2 + 30);
    proxy.finish();
}
