#[path = "tool_support/graph_json.rs"]
mod graph_json;
#[path = "tool_support/graph_mcp.rs"]
mod graph_mcp;
#[cfg(feature = "mcp-proxy")]
#[path = "tool_support/graph_proxy.rs"]
mod graph_proxy;

use std::{
    collections::BTreeMap,
    error::Error,
    io::{self, BufRead, Write},
    path::PathBuf,
    process::ExitCode,
};

use serde_json::{Value, json};
use vlite::{Condition, Database, DistanceMetric, EncryptionKey, HnswMaintenancePolicy, Metadata};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const HELP: &str = "vlite-mcp --db PATH [--read-only | --writable] [--encryption-key-file PATH]\n\
       vlite-mcp --server URL [--token-env VAR] [--read-only | --writable]   (mcp-proxy builds)\n\
Read-only is the default. VLITE_ENCRYPTION_KEY or VLITE_ENCRYPTION_KEY_HEX may supply an encryption key.\n\
--server forwards the graph tools to a running vlite-server instead of opening a file; the bearer token is read from the environment variable named by --token-env.\n\
MCP JSON-RPC messages are newline-delimited on stdin/stdout; diagnostics go to stderr.\n";

type ToolResult = Result<Value, String>;

struct Config {
    path: PathBuf,
    read_only: bool,
    encryption_key: Option<EncryptionKey>,
    /// `--server` proxy mode (ITEM-043): no file is opened.
    #[cfg(feature = "mcp-proxy")]
    remote: Option<graph_proxy::Remote>,
}

/// Where tool calls run: an opened file, or a server in proxy mode.
enum Backend {
    Local(Box<Database>),
    #[cfg(feature = "mcp-proxy")]
    Proxy(graph_proxy::Remote),
}

fn decode_hex_key(text: &str) -> Result<[u8; 32], Box<dyn Error>> {
    if text.len() != 64 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(
            "VLITE_ENCRYPTION_KEY_HEX must contain exactly 64 hexadecimal characters".into(),
        );
    }
    let mut key = [0; 32];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)?;
    }
    Ok(key)
}

fn config() -> Result<Option<Config>, Box<dyn Error>> {
    let mut db_path = None;
    let mut read_only = true;
    let mut saw_mode = false;
    let mut key_file = None;
    let mut server: Option<String> = None;
    let mut token_env: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(None);
            }
            "--server" => {
                if server.is_some() {
                    return Err("duplicate --server option".into());
                }
                server = Some(args.next().ok_or("--server requires a URL")?);
            }
            "--token-env" => {
                if token_env.is_some() {
                    return Err("duplicate --token-env option".into());
                }
                token_env = Some(
                    args.next()
                        .ok_or("--token-env requires an environment variable name")?,
                );
            }
            "--db" => {
                if db_path.is_some() {
                    return Err("duplicate --db option".into());
                }
                db_path = Some(PathBuf::from(args.next().ok_or("--db requires a path")?));
            }
            "--read-only" => {
                if saw_mode {
                    return Err("choose only one of --read-only and --writable".into());
                }
                read_only = true;
                saw_mode = true;
            }
            "--writable" => {
                if saw_mode {
                    return Err("choose only one of --read-only and --writable".into());
                }
                read_only = false;
                saw_mode = true;
            }
            "--encryption-key-file" | "--key-file" => {
                if key_file.is_some() {
                    return Err("duplicate encryption key file option".into());
                }
                key_file = Some(PathBuf::from(
                    args.next().ok_or("key file option requires a path")?,
                ));
            }
            _ => return Err(format!("unknown option {arg}; use --help").into()),
        }
    }
    if let Some(url) = server {
        if db_path.is_some() || key_file.is_some() {
            return Err("--server cannot be combined with --db or an encryption key file".into());
        }
        return proxy_config(&url, token_env, read_only).map(Some);
    }
    if token_env.is_some() {
        return Err("--token-env requires --server".into());
    }
    let path = db_path.ok_or("missing --db PATH; use --help")?;
    let env_key = std::env::var_os("VLITE_ENCRYPTION_KEY");
    let env_hex = std::env::var_os("VLITE_ENCRYPTION_KEY_HEX");
    if usize::from(key_file.is_some())
        + usize::from(env_key.is_some())
        + usize::from(env_hex.is_some())
        > 1
    {
        return Err("select only one encryption key source".into());
    }
    let encryption_key = if let Some(path) = key_file {
        let bytes = std::fs::read(path)?;
        if bytes.len() == 32 {
            Some(EncryptionKey::from_bytes(
                bytes.as_slice().try_into().expect("32-byte key"),
            ))
        } else {
            let end = bytes
                .iter()
                .rposition(|byte| !matches!(byte, b'\r' | b'\n'))
                .map_or(0, |n| n + 1);
            Some(EncryptionKey::from_passphrase(&bytes[..end])?)
        }
    } else if let Some(hex) = env_hex {
        let text = hex
            .into_string()
            .map_err(|_| "encryption key hex must be ASCII")?;
        Some(EncryptionKey::from_bytes(decode_hex_key(&text)?))
    } else if let Some(passphrase) = env_key {
        Some(EncryptionKey::from_passphrase(
            passphrase.to_string_lossy().as_bytes(),
        )?)
    } else {
        None
    };
    Ok(Some(Config {
        path,
        read_only,
        encryption_key,
        #[cfg(feature = "mcp-proxy")]
        remote: None,
    }))
}

