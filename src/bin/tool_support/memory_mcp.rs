//! Authenticated, server-only memory tools. No database or filesystem access.

use std::{io::Read, sync::OnceLock, time::Duration};

use reqwest::{blocking::Client, redirect::Policy};
use serde_json::{Map, Value, json};

const MAX_BYTES: usize = 8_388_608;
const TIMEOUT: Duration = Duration::from_secs(5);

struct Tool {
    name: &'static str,
    suffix: &'static str,
    read_only: bool,
    required: &'static [&'static str],
    description: &'static str,
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "vlite_memory_remember",
        suffix: "remember",
        read_only: false,
        required: &["binding", "scope", "idempotency_key", "items"],
        description: "Remember caller-produced scoped memories on the server.",
    },
    Tool {
        name: "vlite_memory_recall",
        suffix: "recall",
        read_only: true,
        required: &["binding", "scope"],
        description: "Recall scoped memories with bounded-plan diagnostics.",
    },
    Tool {
        name: "vlite_memory_facts",
        suffix: "facts/query",
        read_only: true,
        required: &["binding", "scope", "slot"],
        description: "Query retained facts in one exact scoped slot.",
    },
    Tool {
        name: "vlite_memory_correct_facts",
        suffix: "facts/correct",
        read_only: false,
        required: &[
            "binding",
            "scope",
            "idempotency_key",
            "slot",
            "series",
            "slot_revision",
            "interval",
            "action",
        ],
        description: "Correct facts with explicit revision guards.",
    },
    Tool {
        name: "vlite_memory_consolidate",
        suffix: "consolidate",
        read_only: false,
        required: &["binding", "scope", "idempotency_key", "inputs", "output"],
        description: "Plan or commit caller-produced consolidation; requires writable mode.",
    },
    Tool {
        name: "vlite_memory_forget",
        suffix: "forget",
        read_only: false,
        required: &["binding", "scope", "idempotency_key", "target", "mode"],
        description: "Logically delete or expire scoped memory; not secure erasure.",
    },
    Tool {
        name: "vlite_memory_receipt",
        suffix: "receipts/lookup",
        read_only: true,
        required: &["binding", "scope", "operation", "idempotency_key"],
        description: "Look up a retained receipt to reconcile a mutation.",
    },
    Tool {
        name: "vlite_memory_erasure",
        suffix: "erasures/get",
        read_only: true,
        required: &["binding", "scope", "job_id"],
        description: "Inspect erasure status without advancing the job.",
    },
    Tool {
        name: "vlite_memory_resume_erasure",
        suffix: "erasures/resume",
        read_only: false,
        required: &[
            "binding",
            "scope",
            "idempotency_key",
            "job_id",
            "revision",
            "generation",
            "resume_token",
        ],
        description: "Explicitly resume one bounded erasure chunk.",
    },
];

pub(crate) fn is_tool(name: &str) -> bool {
    TOOLS.iter().any(|tool| tool.name == name)
}

fn reference(name: &str) -> Value {
    json!({"$ref":format!("#/$defs/{name}")})
}

fn object(required: &[&str], properties: Value) -> Value {
    json!({"type":"object","additionalProperties":false,"required":required,"properties":properties})
}

fn optional(properties: Value) -> Value {
    json!({"type":"object","additionalProperties":false,"properties":properties})
}

fn array(items: Value, minimum: Option<usize>, maximum: usize) -> Value {
    let mut schema = json!({"type":"array","items":items,"maxItems":maximum});
    if let Some(minimum) = minimum {
        schema["minItems"] = json!(minimum);
    }
    schema
}

fn enumeration(values: &[&str]) -> Value {
    json!({"type":"string","enum":values})
}

fn default_enum(values: &[&str], default: &str) -> Value {
    let mut schema = enumeration(values);
    schema["default"] = json!(default);
    schema
}

fn budget(minimum: u64, maximum: u64, default: u64) -> Value {
    json!({"type":"integer","minimum":minimum,"maximum":maximum,"default":default})
}

fn boolean(default: bool) -> Value {
    json!({"type":"boolean","default":default})
}

