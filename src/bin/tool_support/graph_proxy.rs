//! ITEM-043: `vlite-mcp --server URL` proxy mode (`mcp-proxy` feature,
//! opt-in; it implies `graph`; enabled in release builds since v0.5.0).
//!
//! The proxy never opens a database file. It lists the same graph tools as a
//! local graph-enabled session and forwards each call to the matching REST
//! graph route of a running `vlite-server`, so authorization (credential
//! operations, `graphs` binding, per-collection evidence redaction) is the
//! server's. Vector tools are not proxied: their REST routes have different
//! shapes, and agents that need them use a local session or the REST API.
//!
//! `tools/list` probes `GET /v1/graphs`: 200 lists the graph tools, and
//! `409 graphs_not_enabled` (vector-only database) or `403` (no graph scope)
//! lists none. The bearer token is read once from the environment variable
//! named by `--token-env`; it is sent only in the `Authorization` header,
//! redirects are not followed, and it is never logged or included in errors.

use std::time::Duration;

use reqwest::{
    Method, StatusCode,
    blocking::{Client, Response},
    redirect::Policy,
};
use serde_json::{Value, json};

use crate::graph_json::{self, Empty, Named, SchemaQueryIn, parse_value, split_graph};
use crate::graph_mcp;

/// Upper bound of one forwarded call; graph reads also have server deadlines.
const TIMEOUT: Duration = Duration::from_secs(120);

/// A REST failure: HTTP status (`None` when the server was unreachable) and
/// the `"<code>: <message>"` text returned to the agent.
struct Failure {
    status: Option<StatusCode>,
    message: String,
}

/// Connection to one `vlite-server`. Intentionally does not implement Debug.
pub(crate) struct Remote {
    base: String,
    token: Option<String>,
    client: Client,
}

impl Remote {
    /// `url` is `http://` or `https://` without credentials, query or
    /// fragment; a path prefix (for a reverse proxy) is kept.
    pub(crate) fn new(url: &str, token: Option<String>) -> Result<Self, String> {
        let rest = url
            .strip_prefix("http://")
            .or_else(|| url.strip_prefix("https://"))
            .ok_or("--server must be an http:// or https:// URL")?;
        let authority = rest.split('/').next().unwrap_or_default();
        if authority.is_empty() {
            return Err("--server must name a host".into());
        }
        if authority.contains('@') {
            return Err("--server must not contain credentials; use --token-env".into());
        }
        if url.contains(['?', '#']) {
            return Err("--server must not contain a query or fragment".into());
        }
        let client = Client::builder()
            .timeout(TIMEOUT)
            .redirect(Policy::none())
            .build()
            .map_err(|_| "cannot initialize the HTTP client")?;
        Ok(Self {
            base: url.trim_end_matches('/').to_owned(),
            token,
            client,
        })
    }