/// Proxy mode: the token's value is never printed, only the variable name.
#[cfg(feature = "mcp-proxy")]
fn proxy_config(
    url: &str,
    token_env: Option<String>,
    read_only: bool,
) -> Result<Config, Box<dyn Error>> {
    let token = match token_env {
        None => None,
        Some(name) => match std::env::var(&name) {
            Ok(token) if !token.is_empty() => Some(token),
            _ => {
                return Err(format!(
                    "environment variable {name} (--token-env) is unset, empty or not UTF-8"
                )
                .into());
            }
        },
    };
    Ok(Config {
        path: PathBuf::new(),
        read_only,
        encryption_key: None,
        remote: Some(graph_proxy::Remote::new(url, token)?),
    })
}

#[cfg(not(feature = "mcp-proxy"))]
fn proxy_config(_: &str, _: Option<String>, _: bool) -> Result<Config, Box<dyn Error>> {
    Err("--server needs vlite-mcp built with the mcp-proxy feature".into())
}

fn open_database(config: &Config) -> vlite::Result<Database> {
    match (config.read_only, config.encryption_key.as_ref()) {
        (true, None) => Database::open_read_only(&config.path),
        (false, None) => Database::open(&config.path),
        (true, Some(key)) => Database::open_read_only_encrypted(&config.path, key),
        (false, Some(key)) => Database::open_encrypted(&config.path, key),
    }
}

fn metadata_json(metadata: &Metadata) -> Value {
    serde_json::to_value(metadata).unwrap_or(Value::Null)
}

fn collection_name(args: &Value) -> Result<&str, String> {
    match args.get("collection") {
        None => Ok("default"),
        Some(value) => value
            .as_str()
            .ok_or_else(|| "collection must be a string".into()),
    }
}

fn collection_arg<'a>(db: &'a Database, args: &Value) -> Result<vlite::CollectionRef<'a>, String> {
    db.collection(collection_name(args)?)
        .map_err(|error| error.to_string())
}

fn parse_id(value: &Value) -> Result<u64, String> {
    if let Some(text) = value.as_str() {
        if text.is_empty()
            || (text.len() > 1 && text.starts_with('0'))
            || !text.bytes().all(|b| b.is_ascii_digit())
        {
            return Err("id must be a canonical decimal u64".into());
        }
        text.parse().map_err(|_| "id exceeds u64 range".into())
    } else {
        value.as_u64().ok_or_else(|| {
            "id must be a non-negative u64 integer or canonical decimal string".into()
        })
    }
}

fn id_arg(args: &Value) -> Result<u64, String> {
    parse_id(args.get("id").ok_or("missing required argument: id")?)
}

fn parse_vector(value: &Value, name: &str) -> Result<Vec<f32>, String> {
    let vector: Vec<f32> = serde_json::from_value(value.clone())
        .map_err(|_| format!("{name} must be an array of finite numbers"))?;
    if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
        return Err(format!("{name} must contain at least one finite number"));
    }
    Ok(vector)
}