fn definitions() -> &'static Value {
    static DEFINITIONS: OnceLock<Value> = OnceLock::new();
    DEFINITIONS.get_or_init(|| {
        let r = reference;
        let mut definitions = Map::new();
        macro_rules! define {
            ($name:literal, $schema:expr) => {
                definitions.insert($name.into(), $schema);
            };
        }
        define!("MemoryBinding", json!({"type":"string","pattern":"^[a-z][a-z0-9_-]{0,62}$"}));
        define!("MemoryKey", json!({"type":"string","minLength":1,"maxLength":256}));
        define!("MemoryU64", json!({"oneOf":[
            {"type":"string","pattern":"^(0|[1-9][0-9]*)$"},
            {"type":"integer","minimum":0,"maximum":u64::MAX}
        ]}));
        define!("MemoryI64", json!({"oneOf":[
            {"type":"string","pattern":"^(0|-?[1-9][0-9]*)$"},
            {"type":"integer","minimum":i64::MIN,"maximum":i64::MAX}
        ]}));
        define!("MemoryKind", enumeration(&["entity","episodic","fact","relation"]));
        define!("MemoryOperation", enumeration(&["remember","correct_facts","consolidate","forget","resume_erasure"]));
        define!("MemoryScope", object(&["agent"], json!({
            "agent":r("MemoryKey"),"user":r("MemoryKey"),"session":r("MemoryKey")
        })));
        define!("MemoryExpected", json!({"oneOf":[
            object(&["absent"], json!({"absent":{"type":"boolean","enum":[true]}})),
            object(&["revision"], json!({"revision":r("MemoryU64")}))
        ]}));
        let nullable_time = json!({"anyOf":[r("MemoryI64"),{"type":"null"}]});
        define!("MemoryInterval", object(&["valid_from_ms","valid_to_ms"], json!({
            "valid_from_ms":nullable_time,"valid_to_ms":nullable_time
        })));
        define!("MemoryRef", object(&["id","revision"], json!({"id":r("MemoryKey"),"revision":r("MemoryU64")})));
        define!("MemorySource", object(&["source_id","source_kind"], json!({
            "source_id":r("MemoryKey"),"source_kind":r("MemoryKey"),
            "uri":{"type":"string","maxLength":256},"transformation_id":r("MemoryKey")
        })));
        let scalar = |kind: &str, value: Value| object(&["type","value"], json!({
            "type":enumeration(&[kind]),"value":value
        }));
        define!("MemoryScalar", json!({"oneOf":[
            scalar("string",json!({"type":"string","maxLength":4096})),
            scalar("integer",r("MemoryI64")),scalar("float",json!({"type":"number"})),
            scalar("boolean",json!({"type":"boolean"}))
        ]}));
        define!("MemoryProperties", json!({"type":"object","maxProperties":64,"additionalProperties":r("MemoryScalar")}));
        define!("MemoryCondition", object(&["field","op"], json!({
            "field":r("MemoryKey"),"op":enumeration(&["eq","lt","lte","gt","gte","exists"]),"value":r("MemoryScalar")
        })));
        define!("MemoryFilter", optional(json!({
            "all":array(r("MemoryCondition"),None,64),"any":array(r("MemoryCondition"),None,64)
        })));
        let evidence = json!({"collection":r("MemoryBinding"),"record":r("MemoryU64")});
        define!("MemoryEvidenceRef", object(&["collection","record"], evidence.clone()));
        let mut evidence = evidence;
        evidence["vector"] = json!({"type":"array","items":{"type":"number"},"minItems":1});
        evidence["text"] = json!({"type":"string","maxLength":4096});
        evidence["payload_base64"] = json!({"type":"string","format":"byte"});
        evidence["metadata"] = r("MemoryProperties");
        define!("MemoryEvidence", object(&["collection","record"], evidence));
        define!("MemoryAssertion", object(
            &["subject_id","predicate","slot_key","destination_id","interval","slot_expected"],
            json!({
                "subject_id":r("MemoryKey"),"predicate":r("MemoryKey"),"slot_key":r("MemoryKey"),
                "destination_id":r("MemoryKey"),"interval":r("MemoryInterval"),
                "slot_expected":r("MemoryExpected"),"disagreement":default_enum(&["reject","disputed"],"reject")
            })
        ));
        define!("MemoryItem", object(&["id","kind","content","observed_at_ms","source"], json!({
            "id":r("MemoryKey"),"kind":r("MemoryKind"),"content":{"type":"string","maxLength":4096},
            "observed_at_ms":r("MemoryI64"),"importance":{"type":"number","minimum":0,"maximum":1,"default":0.5},
            "confidence":{"type":"number","minimum":0,"maximum":1},"expires_at_ms":r("MemoryU64"),
            "source":r("MemorySource"),"provenance":array(r("MemorySource"),None,64),
            "references":array(r("MemoryRef"),None,256),"evidence":array(r("MemoryEvidenceRef"),None,64),
            "metadata":r("MemoryProperties"),"assertion":r("MemoryAssertion"),"expected":r("MemoryExpected"),
            "merge":default_enum(&["none","merge_provenance"],"none")
        })));
        define!("MemoryRanking", optional(json!({
            "selection":default_enum(&["weighted","exact","mmr"],"weighted"),
            "weights":{"type":"array","items":{"type":"number","minimum":0},"minItems":3,"maxItems":3,"default":[1,0,0]},
            "half_life_ms":{"type":"number","exclusiveMinimum":0},
            "future_observations":default_enum(&["reject","clamp"],"reject"),
            "lambda":{"type":"number","minimum":0,"maximum":1,"default":0.5}
        })));
        define!("MemoryLimits", optional(json!({
            "candidates":budget(1,1000,200),"vector_components":budget(1,4000000,1000000),
            "evidence_bytes":budget(1,4194304,1048576),"mmr_components":budget(1,100000000,10000000),
            "deadline_ms":budget(1,5000,250),"max_hops":budget(0,4,2),
            "max_visited_nodes":budget(1,10000,1000),"max_edges_examined":budget(1,100000,10000),
            "max_edges_per_node":budget(1,10000,1000),"max_output_nodes":budget(0,1000,100),
            "max_output_edges":budget(0,2000,200),"max_output_paths":budget(0,100,20)
        })));
        let mut metadata = array(r("MemoryKey"),None,64);
        metadata["default"] = json!([]);
        define!("MemoryProjection", optional(json!({
            "snippets":boolean(true),"provenance":boolean(true),"paths":boolean(false),"metadata":metadata
        })));
        define!("MemorySlot", object(&["subject_id","predicate","slot_key"],json!({
            "subject_id":r("MemoryKey"),"predicate":r("MemoryKey"),"slot_key":r("MemoryKey")
        })));
        define!("MemoryInput", object(&["id","revision"],json!({
            "id":r("MemoryKey"),"revision":r("MemoryU64"),"disposition":default_enum(&["retain","logical_delete"],"retain")
        })));
        define!("MemoryTarget", json!({"oneOf":[
            object(&["ids"],json!({"ids":array(r("MemoryRef"),Some(1),4096)})),
            object(&["series"],json!({"series":r("MemoryRef")})),
            object(&["entity"],json!({"entity":r("MemoryRef")})),
            object(&["session"],json!({"session":{"type":"boolean","enum":[true]}})),
            object(&["scope"],json!({"scope":{"type":"boolean","enum":[true]}}))
        ]}));
        Value::Object(definitions)
    })
}