    fn send(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, Failure> {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().map_err(|error| Failure {
            status: None,
            message: format!("server_unavailable: {}", error.without_url()),
        })?;
        decode(response)
    }

    /// Graph tools when the server reports a graph-enabled database the
    /// credential may read graphs of; none otherwise.
    pub(crate) fn tool_definitions(&self, read_only: bool) -> Result<Vec<Value>, String> {
        match self.send(Method::GET, "/v1/graphs", None) {
            Ok(_) => Ok(graph_mcp::definitions(read_only)),
            Err(Failure {
                status: Some(StatusCode::CONFLICT | StatusCode::FORBIDDEN),
                ..
            }) => Ok(Vec::new()),
            Err(failure) => Err(failure.message),
        }
    }

    /// Runs a graph tool on the server; `None` when `name` is not one.
    pub(crate) fn call(
        &self,
        read_only: bool,
        name: &str,
        args: &Value,
    ) -> Option<Result<Value, String>> {
        graph_mcp::is_graph_tool(name).then(|| self.run(read_only, name, args))
    }

    fn run(&self, read_only: bool, name: &str, args: &Value) -> Result<Value, String> {
        let Some(object) = args.as_object() else {
            return Err(invalid("tool arguments must be a JSON object"));
        };
        if read_only && let Some(error) = graph_mcp::read_only_error(name) {
            return Err(error);
        }
        let forward = |method, path: &str, body| {
            self.send(method, path, body)
                .map_err(|failure| failure.message)
        };
        let text = args.to_string();
        match name {
            "vlite_graph_list" => {
                let _: Empty = graph_json::parse(&text).map_err(invalid)?;
                forward(Method::GET, "/v1/graphs", None)
            }
            "vlite_graph_create" => {
                let request: Named = graph_json::parse(&text).map_err(invalid)?;
                forward(
                    Method::POST,
                    "/v1/graphs",
                    Some(json!({"graph": request.graph})),
                )
            }
            "vlite_graph_drop" => {
                let request: Named = graph_json::parse(&text).map_err(invalid)?;
                forward(Method::DELETE, &graph_path(&request.graph, "")?, None)
            }
            "vlite_graph_schema" => {
                let (graph, body) = split_graph(args).map_err(invalid)?;
                let request: SchemaQueryIn = parse_value(body).map_err(invalid)?;
                let query = request
                    .budget
                    .map_or_else(String::new, |budget| format!("?budget={budget}"));
                forward(
                    Method::GET,
                    &graph_path(&graph, &format!("/schema{query}"))?,
                    None,
                )
            }
            _ => {
                let (graph, mut body) = split_graph(args).map_err(invalid)?;
                let (method, suffix) = match name {
                    "vlite_graph_write" => (Method::POST, "/batch"),
                    "vlite_graph_put_schema" => (Method::PUT, "/schema"),
                    "vlite_graph_retrieve" => (Method::POST, "/retrieve"),
                    "vlite_graph_query" => (Method::POST, "/query"),
                    _ => {
                        let command = graph_mcp::typed_read(name, object)
                            .unwrap_or_else(|| Err(format!("unknown tool: {name}")))?;
                        let fields = body.as_object_mut().expect("split_graph returns an object");
                        let explain = match fields.remove("explain") {
                            None => false,
                            Some(value) => value
                                .as_bool()
                                .ok_or_else(|| invalid("explain must be a boolean"))?,
                        };
                        if explain {
                            fields.insert("operation".into(), json!(command));
                            (Method::POST, "/explain")
                        } else {
                            (Method::POST, read_route(command))
                        }
                    }
                };
                forward(method, &graph_path(&graph, suffix)?, Some(body))
            }
        }
    }
}

fn invalid(message: impl std::fmt::Display) -> String {
    format!("invalid_request: {message}")
}

/// The REST route of a `vlite-graph-cli` read command.
fn read_route(command: &str) -> &'static str {
    match command {
        "get-node" => "/nodes/get",
        "get-edge" => "/edges/get",
        "find-nodes" => "/nodes/find",
        "neighbors" => "/neighbors",
        "traverse" => "/traverse",
        _ => "/shortest-path",
    }
}

/// `/v1/graphs/{graph}{suffix}` with the name percent-encoded. Names made of
/// dots would be normalized away as path segments, and are never valid graph
/// names, so they are rejected here with the server's error.
fn graph_path(graph: &str, suffix: &str) -> Result<String, String> {
    if graph.is_empty() || graph.bytes().all(|byte| byte == b'.') {
        let error = vlite::Error::InvalidGraphName;
        return Err(format!("{}: {error}", graph_json::error_code(&error)));
    }
    let mut path = String::from("/v1/graphs/");
    for byte in graph.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            path.push(char::from(byte));
        } else {
            path.push_str(&format!("%{byte:02X}"));
        }
    }
    path.push_str(suffix);
    Ok(path)
}

/// 2xx bodies are returned as-is; REST errors become `"<code>: <message>"`.
fn decode(response: Response) -> Result<Value, Failure> {
    let status = response.status();
    let body: Option<Value> = response.json().ok();
    if status.is_success() {
        return body.ok_or_else(|| Failure {
            status: Some(status),
            message: "server_error: the server returned a non-JSON response".into(),
        });
    }
    let error = body.as_ref().and_then(|body| body.get("error"));
    let field = |name| {
        error
            .and_then(|error| error.get(name))
            .and_then(Value::as_str)
    };
    let message = match (field("code"), field("message")) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        _ => format!("http_{}: {status}", status.as_u16()),
    };
    Err(Failure {
        status: Some(status),
        message,
    })
}