fn parse_metric(value: Option<&Value>) -> Result<DistanceMetric, String> {
    let metric = match value {
        None => "squared_l2",
        Some(value) => value.as_str().ok_or("metric must be a string")?,
    };
    match metric {
        "squared_l2" => Ok(DistanceMetric::SquaredL2),
        "cosine" => Ok(DistanceMetric::Cosine),
        "inner_product" => Ok(DistanceMetric::InnerProduct),
        _ => Err("metric must be squared_l2, cosine, or inner_product".into()),
    }
}

fn bounded_integer(
    args: &Value,
    key: &str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, String> {
    let value = match args.get(key) {
        None => default,
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| format!("{key} must be an integer between {min} and {max}"))?,
    };
    if !(min..=max).contains(&value) {
        return Err(format!("{key} must be between {min} and {max}"));
    }
    Ok(value)
}

fn validate_tool_arguments(name: &str, args: &Value) -> Result<(), String> {
    let allowed: &[&str] = match name {
        "vlite_info" | "vlite_list_collections" => &[],
        "vlite_search" => &["collection", "vector", "k", "filter", "metric", "mode"],
        "vlite_get" => &["collection", "id", "include_vector"],
        "vlite_list" => &["collection", "after", "limit"],
        "vlite_upsert" => &[
            "collection",
            "id",
            "vector",
            "metadata",
            "payload",
            "payload_base64",
        ],
        "vlite_delete" => &["collection", "id"],
        "vlite_index_status" | "vlite_build_index" => &["collection"],
        _ => return Err(format!("unknown tool: {name}")),
    };
    for key in args
        .as_object()
        .ok_or("tool arguments must be a JSON object")?
        .keys()
    {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("unexpected argument: {key}"));
        }
    }
    Ok(())
}

fn parse_conditions(value: Option<&Value>) -> Result<Vec<Condition>, String> {
    match value {
        None => Ok(Vec::new()),
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|error| format!("invalid filter: {error}")),
    }
}

fn parse_metadata(value: Option<&Value>) -> Result<Metadata, String> {
    match value {
        None => Ok(BTreeMap::new()),
        Some(value) => serde_json::from_value::<Metadata>(value.clone())
            .map_err(|error| format!("invalid metadata: {error}")),
    }
}

fn payload_bytes(args: &Value) -> Result<Vec<u8>, String> {
    match (args.get("payload"), args.get("payload_base64")) {
        (Some(_), Some(_)) => Err("provide either payload or payload_base64, not both".into()),
        (Some(value), None) => value
            .as_str()
            .map(|text| text.as_bytes().to_vec())
            .ok_or_else(|| "payload must be UTF-8 text".into()),
        (None, Some(value)) => {
            decode_base64(value.as_str().ok_or("payload_base64 must be a string")?)
        }
        (None, None) => Ok(Vec::new()),
    }
}

fn decode_base64(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(4) {
        return Err("payload_base64 must use padded standard base64".into());
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    for (chunk_index, chunk) in text.as_bytes().as_chunks::<4>().0.iter().enumerate() {
        let last = chunk_index + 1 == text.len() / 4;
        let value = |byte: u8| -> Option<u8> {
            match byte {
                b'A'..=b'Z' => Some(byte - b'A'),
                b'a'..=b'z' => Some(byte - b'a' + 26),
                b'0'..=b'9' => Some(byte - b'0' + 52),
                b'+' => Some(62),
                b'/' => Some(63),
                _ => None,
            }
        };
        let a = value(chunk[0]).ok_or("invalid base64 payload")?;
        let b = value(chunk[1]).ok_or("invalid base64 payload")?;
        let c = if chunk[2] == b'=' {
            None
        } else {
            Some(value(chunk[2]).ok_or("invalid base64 payload")?)
        };
        let d = if chunk[3] == b'=' {
            None
        } else {
            Some(value(chunk[3]).ok_or("invalid base64 payload")?)
        };
        if !last && (c.is_none() || d.is_none()) || c.is_none() && d.is_some() {
            return Err("invalid base64 padding".into());
        }
        out.push((a << 2) | (b >> 4));
        if let Some(c) = c {
            out.push((b << 4) | (c >> 2));
            if let Some(d) = d {
                out.push((c << 6) | d);
            }
        }
    }
    if encode_base64(&out) != text {
        return Err("payload_base64 must be canonical padded base64".into());
    }
    Ok(out)
}

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(a >> 2) as usize] as char);
        out.push(TABLE[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn record_json(id: u64, record: &vlite::Record, include_vector: bool) -> Value {
    let payload = match std::str::from_utf8(&record.payload) {
        Ok(text) => json!({"payload": text}),
        Err(_) => json!({"payload_base64": encode_base64(&record.payload)}),
    };
    let mut value = json!({"id": id.to_string(), "metadata": metadata_json(&record.metadata)});
    if let (Some(object), Some(payload)) = (value.as_object_mut(), payload.as_object()) {
        object.extend(payload.clone());
    }
    if include_vector {
        value["vector"] = json!(record.vector);
    }
    value
}

fn metadata_value_schema() -> Value {
    json!({"oneOf":[
        {"type":"object","properties":{"type":{"const":"string"},"value":{"type":"string"}},"required":["type","value"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"integer"},"value":{"type":"string","pattern":"^-?(0|[1-9][0-9]*)$"}},"required":["type","value"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"float"},"value":{"type":"number"}},"required":["type","value"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"boolean"},"value":{"type":"boolean"}},"required":["type","value"],"additionalProperties":false},
        {"type":"object","properties":{"type":{"const":"geopoint"},"value":{"type":"object","properties":{"latitude":{"type":"number","minimum":-90,"maximum":90},"longitude":{"type":"number","minimum":-180,"maximum":180}},"required":["latitude","longitude"],"additionalProperties":false}},"required":["type","value"],"additionalProperties":false}
    ]})
}