fn schema(tool: &Tool) -> Value {
    let r = reference;
    let mut properties = match tool.name {
        "vlite_memory_remember" => json!({
            "items":array(r("MemoryItem"),Some(1),4096),"evidence":array(r("MemoryEvidence"),None,256)
        }),
        "vlite_memory_recall" => json!({
            "scope":r("MemoryScope"),"scopes":array(r("MemoryScope"),Some(1),16),
            "vector":{"type":"array","items":{"type":"number"},"minItems":1},
            "text":{"type":"string","minLength":1,"maxLength":16384},"embedding_provider":r("MemoryKey"),
            "entity_seeds":array(r("MemoryKey"),None,64),"kinds":array(r("MemoryKind"),None,4),
            "filter":r("MemoryFilter"),"as_of_ms":r("MemoryI64"),"ranking":r("MemoryRanking"),
            "limits":r("MemoryLimits"),"projection":r("MemoryProjection"),
            "mode":default_enum(&["exact","approximate"],"exact"),
            "limit":budget(1,100,20),"allow_partial":boolean(false)
        }),
        "vlite_memory_facts" => json!({
            "scope":r("MemoryScope"),"slot":r("MemorySlot"),"as_of_ms":r("MemoryI64"),
            "history":boolean(false),"limit":budget(1,100,20),"limits":r("MemoryLimits"),
            "projection":r("MemoryProjection"),"allow_partial":boolean(false)
        }),
        "vlite_memory_correct_facts" => json!({
            "slot":r("MemorySlot"),"series":r("MemoryRef"),"slot_revision":r("MemoryU64"),
            "interval":r("MemoryInterval"),"action":enumeration(&["close","replace","remove"]),
            "at_ms":r("MemoryI64"),"correction_interval":r("MemoryInterval"),
            "replacement":r("MemoryItem"),"evidence":array(r("MemoryEvidence"),None,256)
        }),
        "vlite_memory_consolidate" => json!({
            "inputs":array(r("MemoryInput"),Some(1),256),"output":r("MemoryItem"),
            "evidence":array(r("MemoryEvidence"),None,256),"dry_run":boolean(false)
        }),
        "vlite_memory_forget" => json!({
            "target":r("MemoryTarget"),"mode":enumeration(&["logical_delete","expire"]),
            "expires_at_ms":r("MemoryU64"),"allow_job":boolean(false)
        }),
        "vlite_memory_receipt" => json!({
            "scope":r("MemoryScope"),"operation":r("MemoryOperation"),"idempotency_key":r("MemoryKey")
        }),
        "vlite_memory_erasure" => json!({"scope":r("MemoryScope"),"job_id":r("MemoryKey")}),
        "vlite_memory_resume_erasure" => json!({
            "job_id":r("MemoryKey"),"revision":r("MemoryU64"),"generation":r("MemoryU64"),
            "resume_token":{"type":"string","minLength":1,"maxLength":4096}
        }),
        _ => unreachable!("tool inventory is closed"),
    };
    if !tool.read_only {
        for (name, value) in [
            ("scope", r("MemoryScope")),
            ("idempotency_key", r("MemoryKey")),
            ("max_references", budget(1, 256, 64)),
            ("deadline_ms", budget(1, 5000, 250)),
            ("require_complete_feed", boolean(false)),
        ] {
            properties[name] = value;
        }
    }
    properties["binding"] = r("MemoryBinding");
    let mut schema = object(tool.required, properties);
    schema["$defs"] = definitions().clone();
    schema
}

pub(crate) fn tool_definitions(read_only: bool) -> Vec<Value> {
    TOOLS
        .iter()
        .filter(|tool| !read_only || tool.read_only)
        .map(|tool| {
            let mut annotations = json!({"readOnlyHint":tool.read_only});
            if !tool.read_only {
                annotations["idempotentHint"] = json!(false);
            }
            json!({
                "name":tool.name,"description":tool.description,"inputSchema":schema(tool),
                "annotations":annotations
            })
        })
        .collect()
}

// Only structural checks run here. Bounds, canonical strings and cross-field
// semantics belong to the authenticated server; no request defaults are applied.
fn validate(value: &Value, schema: &Value, path: &str) -> Result<(), String> {
    if let Some(name) = schema.get("$ref").and_then(Value::as_str) {
        let name = name
            .strip_prefix("#/$defs/")
            .expect("local memory definition");
        return validate(value, &definitions()[name], path);
    }
    if let Some(choices) = schema.get("oneOf").or_else(|| schema.get("anyOf")) {
        for choice in choices.as_array().expect("schema alternatives") {
            if let Some(kinds) = choice
                .pointer("/properties/type/enum")
                .and_then(Value::as_array)
                && !kinds.contains(&value["type"])
            {
                continue;
            }
            if validate(value, choice, path).is_ok() {
                return Ok(());
            }
        }
        return Err(format!(
            "invalid_request: {path} does not match an allowed shape"
        ));
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let fields = value
                .as_object()
                .ok_or_else(|| format!("invalid_request: {path} must be an object"))?;
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for name in required {
                    let name = name.as_str().expect("required field name");
                    if !fields.contains_key(name) {
                        return Err(format!("invalid_request: {path}.{name} is required"));
                    }
                }
            }
            for (name, value) in fields {
                let property = schema.get("properties").and_then(|p| p.get(name));
                let additional = schema.get("additionalProperties");
                match property.or_else(|| additional.filter(|s| s.is_object())) {
                    Some(property) => validate(value, property, &format!("{path}.{name}"))?,
                    None => return Err(format!("invalid_request: {path}.{name} is unknown")),
                }
            }
        }
        Some("array") => {
            let values = value
                .as_array()
                .ok_or_else(|| format!("invalid_request: {path} must be an array"))?;
            for (index, value) in values.iter().enumerate() {
                validate(value, &schema["items"], &format!("{path}[{index}]"))?;
            }
        }
        Some(kind) => {
            let valid = match kind {
                "string" => value.is_string(),
                "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
                "number" => value.is_number(),
                "boolean" => value.is_boolean(),
                "null" => value.is_null(),
                _ => unreachable!("known schema type"),
            };
            if !valid {
                return Err(format!("invalid_request: {path} must be a {kind}"));
            }
        }
        None => unreachable!("all memory schemas declare a shape"),
    }
    Ok(())
}