fn filter_schema() -> Value {
    json!({"type":"array","items":{"oneOf":[
        {"type":"object","properties":{"op":{"const":"eq"},"field":{"type":"string"},"value":metadata_value_schema()},"required":["op","field","value"],"additionalProperties":false},
        {"type":"object","properties":{"op":{"const":"range"},"field":{"type":"string"},"min":{"anyOf":[metadata_value_schema(),{"type":"null"}]},"max":{"anyOf":[metadata_value_schema(),{"type":"null"}]}},"required":["op","field"],"additionalProperties":false},
        {"type":"object","properties":{"op":{"const":"geo_radius"},"field":{"type":"string"},"latitude":{"type":"number","minimum":-90,"maximum":90},"longitude":{"type":"number","minimum":-180,"maximum":180},"radius_meters":{"type":"number","exclusiveMinimum":0}},"required":["op","field","latitude","longitude","radius_meters"],"additionalProperties":false}
    ]}})
}

fn tool_definitions(read_only: bool) -> Vec<Value> {
    let id_schema = json!({"oneOf":[{"type":"string","pattern":"^(0|[1-9][0-9]*)$"},{"type":"integer","minimum":0}]});
    let collection = json!({"type":"string","description":"Collection name; defaults to default."});
    let vector_schema =
        json!({"type":"array","items":{"type":"number"},"minItems":1,"maxItems":65536});
    let metadata_value_schema = metadata_value_schema();
    let filter_schema = filter_schema();
    let mut tools = vec![
        json!({"name":"vlite_info","description":"Describe database format, dimensions, record counts, collections and access mode.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true}}),
        json!({"name":"vlite_list_collections","description":"List VLite collections and their dimensions and live record counts.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true}}),
        json!({"name":"vlite_search","description":"Search vectors using exact or approximate search. Filters use VLite typed metadata conditions.","inputSchema":{"type":"object","properties":{"collection":collection,"vector":vector_schema,"k":{"type":"integer","minimum":1,"maximum":100,"default":10},"filter":filter_schema,"metric":{"type":"string","enum":["squared_l2","cosine","inner_product"],"default":"squared_l2"},"mode":{"type":"string","enum":["exact","approximate"],"default":"exact"}},"required":["vector"],"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true}}),
        json!({"name":"vlite_get","description":"Fetch one record by unsigned 64-bit ID; vectors are omitted unless requested.","inputSchema":{"type":"object","properties":{"collection":collection,"id":id_schema,"include_vector":{"type":"boolean","default":false}},"required":["id"],"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true}}),
        json!({"name":"vlite_list","description":"List records in ascending ID order using cursor pagination; vectors are omitted.","inputSchema":{"type":"object","properties":{"collection":collection,"after":id_schema,"limit":{"type":"integer","minimum":1,"maximum":100,"default":20}},"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true}}),
        json!({"name":"vlite_index_status","description":"Report the collection's in-memory HNSW index readiness and maintenance pressure.","inputSchema":{"type":"object","properties":{"collection":collection},"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true}}),
        json!({"name":"vlite_build_index","description":"Build an in-memory HNSW index for a collection; it does not modify the database file.","inputSchema":{"type":"object","properties":{"collection":collection},"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true}}),
    ];
    if !read_only {
        tools.push(json!({"name":"vlite_upsert","description":"Insert or replace a record. Payload is optional UTF-8 text or canonical base64 bytes.","inputSchema":{"type":"object","properties":{"collection":collection,"id":id_schema,"vector":vector_schema,"metadata":{"type":"object","additionalProperties":metadata_value_schema},"payload":{"type":"string"},"payload_base64":{"type":"string"}},"required":["id","vector"],"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true}}));
        tools.push(json!({"name":"vlite_delete","description":"Delete a record by unsigned 64-bit ID.","inputSchema":{"type":"object","properties":{"collection":collection,"id":id_schema},"required":["id"],"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true}}));
    }
    tools
}

fn tool_call(
    db: &mut Database,
    path: &std::path::Path,
    read_only: bool,
    name: &str,
    args: &Value,
) -> ToolResult {
    if let Some(result) = graph_mcp::call(db, read_only, name, args) {
        return result;
    }
    validate_tool_arguments(name, args)?;
    match name {
        "vlite_info" => {
            let collections = db.collections().map_err(|e| e.to_string())?;
            let items: Vec<Value> = collections
                .iter()
                .map(|c| json!({"name":c.name,"dimensions":c.dimensions,"count":c.live_records}))
                .collect();
            let total: usize = collections.iter().map(|c| c.live_records).sum();
            let dimensions = db
                .collection("default")
                .map_err(|e| e.to_string())?
                .dimensions();
            let bytes = std::fs::metadata(path).map(|m| m.len()).ok();
            #[allow(unused_mut)]
            let mut info = json!({"format_version":db.format_version(),"dimensions":dimensions,"count":total,"collections":items,"read_only":read_only,"file_bytes":bytes});
            graph_mcp::extend_info(db, &mut info)?;
            Ok(info)
        }
        "vlite_list_collections" => {
            let items: Vec<Value> = db.collections().map_err(|e| e.to_string())?.iter().map(|c| json!({"name":c.name,"dimensions":c.dimensions,"count":c.live_records,"id":c.id})).collect();
            Ok(json!({"collections":items}))
        }
        "vlite_search" => {
            let vector = parse_vector(
                args.get("vector")
                    .ok_or("missing required argument: vector")?,
                "vector",
            )?;
            let k = bounded_integer(args, "k", 10, 1, 100)?;
            let metric = parse_metric(args.get("metric"))?;
            let conditions = parse_conditions(args.get("filter"))?;
            let mode = match args.get("mode") {
                None => "exact",
                Some(value) => value.as_str().ok_or("mode must be a string")?,
            };
            if mode != "exact" && mode != "approximate" {
                return Err("mode must be exact or approximate".into());
            }
            let name = collection_name(args)?;
            let expected = db.collection(name).map_err(|e| e.to_string())?.dimensions();
            if vector.len() != expected {
                return Err(format!(
                    "vector dimension mismatch: expected {expected}, got {}",
                    vector.len()
                ));
            }
            let hits = if mode == "exact" {
                db.collection(name)
                    .map_err(|e| e.to_string())?
                    .search_filtered_with_metric(&vector, k, &conditions, metric)
            } else {
                db.collection_mut(name)
                    .map_err(|e| e.to_string())?
                    .search_approximate_filtered_with_metric(
                        &vector,
                        k,
                        100,
                        vlite::HnswConfig::default(),
                        &conditions,
                        metric,
                    )
            }
            .map_err(|e| e.to_string())?;
            Ok(
                json!({"hits":hits.iter().map(|hit| json!({"id":hit.id.to_string(),"distance":hit.distance})).collect::<Vec<_>>()}),
            )
        }
        "vlite_get" => {
            let id = id_arg(args)?;
            let include_vector = match args.get("include_vector") {
                None => false,
                Some(value) => value.as_bool().ok_or("include_vector must be a boolean")?,
            };
            let collection = collection_arg(db, args)?;
            Ok(
                json!({"record":collection.get(id).map_err(|e| e.to_string())?.map(|r| record_json(id, r, include_vector))}),
            )
        }
        "vlite_list" => {
            let after = args.get("after").map(parse_id).transpose()?;
            let limit = bounded_integer(args, "limit", 20, 1, 100)?;
            let page = collection_arg(db, args)?
                .browse(after, limit)
                .map_err(|e| e.to_string())?;
            Ok(
                json!({"records":page.hits.iter().map(|hit| record_json(hit.id, &hit.record, false)).collect::<Vec<_>>(),"next_after":page.next_after.map(|id|id.to_string())}),
            )
        }
        "vlite_upsert" => {
            if read_only {
                return Err("database is read-only; vlite_upsert is unavailable".into());
            }
            let id = id_arg(args)?;
            let vector = parse_vector(
                args.get("vector")
                    .ok_or("missing required argument: vector")?,
                "vector",
            )?;
            let metadata = parse_metadata(args.get("metadata"))?;
            let payload = payload_bytes(args)?;
            let name = collection_name(args)?;
            let expected = db.collection(name).map_err(|e| e.to_string())?.dimensions();
            if vector.len() != expected {
                return Err(format!(
                    "vector dimension mismatch: expected {expected}, got {}",
                    vector.len()
                ));
            }
            db.collection_mut(name)
                .map_err(|e| e.to_string())?
                .put_with_metadata(id, &vector, &payload, &metadata)
                .map_err(|e| e.to_string())?;
            Ok(json!({"upserted":true,"id":id.to_string()}))
        }
        "vlite_delete" => {
            if read_only {
                return Err("database is read-only; vlite_delete is unavailable".into());
            }
            let id = id_arg(args)?;
            let deleted = collection_arg_mut(db, args)?
                .delete(id)
                .map_err(|e| e.to_string())?;
            Ok(json!({"deleted":deleted,"id":id.to_string()}))
        }
        "vlite_index_status" => {
            let name = collection_name(args)?;
            let settings = db
                .collection(name)
                .map_err(|e| e.to_string())?
                .settings()
                .clone();
            let status = db
                .collection_mut(name)
                .map_err(|e| e.to_string())?
                .hnsw_maintenance_status(
                    settings.hnsw,
                    settings.metric,
                    HnswMaintenancePolicy::default(),
                )
                .map_err(|e| e.to_string())?;
            Ok(
                json!({"ready":status.ready,"pressure":format!("{:?}", status.pressure),"active_buffer":status.active_buffer.map(|buffer|json!({"buffered_ids":buffer.buffered_ids,"remaining_ids":buffer.remaining_ids,"buffer_vector_bytes":buffer.buffer_vector_bytes,"remaining_vector_bytes":buffer.remaining_vector_bytes})),"rebuild_active":status.rebuild.is_some()}),
            )
        }
        "vlite_build_index" => {
            let name = collection_name(args)?;
            let settings = db
                .collection(name)
                .map_err(|e| e.to_string())?
                .settings()
                .clone();
            db.collection_mut(name)
                .map_err(|e| e.to_string())?
                .build_hnsw_with_metric(settings.hnsw, settings.metric)
                .map_err(|e| e.to_string())?;
            let ready = db
                .collection_mut(name)
                .map_err(|e| e.to_string())?
                .hnsw_maintenance_status(
                    settings.hnsw,
                    settings.metric,
                    HnswMaintenancePolicy::default(),
                )
                .map_err(|e| e.to_string())?
                .ready;
            Ok(json!({"built":ready,"persistent":false,"read_only":read_only}))
        }
        _ => Err(format!("unknown tool: {name}")),
    }
}

fn collection_arg_mut<'a>(
    db: &'a mut Database,
    args: &Value,
) -> Result<vlite::CollectionMut<'a>, String> {
    db.collection_mut(collection_name(args)?)
        .map_err(|error| error.to_string())
}

fn protocol_error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

fn tool_result(result: ToolResult) -> Value {
    match result {
        Ok(value) => {
            json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":false})
        }
        Err(message) => {
            json!({"content":[{"type":"text","text":message}],"structuredContent":{"error":message},"isError":true})
        }
    }
}

fn dispatch(backend: &mut Backend, config: &Config, message: Value) -> Option<Value> {
    let Some(object) = message.as_object() else {
        return Some(protocol_error(Value::Null, -32600, "Invalid Request"));
    };
    let id = object.get("id");
    let has_id = id.is_some();
    let response_id = id.cloned().unwrap_or(Value::Null);
    let valid_id =
        id.is_some_and(|id| id.is_string() || id.as_i64().is_some() || id.as_u64().is_some());
    let method = object.get("method").and_then(Value::as_str);
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || method.is_none()
        || (has_id && !valid_id)
    {
        return Some(protocol_error(response_id, -32600, "Invalid Request"));
    }
    let method = method.unwrap_or_default();
    let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
    if !params.is_object() {
        return if has_id {
            Some(protocol_error(response_id, -32602, "Invalid params"))
        } else {
            None
        };
    }
    let result = match method {
        "initialize" => {
            let version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROTOCOL_VERSION);
            let negotiated = if SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
                version
            } else {
                DEFAULT_PROTOCOL_VERSION
            };
            let instructions = match backend {
                Backend::Local(_) => {
                    "VLite vector database tools. Read-only is the default; write tools are exposed only with --writable."
                }
                #[cfg(feature = "mcp-proxy")]
                Backend::Proxy(_) => {
                    "VLite property-graph tools forwarded to a vlite-server; the server authorizes every call. Read-only is the default; write tools are exposed only with --writable."
                }
            };
            Some(
                json!({"protocolVersion":negotiated,"capabilities":{"tools":{}},"serverInfo":{"name":"vlite-mcp","version":VERSION},"instructions":instructions}),
            )
        }
        "notifications/initialized" | "notifications/cancelled" | "notifications/progress" => None,
        "ping" => Some(json!({})),
        "tools/list" => match backend {
            Backend::Local(db) => {
                #[allow(unused_mut)]
                let mut tools = tool_definitions(config.read_only);
                tools.extend(graph_mcp::tool_definitions(db, config.read_only));
                Some(json!({"tools":tools}))
            }
            #[cfg(feature = "mcp-proxy")]
            Backend::Proxy(remote) => match remote.tool_definitions(config.read_only) {
                Ok(tools) => Some(json!({"tools":tools})),
                Err(message) => {
                    return has_id.then(|| protocol_error(response_id, -32603, &message));
                }
            },
        },
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str);
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match (name, args.as_object()) {
                (Some(name), Some(_)) => Some(tool_result(match backend {
                    Backend::Local(db) => {
                        tool_call(db, &config.path, config.read_only, name, &args)
                    }
                    #[cfg(feature = "mcp-proxy")]
                    Backend::Proxy(remote) => remote
                        .call(config.read_only, name, &args)
                        .unwrap_or_else(|| Err(format!("unknown tool: {name}"))),
                })),
                _ => {
                    return Some(protocol_error(
                        response_id,
                        -32602,
                        "tools/call requires a tool name and object arguments",
                    ));
                }
            }
        }
        _ => {
            return if has_id {
                Some(protocol_error(response_id, -32601, "Method not found"))
            } else {
                None
            };
        }
    };
    if !has_id {
        return None;
    }
    Some(json!({"jsonrpc":"2.0","id":response_id,"result":result.unwrap_or(Value::Null)}))
}

fn open_backend(config: &mut Config) -> Result<Backend, Box<dyn Error>> {
    #[cfg(feature = "mcp-proxy")]
    if let Some(remote) = config.remote.take() {
        return Ok(Backend::Proxy(remote));
    }
    Ok(Backend::Local(Box::new(open_database(config)?)))
}

fn run() -> Result<(), Box<dyn Error>> {
    let Some(mut config) = config()? else {
        return Ok(());
    };
    let mut backend = open_backend(&mut config)?;
    let stdin = io::stdin();
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(message) => dispatch(&mut backend, &config, message),
            Err(_) => Some(protocol_error(Value::Null, -32700, "Parse error")),
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut stdout, &response)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
    }
    match backend {
        Backend::Local(db) => db.close()?,
        #[cfg(feature = "mcp-proxy")]
        Backend::Proxy(_) => {}
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("vlite-mcp: {error}");
            ExitCode::FAILURE
        }
    }
}