/// Separate from the graph transport so memory discovery never needs raw access.
/// The same startup URL and bearer-token value are used by both adapters.
pub(crate) struct Remote {
    base: String,
    token: Option<String>,
    client: Client,
}

impl Remote {
    pub(crate) fn new(url: &str, token: Option<String>) -> Result<Self, String> {
        let client = Client::builder()
            .no_proxy()
            .timeout(TIMEOUT)
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| "cannot initialize the memory HTTP client")?;
        Ok(Self {
            base: url.trim_end_matches('/').into(),
            token,
            client,
        })
    }

    pub(crate) fn call(
        &self,
        read_only: bool,
        name: &str,
        args: &Value,
        message_text: &str,
    ) -> Option<Value> {
        TOOLS.iter().find(|tool| tool.name == name).map(|tool| {
            let result = super::kv_mcp::unique_json(message_text)
                .and_then(|()| self.run(read_only, tool, args));
            safe_result(result, self.token.as_deref(), tool.read_only)
        })
    }

    fn run(&self, read_only: bool, tool: &Tool, args: &Value) -> Result<Value, String> {
        let url = reqwest::Url::parse(&self.base)
            .map_err(|_| "insecure_url: invalid memory server URL")?;
        let loopback = url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        });
        if url.scheme() == "http" && !loopback {
            return Err(
                "insecure_url: HTTP is permitted only for loopback hosts; use HTTPS".into(),
            );
        }
        if let Some(token) = &self.token
            && (token.is_empty()
                || token.len() > 4096
                || !token.bytes().all(|byte| (0x21..=0x7e).contains(&byte)))
        {
            return Err("invalid_token: token must contain 1..4096 visible ASCII bytes".into());
        }
        if read_only && !tool.read_only {
            return Err(format!(
                "read_only: database is read-only; {} is unavailable",
                tool.name
            ));
        }
        validate(args, &schema(tool), "arguments")?;
        if args.to_string().len() > MAX_BYTES {
            return Err("invalid_json: memory request exceeds 8388608 bytes".into());
        }
        let mut body = args.as_object().expect("validated object").clone();
        let binding = body.remove("binding").expect("required binding");
        let binding = binding.as_str().expect("validated binding");
        if binding.is_empty()
            || binding.len() > 63
            || !binding.as_bytes()[0].is_ascii_lowercase()
            || !binding.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte)
            })
        {
            return Err("invalid_request: binding must match [a-z][a-z0-9_-]{0,62}".into());
        }
        let mut request = self
            .client
            .post(format!("{}/v1/memory/{binding}/{}", self.base, tool.suffix))
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&body);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .map_err(|_| transport_error(tool.read_only))?;
        let status = response.status();
        let mut bytes = Vec::new();
        response
            .take((MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| {
                response_error(
                    tool.read_only,
                    "invalid_response: cannot read memory response",
                )
            })?;
        if bytes.len() > MAX_BYTES {
            return Err(response_error(
                tool.read_only,
                "response_too_large: memory response exceeds 8388608 bytes",
            ));
        }
        let invalid = || {
            response_error(
                tool.read_only,
                "invalid_response: expected a JSON object without duplicate keys",
            )
        };
        let text = std::str::from_utf8(&bytes).map_err(|_| invalid())?;
        super::kv_mcp::unique_json(text).map_err(|_| invalid())?;
        let value: Value = serde_json::from_str(text).map_err(|_| invalid())?;
        if !value.is_object() {
            return Err(invalid());
        }
        if self
            .token
            .as_deref()
            .is_some_and(|token| credential_in_value(&value, token))
        {
            return Err(credential_error(tool.read_only));
        }
        if !status.is_success() || value.get("error").is_some() {
            let code = value["error"]["code"].as_str();
            let message = value["error"]["message"].as_str();
            return Err(match (code, message) {
                (Some(code), Some(message)) => {
                    bounded_error(format!("{code}: {message}"), tool.read_only)
                }
                _ => response_error(
                    tool.read_only,
                    "invalid_response: invalid server error envelope",
                ),
            });
        }
        let result = json!({
            "content":[{"type":"text","text":value.to_string()}],
            "structuredContent":value,"isError":false
        });
        if result.to_string().len() > MAX_BYTES {
            return Err(response_error(
                tool.read_only,
                "response_too_large: memory tool result exceeds 8388608 bytes",
            ));
        }
        Ok(value)
    }
}

fn credential_in_value(value: &Value, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    match value {
        Value::String(text) => text.contains(token),
        Value::Array(values) => values.iter().any(|value| credential_in_value(value, token)),
        Value::Object(fields) => fields
            .iter()
            .any(|(key, value)| key.contains(token) || credential_in_value(value, token)),
        _ => false,
    }
}

fn credential_error(read_only: bool) -> String {
    response_error(
        read_only,
        "credential_in_response: response contained the bearer credential and was not returned",
    )
}

fn result_value(result: Result<Value, String>) -> Value {
    match result {
        Ok(value) => json!({
            "content":[{"type":"text","text":value.to_string()}],
            "structuredContent":value,"isError":false
        }),
        Err(message) => json!({
            "content":[{"type":"text","text":message}],
            "structuredContent":{"error":message},"isError":true
        }),
    }
}

fn representation_contains(result: &Value, token: &str) -> bool {
    !token.is_empty()
        && (result
            .get("structuredContent")
            .is_some_and(|value| value.to_string().contains(token))
            || result
                .get("content")
                .is_some_and(|value| value.to_string().contains(token)))
}

fn safe_result(result: Result<Value, String>, token: Option<&str>, read_only: bool) -> Value {
    let token = token.unwrap_or_default();
    let message = match result {
        Ok(value) => {
            let result = result_value(Ok(value));
            if !representation_contains(&result, token) {
                return result;
            }
            credential_error(read_only)
        }
        Err(message) => message,
    };
    let message = if token.is_empty() {
        message
    } else {
        message.replace(token, "[REDACTED]")
    };
    let message: String = message
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let mut result = result_value(Err(bounded_error(message, read_only)));
    if representation_contains(&result, token) {
        result = result_value(Err("error: details withheld".into()));
        if representation_contains(&result, token) {
            result = result_value(Err(String::new()));
            // A short credential can collide with fixed representation keys.
            if result["structuredContent"].to_string().contains(token) {
                result
                    .as_object_mut()
                    .expect("tool result")
                    .remove("structuredContent");
            }
            if result["content"].to_string().contains(token) {
                result["content"] = json!([]);
                if result["content"].to_string().contains(token) {
                    result
                        .as_object_mut()
                        .expect("tool result")
                        .remove("content");
                }
            }
        }
    }
    result
}

fn response_error(read_only: bool, message: &str) -> String {
    if read_only {
        message.into()
    } else {
        format!("{message}; mutation outcome UNKNOWN; reconcile by receipt before retrying")
    }
}

fn transport_error(read_only: bool) -> String {
    response_error(
        read_only,
        "server_unavailable: memory transport failure or timeout",
    )
}

fn bounded_error(message: String, read_only: bool) -> String {
    let result = json!({"content":[{"type":"text","text":message}],"structuredContent":{"error":message},"isError":true});
    if result.to_string().len() > MAX_BYTES {
        response_error(
            read_only,
            "response_too_large: memory error result exceeds 8388608 bytes",
        )
    } else {
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_representation_guard() {
        assert!(representation_contains(
            &json!({"structuredContent":{"n":123},"content":[]}),
            "123"
        ));
    }

    #[test]
    fn text_representation_guard() {
        assert!(representation_contains(
            &json!({"structuredContent":{},"content":[{"type":"text","text":"123"}]}),
            "123"
        ));
    }

    #[test]
    fn diagnostic_fallback_and_shortest_safe_result() {
        let result = safe_result(
            Err("invalid_request: [REDACTED]".into()),
            Some("[REDACTED]"),
            false,
        );
        assert_eq!(
            result["structuredContent"]["error"],
            "error: details withheld"
        );
        for token in ["error", "text", "\"", "[", "]", ":", "{}"] {
            let result = safe_result(Err(format!("invalid_request: {token}")), Some(token), false);
            assert_eq!(result["isError"], true);
            assert!(
                !representation_contains(&result, token),
                "{token}: {result}"
            );
        }
    }

    #[test]
    fn diagnostic_redaction_and_controls() {
        let result = safe_result(
            Err("invalid_request: secret\n\tmessage".into()),
            Some("secret"),
            true,
        );
        assert_eq!(
            result["structuredContent"]["error"],
            "invalid_request: [REDACTED]  message"
        );
    }
}
