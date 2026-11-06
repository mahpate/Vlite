//! Key-value operations carried by the existing owned JSON ABI as
//! `{"op":"kv","request":{"op":"get"|"put"|"delete"|"list",...}}`.
//!
//! The request and response JSON are those of the local MCP and shell KV tools
//! (`src/bin/tool_support/kv_mcp.rs`): `collection` is required and a missing
//! one is never created, values travel as canonical padded base64, versions
//! and deadlines as canonical decimal strings, unknown and repeated request
//! fields are rejected, and a list page holds at most 16 MiB of compact JSON.
//! Errors use the ABI's `{"ok":false,"error":{"code","message"}}` envelope with
//! the tools' stable codes. A further operation is one `Request` variant, one
//! arm of `execute` and, when it writes, its name in `WRITES`.
use super::*;
use serde::{Deserializer, de};
use std::ops::ControlFlow;
use vlite::kv::{KvPut, MAX_KV_VALUE_BYTES};

const PAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_VALUE_BASE64: usize = MAX_KV_VALUE_BYTES.div_ceil(3) * 4;

// An entry always fits a page together with the cursor that names it, so a page
// is never empty for want of room: the largest value as base64, the key twice
// (item and cursor) at six bytes per key byte when escaped, and the field names.
// The key is bounded by the 16-bit length a stored record holds, not by
// `MAX_KV_KEY_BYTES`: a record planted through another route need not keep to it.
const _: () = assert!(MAX_VALUE_BASE64 + 2 * 6 * (u16::MAX as usize) + 512 < PAGE_BYTES);

/// The request object as JSON with a repeated key remembered: `Value` keeps
/// only the last occurrence, and the tools reject such a request.
pub(super) struct Body {
    value: Value,
    duplicate: bool,
}

impl Body {
    fn leaf(value: Value) -> Self {
        Self {
            value,
            duplicate: false,
        }
    }
}

impl<'de> Deserialize<'de> for Body {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        deserializer.deserialize_any(BodyVisitor)
    }
}

struct BodyVisitor;

impl<'de> de::Visitor<'de> for BodyVisitor {
    type Value = Body;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> std::result::Result<Body, A::Error> {
        let mut object = serde_json::Map::new();
        let mut duplicate = false;
        while let Some((key, member)) = map.next_entry::<String, Body>()? {
            duplicate |= member.duplicate;
            duplicate |= object.insert(key, member.value).is_some();
        }
        Ok(Body {
            value: Value::Object(object),
            duplicate,
        })
    }

    fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Body, A::Error> {
        let mut items = Vec::new();
        let mut duplicate = false;
        while let Some(item) = seq.next_element::<Body>()? {
            duplicate |= item.duplicate;
            items.push(item.value);
        }
        Ok(Body {
            value: Value::Array(items),
            duplicate,
        })
    }

    fn visit_str<E: de::Error>(self, text: &str) -> std::result::Result<Body, E> {
        Ok(Body::leaf(Value::String(text.to_owned())))
    }

    fn visit_bool<E: de::Error>(self, flag: bool) -> std::result::Result<Body, E> {
        Ok(Body::leaf(Value::Bool(flag)))
    }

    fn visit_u64<E: de::Error>(self, number: u64) -> std::result::Result<Body, E> {
        Ok(Body::leaf(Value::from(number)))
    }

    fn visit_i64<E: de::Error>(self, number: i64) -> std::result::Result<Body, E> {
        Ok(Body::leaf(Value::from(number)))
    }

    fn visit_f64<E: de::Error>(self, number: f64) -> std::result::Result<Body, E> {
        serde_json::Number::from_f64(number)
            .map(|number| Body::leaf(Value::Number(number)))
            .ok_or_else(|| E::custom("number must be finite"))
    }

    fn visit_unit<E: de::Error>(self) -> std::result::Result<Body, E> {
        Ok(Body::leaf(Value::Null))
    }
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Get {
        collection: String,
        namespace: String,
        key: String,
    },
    Put {
        collection: String,
        namespace: String,
        key: String,
        value_base64: String,
        #[serde(default)]
        expected: Expectation,
        expires_at_ms: Option<String>,
    },
    Delete {
        collection: String,
        namespace: String,
        key: String,
        #[serde(default)]
        expected: Expectation,
    },
    List {
        collection: String,
        namespace: String,
        #[serde(default)]
        prefix: String,
        #[serde(default, deserialize_with = "present_string")]
        after: Option<String>,
        #[serde(default = "default_limit")]
        limit: u64,
    },
}

fn present_string<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

fn default_limit() -> u64 {
    100
}

#[derive(Default)]
enum AnyAbsent {
    #[default]
    Any,
    Absent,
}

impl<'de> Deserialize<'de> for AnyAbsent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "any" => Ok(Self::Any),
            "absent" => Ok(Self::Absent),
            value => Err(de::Error::unknown_variant(value, &["any", "absent"])),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revision {
    revision: String,
}

/// `"any"`, `"absent"` or `{"revision":"<canonical decimal u64>"}`.
#[derive(Deserialize)]
#[serde(untagged)]
enum Expectation {
    Name(AnyAbsent),
    Revision(Revision),
}

impl Default for Expectation {
    fn default() -> Self {
        Self::Name(AnyAbsent::Any)
    }
}

impl Expectation {
    fn expected(&self) -> Result<Expected> {
        Ok(match self {
            Self::Name(AnyAbsent::Any) => Expected::Any,
            Self::Name(AnyAbsent::Absent) => Expected::Absent,
            Self::Revision(value) => Expected::Revision(decimal(&value.revision)?),
        })
    }
}

fn decimal(text: &str) -> Result<u64> {
    if text.is_empty()
        || (text.len() > 1 && text.starts_with('0'))
        || !text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid("expected a canonical decimal u64 string"));
    }
    text.parse()
        .map_err(|_| invalid("decimal exceeds u64 range"))
}

fn bounded(text: &str, name: &str, min: usize, max: usize) -> Result<()> {
    if !(min..=max).contains(&text.len()) {
        return Err(invalid(&format!("{name} must be {min}..{max} UTF-8 bytes")));
    }
    Ok(())
}

fn address(collection: &str, namespace: &str, key: Option<&str>) -> Result<()> {
    if collection.is_empty() {
        return Err(invalid("collection must not be empty"));
    }
    bounded(namespace, "namespace", 1, 255)?;
    if let Some(key) = key {
        bounded(key, "key", 1, 1024)?;
    }
    Ok(())
}

/// The tools' stable codes. They differ from the ABI's generic `failure` in the
/// cases the key-value surface names itself (`upgrade_required`,
/// `not_kv_collection`, ...), and in an unmapped error being `database_error`.
fn kv_failure(error: vlite::Error) -> Failure {
    use vlite::Error as E;
    let code = match &error {
        E::CollectionNotFound => "collection_not_found",
        E::NotKvCollection => "not_kv_collection",
        E::KvSlotsExhausted => "kv_slots_exhausted",
        E::InvalidKvKey(_) | E::PayloadTooLarge(_) | E::InvalidCollectionName => "invalid_input",
        E::ExpiryRequiresUpgrade | E::BatchRequiresUpgrade | E::MetadataRequiresUpgrade => {
            "upgrade_required"
        }
        E::RevisionMismatch { .. } => "precondition_failed",
        E::ReadOnly => "read_only",
        E::Io(_) => "io_error",
        E::Locked => "locked",
        E::CommitIndeterminate(_) => "commit_indeterminate",
        E::Poisoned => "poisoned",
        E::Corrupt { .. } => "corrupt",
        E::IncompleteTail { .. } => "incomplete_tail",
        E::GraphAttachedRecordExpiry => "graph_attached_record_expiry",
        E::GraphAttachedRecord => "graph_attached_record",
        _ => "database_error",
    };
    (code, error.to_string())
}

/// The operations that change the file; `execute` refuses them on a read-only
/// handle before it reads the request any further, as the tools do.
const WRITES: [&str; 2] = ["put", "delete"];

fn writable(db: &Database, operation: &str) -> Result<()> {
    if db.is_read_only() {
        return Err((
            "read_only",
            format!("database is read-only; kv {operation} is unavailable"),
        ));
    }
    Ok(())
}

fn decode_value(text: &str) -> Result<Vec<u8>> {
    if text.len() > MAX_VALUE_BASE64 {
        return Err(too_large());
    }
    let value = BASE64_STANDARD
        .decode(text)
        .map_err(|_| invalid("value_base64 must be canonical padded standard base64"))?;
    if value.len() > MAX_KV_VALUE_BYTES {
        return Err(too_large());
    }
    Ok(value)
}

fn too_large() -> Failure {
    invalid(&format!("value exceeds {MAX_KV_VALUE_BYTES} decoded bytes"))
}

pub(super) fn execute(state: &mut State, handle: u64, body: Body) -> Result<Value> {
    if body.duplicate {
        return Err(("invalid_request", "duplicate argument field".into()));
    }
    // serde reads an internally tagged enum from a sequence too; the dialect is an object.
    if !body.value.is_object() {
        return Err(("invalid_request", "request must be a JSON object".into()));
    }
    // A read-only handle refuses a write before the request is read any further,
    // so a precondition, a bound or a malformed field never masks `read_only`.
    // A repeated field and a request that is not an object come first, as in
    // vlite-mcp (vlite-shell checks read-only before a repeated field).
    let write = body
        .value
        .get("op")
        .and_then(Value::as_str)
        .filter(|operation| WRITES.contains(operation));
    if let (Some(operation), Some(db)) = (write, state.databases.get(&handle)) {
        writable(db, operation)?;
    }
    let request: Request = serde_json::from_value(body.value)
        .map_err(|error| ("invalid_request", error.to_string()))?;
    let db = state
        .databases
        .get_mut(&handle)
        .ok_or_else(|| ("invalid_handle", "Unknown or closed handle".into()))?;
    match request {
        Request::Get {
            collection,
            namespace,
            key,
        } => {
            address(&collection, &namespace, Some(&key))?;
            let entry = db
                .kv_view(&collection)
                .map_err(kv_failure)?
                .get(&namespace, &key)
                .map_err(kv_failure)?
                .ok_or_else(|| ("kv_not_found", "key is absent or expired".into()))?;
            Ok(item_json(
                &entry.key,
                &entry.value,
                entry.version,
                entry.expires_at_ms,
            ))
        }
        Request::Put {
            collection,
            namespace,
            key,
            value_base64,
            expected,
            expires_at_ms,
        } => {
            address(&collection, &namespace, Some(&key))?;
            let expected = expected.expected()?;
            let expires_at_ms = expires_at_ms.as_deref().map(decimal).transpose()?;
            let value = decode_value(&value_base64)?;
            // `kv_store` creates a missing collection; this surface must not.
            db.kv_view(&collection).map_err(kv_failure)?;
            let version = db
                .kv_store(&collection)
                .map_err(kv_failure)?
                .put(
                    &namespace,
                    &key,
                    &value,
                    KvPut {
                        expected,
                        expires_at_ms,
                    },
                )
                .map_err(kv_failure)?;
            Ok(
                json!({"version":version.to_string(),"expires_at_ms":expires_at_ms.map(|deadline|deadline.to_string())}),
            )
        }
        Request::Delete {
            collection,
            namespace,
            key,
            expected,
        } => {
            address(&collection, &namespace, Some(&key))?;
            let expected = expected.expected()?;
            db.kv_view(&collection).map_err(kv_failure)?;
            let deleted = db
                .kv_store(&collection)
                .map_err(kv_failure)?
                .delete(&namespace, &key, expected)
                .map_err(kv_failure)?;
            Ok(json!({"deleted":deleted}))
        }
        Request::List {
            collection,
            namespace,
            prefix,
            after,
            limit,
        } => {
            address(&collection, &namespace, None)?;
            bounded(&prefix, "prefix", 0, 1024)?;
            if let Some(after) = &after {
                bounded(after, "after", 1, 1024)?;
            }
            if !(1..=10_000).contains(&limit) {
                return Err(invalid("limit must be 1..10000"));
            }
            page(
                db,
                &collection,
                &namespace,
                &prefix,
                after.as_deref(),
                limit as usize,
            )
        }
    }
}

/// One entry of a response: the key, the base64 value, and the decimal
/// version and deadline.
fn item_json(key: &str, value: &[u8], version: u64, expires_at_ms: Option<u64>) -> Value {
    json!({"key":key,"value_base64":BASE64_STANDARD.encode(value),
        "version":version.to_string(),"expires_at_ms":expires_at_ms.map(|deadline|deadline.to_string())})
}

/// The compact JSON size of `item_json`, found from the length of the value
/// alone so that a value is never encoded just to be measured: the item with an
/// empty value plus four base64 characters for every started group of three
/// value bytes.
fn item_size(key: &str, value_len: usize, version: u64, expires_at_ms: Option<u64>) -> usize {
    json!({"key":key,"value_base64":"","version":version.to_string(),
        "expires_at_ms":expires_at_ms.map(|deadline|deadline.to_string())})
    .to_string()
    .len()
        + value_len.div_ceil(3) * 4
}

fn page(
    db: &Database,
    collection: &str,
    namespace: &str,
    prefix: &str,
    after: Option<&str>,
    limit: usize,
) -> Result<Value> {
    let view = db.kv_view(collection).map_err(kv_failure)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| ("internal_error", error.to_string()))?
        .as_millis()
        .try_into()
        .map_err(|_| {
            (
                "internal_error",
                "system clock exceeds u64 milliseconds".to_owned(),
            )
        })?;
    visit_page(after, limit, |after, limit, visit| {
        view.visit_at(namespace, prefix, after, limit, now, |entry| {
            visit(Lent {
                key: entry.key,
                value: entry.value,
                version: entry.version,
                expires_at_ms: entry.expires_at_ms,
                has_next: entry.has_next,
            })
        })
        .map_err(kv_failure)
    })
}

/// An entry as `visit_page` reads it. `KvRef` is `#[non_exhaustive]`, so only
/// the library can build one: the tests stand in for `KvView::visit_at` with
/// this copy of its fields.
struct Lent<'a> {
    key: &'a str,
    value: &'a [u8],
    version: u64,
    expires_at_ms: Option<u64>,
    has_next: bool,
}

/// One page of at most `limit` entries and `PAGE_BYTES` of compact JSON, built
/// from a single scan. `scan` calls its visitor, in key order, for at most
/// `limit` entries; the visitor stops it at the first entry that does not fit.
/// An entry's value is measured from the borrow and only encoded when the
/// entry is added, so what is read is the emitted entries and, at most, the one
/// that did not fit.
fn visit_page(
    after: Option<&str>,
    limit: usize,
    scan: impl FnOnce(Option<&str>, usize, &mut dyn FnMut(Lent<'_>) -> ControlFlow<()>) -> Result<()>,
) -> Result<Value> {
    let mut items = Vec::new();
    let mut item_bytes = 0;
    // The last emitted key, and whether another live entry follows it.
    let mut last: Option<(String, bool)> = None;
    scan(after, limit, &mut |entry| {
        let bytes = item_size(
            entry.key,
            entry.value.len(),
            entry.version,
            entry.expires_at_ms,
        );
        // As the last entry of the page it carries the cursor, when more follow.
        let next = entry.has_next.then_some(entry.key);
        let envelope = json!({"items":[],"next_after":next}).to_string().len();
        let size = envelope + item_bytes + bytes + items.len();
        if size > PAGE_BYTES {
            return ControlFlow::Break(());
        }
        item_bytes += bytes;
        items.push(item_json(
            entry.key,
            entry.value,
            entry.version,
            entry.expires_at_ms,
        ));
        last = Some((entry.key.to_owned(), entry.has_next));
        ControlFlow::Continue(())
    })?;
    // The last emitted entry knows whether another live entry follows it: the
    // one that did not fit, or one beyond `limit`.
    let next = last
        .as_ref()
        .filter(|(_, more)| *more)
        .map(|(key, _)| key.as_str());
    // The items move into the response instead of being copied by `json!`.
    let mut body = serde_json::Map::with_capacity(2);
    body.insert("items".to_owned(), Value::Array(items));
    body.insert(
        "next_after".to_owned(),
        next.map_or(Value::Null, |key| Value::String(key.to_owned())),
    );
    Ok(Value::Object(body))
}

#[cfg(test)]
mod tests {
    use super::super::{vlite_execute, vlite_response_free};
    use super::{MAX_VALUE_BASE64, PAGE_BYTES};
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::path::Path;

    /// The goldens the MCP and shell tools and every binding replay.
    const GOLDENS: &str = include_str!("../../../tests/fixtures/kv/kv_dialect_goldens.json");

    fn call_text(handle: u64, request: &str) -> Value {
        unsafe {
            let response = vlite_execute(handle, request.as_ptr(), request.len());
            let value =
                serde_json::from_slice(std::ffi::CStr::from_ptr(response).to_bytes()).unwrap();
            vlite_response_free(response);
            value
        }
    }

    fn call(handle: u64, value: Value) -> Value {
        call_text(handle, &value.to_string())
    }

    fn kv(handle: u64, request: Value) -> Value {
        call(handle, json!({"op": "kv", "request": request}))
    }

    fn ok(handle: u64, request: Value) -> Value {
        let response = kv(handle, request.clone());
        assert_eq!(response["ok"], true, "{request} -> {response}");
        response["result"].clone()
    }

    fn code(handle: u64, request: Value) -> String {
        let response = kv(handle, request.clone());
        assert_eq!(response["ok"], false, "{request} -> {response}");
        response["error"]["code"].as_str().unwrap().to_owned()
    }

    fn handle(response: &Value) -> u64 {
        assert_eq!(response["ok"], true, "{response}");
        response["result"]["handle"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }

    fn memory(dimensions: usize) -> u64 {
        handle(&call(
            0,
            json!({"op": "features", "request": {"op": "create", "path": null,
                "dimensions": dimensions}}),
        ))
    }

    fn create_file(path: &Path, format: Option<u32>) -> u64 {
        let mut request = json!({"op": "create", "path": path, "dimensions": 1});
        if let Some(format) = format {
            request["format"] = json!(format);
        }
        handle(&call(0, request))
    }

    fn open_file(path: &Path, read_only: bool) -> u64 {
        handle(&call(
            0,
            json!({"op": "open", "path": path, "read_only": read_only}),
        ))
    }

    fn close(handle: u64) {
        assert_eq!(call(handle, json!({"op": "close"}))["ok"], true);
    }

    fn put(handle: u64, key: &str, value_base64: &str) -> Value {
        ok(
            handle,
            json!({"op": "put", "collection": "default", "namespace": "n", "key": key,
                "value_base64": value_base64}),
        )
    }

    // The goldens: `$ref:NAME` in a request or result is the bound decimal,
    // `{"$repeat":[text,count],"$suffix":text}` a long string, and a result
    // string `$bind:NAME` or `$bind:NAME>OTHER` binds a commit version (greater
    // than OTHER's).

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
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| expand(item, bound)).collect())
            }
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

    fn skipped(node: &Value) -> bool {
        node.get("skip")
            .and_then(Value::as_array)
            .is_some_and(|surfaces| surfaces.iter().any(|surface| surface == "c"))
    }

    fn replay(sequence: &Value, directory: &Path) -> usize {
        let name = sequence["name"].as_str().unwrap();
        let none = BTreeMap::new();
        let db = if sequence["mode"] == "read_only" {
            let path = directory.join(format!("{name}.vlite"));
            let writer = create_file(&path, None);
            for seed in sequence["seed"].as_array().unwrap() {
                ok(writer, expand(seed, &none));
            }
            close(writer);
            open_file(&path, true)
        } else {
            let db = memory(1);
            for collection in sequence["collections"].as_array().into_iter().flatten() {
                let response = call(
                    db,
                    json!({"op": "create_collection", "name": collection["name"],
                        "dimensions": collection["dimensions"]}),
                );
                assert_eq!(response["ok"], true, "{name}: {response}");
            }
            db
        };
        let mut bound = BTreeMap::new();
        let mut replayed = 0;
        for step in sequence["steps"].as_array().unwrap() {
            if skipped(step) {
                continue;
            }
            replayed += 1;
            let step_name = step["name"].as_str().unwrap();
            let request = expand(&step["request"], &bound);
            let response = kv(db, request.clone());
            let context = format!("{name}/{step_name}: {request} -> {response}");
            match (step.get("result"), step.get("error")) {
                (Some(result), None) => {
                    assert_eq!(response["ok"], true, "{context}");
                    let expected = expand(result, &bound);
                    assert!(
                        matches(&expected, &response["result"], &mut bound),
                        "{context}; expected {expected}"
                    );
                }
                (None, Some(error)) => {
                    assert_eq!(response["ok"], false, "{context}");
                    assert_eq!(response["error"]["code"], *error, "{context}");
                    assert!(
                        response["error"]["message"]
                            .as_str()
                            .is_some_and(|message| !message.is_empty()),
                        "{context}"
                    );
                }
                _ => panic!("{name}/{step_name} needs exactly one of result and error"),
            }
        }
        close(db);
        replayed
    }

    #[test]
    fn replays_the_shared_dialect_goldens() {
        let goldens: Value = serde_json::from_str(GOLDENS).unwrap();
        assert_eq!(goldens["format"], 1);
        let directory = tempfile::tempdir().unwrap();
        let sequences: Vec<&Value> = goldens["sequences"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|sequence| !skipped(sequence))
            .collect();
        let expected: usize = sequences
            .iter()
            .map(|sequence| {
                let steps = sequence["steps"].as_array().unwrap();
                steps.iter().filter(|step| !skipped(step)).count()
            })
            .sum();
        let replayed: usize = sequences
            .iter()
            .map(|sequence| replay(sequence, directory.path()))
            .sum();
        // A `skip` added to the file must not quietly remove coverage.
        assert_eq!(replayed, expected);
        assert!(replayed >= 100, "only {replayed} golden steps replayed");
    }

    #[test]
    fn repeated_request_fields_are_rejected_at_any_depth() {
        let db = memory(1);
        for request in [
            r#"{"op":"kv","request":{"op":"get","collection":"default","collection":"default","namespace":"n","key":"a"}}"#,
            r#"{"op":"kv","request":{"op":"get","op":"get","collection":"default","namespace":"n","key":"a"}}"#,
            r#"{"op":"kv","request":{"op":"delete","collection":"default","namespace":"n","key":"a","expected":{"revision":"1","revision":"1"}}}"#,
        ] {
            let response = call_text(db, request);
            assert_eq!(response["ok"], false, "{request}");
            assert_eq!(response["error"]["code"], "invalid_request", "{request}");
            assert_eq!(
                response["error"]["message"], "duplicate argument field",
                "{request}"
            );
        }
        // The same names at different depths are not repeats.
        assert_eq!(
            code(
                db,
                json!({"op": "delete", "collection": "default", "namespace": "n", "key": "a",
                    "expected": {"revision": "1"}})
            ),
            "precondition_failed"
        );
        close(db);
    }

    #[test]
    fn malformed_envelopes_requests_and_handles_report_stable_codes() {
        let db = memory(1);
        for request in [
            json!({}),
            json!({"op": "frobnicate"}),
            json!({"op": ""}),
            json!([]),
            // serde would read these as an internally tagged enum from a sequence.
            json!(["get", "default", "n", "a"]),
            json!(["put", "absent", "n", "a", ""]),
            json!("get"),
            json!(null),
            json!(7),
        ] {
            assert_eq!(code(db, request.clone()), "invalid_request", "{request}");
        }
        // A missing or repeated envelope field is the ABI's own parse error.
        for request in [
            r#"{"op":"kv"}"#,
            r#"{"op":"kv","request":{"op":"list","collection":"default","namespace":"n"},"extra":1}"#,
            r#"{"op":"kv","request":{"op":"list","collection":"default","namespace":"n"},"request":{"op":"list","collection":"default","namespace":"n"}}"#,
        ] {
            assert_eq!(call_text(db, request)["error"]["code"], "invalid_input");
        }
        let get = json!({"op": "get", "collection": "default", "namespace": "n", "key": "a"});
        assert_eq!(code(db, get.clone()), "kv_not_found");
        close(db);
        assert_eq!(code(db, get.clone()), "invalid_handle");
        assert_eq!(code(u64::MAX, get), "invalid_handle");
    }

    #[test]
    fn a_missing_collection_is_never_created() {
        let db = memory(1);
        for request in [
            json!({"op": "put", "collection": "absent", "namespace": "n", "key": "a",
                "value_base64": ""}),
            json!({"op": "delete", "collection": "absent", "namespace": "n", "key": "a"}),
            json!({"op": "put", "collection": "absent", "namespace": "n", "key": "a",
                "value_base64": "", "expected": "absent"}),
        ] {
            assert_eq!(
                code(db, request.clone()),
                "collection_not_found",
                "{request}"
            );
        }
        let collections = call(db, json!({"op": "list_collections"}));
        let names: Vec<&str> = collections["result"]["collections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|collection| collection["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["default"]);
        close(db);
    }

    #[test]
    fn a_full_probe_window_reports_kv_slots_exhausted() {
        let db = memory(1);
        // Ordinary vector records on every slot the key may use hold them.
        for offset in 0..vlite::kv::KV_PROBE_SLOTS {
            let id = vlite::kv::first_slot("n", "a").wrapping_add(offset);
            let response = call(
                db,
                json!({"op": "put", "id": id.to_string(), "vector": [0.0]}),
            );
            assert_eq!(response["ok"], true, "{response}");
        }
        assert_eq!(
            code(
                db,
                json!({"op": "put", "collection": "default", "namespace": "n",
            "key": "a", "value_base64": ""})
            ),
            "kv_slots_exhausted"
        );
        assert_eq!(
            ok(
                db,
                json!({"op": "delete", "collection": "default", "namespace": "n", "key": "a"})
            ),
            json!({"deleted": false})
        );
        put(db, "other", "AA==");
        close(db);
    }

    #[test]
    fn a_format_4_file_stores_values_but_needs_an_upgrade_for_expiry() {
        let directory = tempfile::tempdir().unwrap();
        let db = create_file(&directory.path().join("v4.vlite"), Some(4));
        let version = put(db, "a", "AAEC")["version"].as_str().unwrap().to_owned();
        assert_eq!(
            ok(
                db,
                json!({"op": "get", "collection": "default", "namespace": "n", "key": "a"})
            ),
            json!({"key": "a", "value_base64": "AAEC", "version": version, "expires_at_ms": null})
        );
        for expires_at_ms in ["0", "18446744073709551615"] {
            assert_eq!(
                code(
                    db,
                    json!({"op": "put", "collection": "default", "namespace": "n",
                    "key": "a", "value_base64": "", "expires_at_ms": expires_at_ms})
                ),
                "upgrade_required"
            );
        }
        close(db);
    }

    #[test]
    fn a_read_only_handle_refuses_writes_before_any_other_check() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("read-only.vlite");
        let writer = create_file(&path, None);
        let version = put(writer, "a", "AAEC")["version"]
            .as_str()
            .unwrap()
            .to_owned();
        close(writer);
        let db = open_file(&path, true);
        for request in [
            json!({"op": "put", "collection": "default", "namespace": "n", "key": "b",
                "value_base64": ""}),
            json!({"op": "put", "collection": "default", "namespace": "n", "key": "a",
                "value_base64": "", "expected": "absent"}),
            json!({"op": "put", "collection": "absent", "namespace": "n", "key": "a",
                "value_base64": "", "expires_at_ms": "0"}),
            json!({"op": "put", "collection": "default", "namespace": "", "key": "a",
                "value_base64": "***"}),
            json!({"op": "delete", "collection": "default", "namespace": "n", "key": "a",
                "expected": {"revision": "1"}}),
            json!({"op": "delete", "collection": "absent", "namespace": "n", "key": "a"}),
            // Even a malformed write is refused first.
            json!({"op": "put"}),
            json!({"op": "put", "collection": "default", "namespace": "n", "key": "a",
                "value_base64": "", "extra": 1}),
            json!({"op": "delete", "namespace": 7}),
        ] {
            assert_eq!(code(db, request.clone()), "read_only", "{request}");
        }
        // A malformed read is still an invalid request.
        assert_eq!(
            code(db, json!({"op": "get", "collection": "default"})),
            "invalid_request"
        );
        // A repeated field and a request that is not an object are reported
        // before the read-only refusal, as in vlite-mcp.
        let repeated = call_text(
            db,
            r#"{"op":"kv","request":{"op":"put","op":"put","collection":"default","namespace":"n","key":"b","value_base64":""}}"#,
        );
        assert_eq!(repeated["error"]["code"], "invalid_request");
        assert_eq!(repeated["error"]["message"], "duplicate argument field");
        assert_eq!(
            code(db, json!(["put", "default", "n", "b", ""])),
            "invalid_request"
        );
        assert_eq!(
            ok(
                db,
                json!({"op": "get", "collection": "default", "namespace": "n", "key": "a"})
            ),
            json!({"key": "a", "value_base64": "AAEC", "version": version, "expires_at_ms": null})
        );
        assert_eq!(
            ok(
                db,
                json!({"op": "list", "collection": "default", "namespace": "n"})
            )["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        close(db);
    }

    #[test]
    fn values_are_canonical_padded_standard_base64() {
        let db = memory(1);
        for text in [
            "", "AA==", "AAE=", "AAEC", "+w==", "/w==", "+/8=", "AA+/", "AAECAwQF",
        ] {
            let version = put(db, "k", text)["version"].as_str().unwrap().to_owned();
            assert_eq!(
                ok(
                    db,
                    json!({"op": "get", "collection": "default", "namespace": "n", "key": "k"})
                ),
                json!({"key": "k", "value_base64": text, "version": version,
                    "expires_at_ms": null}),
                "{text:?}"
            );
        }
        for text in [
            "AB==", "AAF=", "AA", "A", "AAA", "AA==AAAA", "AA=A", "AA-_", " AA=", "AA= ", "AA==\n",
            "AAEC\n", "====", "A===", "=AAA", "AAé=", "é", "AAAAA",
        ] {
            assert_eq!(
                code(
                    db,
                    json!({"op": "put", "collection": "default", "namespace": "n",
                    "key": "k", "value_base64": text})
                ),
                "invalid_input",
                "{text:?}"
            );
        }
        close(db);
    }

    #[test]
    fn decimals_are_canonical_u64_strings() {
        let db = memory(1);
        for text in ["0", "1", "9007199254740993", "18446744073709551615"] {
            assert_eq!(
                put_with_deadline(db, text)["expires_at_ms"],
                json!(text),
                "{text}"
            );
        }
        for text in [
            "",
            "00",
            "01",
            "-1",
            "+1",
            "1.5",
            "1e3",
            "0x10",
            " 1",
            "1 ",
            "١",
            "٣",
            "18446744073709551616",
            "99999999999999999999999",
        ] {
            let response = kv(
                db,
                json!({"op": "put", "collection": "default", "namespace": "n", "key": "d",
                    "value_base64": "", "expires_at_ms": text}),
            );
            assert_eq!(response["error"]["code"], "invalid_input", "{text:?}");
        }
        close(db);
    }

    fn put_with_deadline(handle: u64, deadline: &str) -> Value {
        ok(
            handle,
            json!({"op": "put", "collection": "default", "namespace": "n", "key": "d",
                "value_base64": "", "expires_at_ms": deadline}),
        )
    }

    // Page fitting, without a database: a scan lends entries whose values are
    // slices of one zero buffer.

    struct Spec {
        key: String,
        value_len: usize,
        version: u64,
        expires_at_ms: Option<u64>,
    }

    fn spec(key: &str, value_len: usize) -> Spec {
        Spec {
            key: key.to_owned(),
            value_len,
            version: 7,
            expires_at_ms: None,
        }
    }

    fn page_of(specs: &[Spec], limit: usize) -> Value {
        let zeros = vec![0u8; specs.iter().map(|spec| spec.value_len).max().unwrap_or(0)];
        super::visit_page(None, limit, |_, limit, visit| {
            for (index, spec) in specs.iter().take(limit).enumerate() {
                let flow = visit(super::Lent {
                    key: &spec.key,
                    value: &zeros[..spec.value_len],
                    version: spec.version,
                    expires_at_ms: spec.expires_at_ms,
                    has_next: index + 1 < specs.len(),
                });
                if flow.is_break() {
                    break;
                }
            }
            Ok(())
        })
        .unwrap()
    }

    #[test]
    fn a_page_holds_exactly_the_entries_that_fit_in_16_mib() {
        let specs: Vec<Spec> = (0..13)
            .map(|index| spec(&format!("k{index:02}"), vlite::kv::MAX_KV_VALUE_BYTES))
            .collect();
        let page = page_of(&specs, 10_000);
        let items = page["items"].as_array().unwrap();
        assert_eq!(items.len(), 12);
        assert_eq!(page["next_after"], "k11");
        assert!(page.to_string().len() <= PAGE_BYTES);
        // One more entry would not fit.
        let mut larger = page.clone();
        larger["items"]
            .as_array_mut()
            .unwrap()
            .push(super::item_json(
                "k12",
                &vec![0; vlite::kv::MAX_KV_VALUE_BYTES],
                7,
                None,
            ));
        assert!(larger.to_string().len() > PAGE_BYTES);
        // The limit stops a page that has room left, with the cursor.
        let short = page_of(&specs, 2);
        assert_eq!(short["items"].as_array().unwrap().len(), 2);
        assert_eq!(short["next_after"], "k01");
        let all = page_of(&specs[..2], 2);
        assert_eq!(all["next_after"], Value::Null);
    }

    /// The compact JSON a page of `specs` has when every entry fits.
    fn json_len(specs: &[Spec]) -> usize {
        let items: Vec<Value> = specs
            .iter()
            .map(|spec| {
                super::item_json(
                    &spec.key,
                    &vec![0; spec.value_len],
                    spec.version,
                    spec.expires_at_ms,
                )
            })
            .collect();
        json!({"items": items, "next_after": null})
            .to_string()
            .len()
    }

    #[test]
    fn the_page_size_limit_is_inclusive() {
        let pair = |value_len: usize, key_len: usize| {
            vec![spec("a", value_len), spec(&"z".repeat(key_len), value_len)]
        };
        // Each 3 value bytes add 4 characters to both items; the second key's
        // length then tunes the page to the byte.
        let value_len = (PAGE_BYTES - json_len(&pair(0, 1)) - 100) / 8 * 3;
        let base = json_len(&pair(value_len, 1));
        let key_len = 1 + PAGE_BYTES - base;
        let exact = pair(value_len, key_len);
        assert_eq!(json_len(&exact), PAGE_BYTES);
        let page = page_of(&exact, 2);
        assert_eq!(page["items"].as_array().unwrap().len(), 2);
        assert_eq!(page["next_after"], Value::Null);
        assert_eq!(page.to_string().len(), PAGE_BYTES);
        // One byte more and the second entry moves to the next page.
        let over = pair(value_len, key_len + 1);
        assert_eq!(json_len(&over), PAGE_BYTES + 1);
        let page = page_of(&over, 2);
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert_eq!(page["next_after"], "a");
    }

    #[test]
    fn listing_defaults_to_a_hundred_entries() {
        let db = memory(1);
        for index in 0..101 {
            put(db, &format!("k{index:03}"), "");
        }
        let page = ok(
            db,
            json!({"op": "list", "collection": "default", "namespace": "n"}),
        );
        assert_eq!(page["items"].as_array().unwrap().len(), 100);
        assert_eq!(page["next_after"], "k099");
        let rest = ok(
            db,
            json!({"op": "list", "collection": "default", "namespace": "n",
                "after": page["next_after"]}),
        );
        assert_eq!(rest["items"].as_array().unwrap().len(), 1);
        assert_eq!(rest["next_after"], Value::Null);
        close(db);
    }

    #[test]
    fn listing_returns_pages_of_real_values() {
        let db = memory(1);
        // The largest value: 349096 groups of three bytes and two bytes more.
        let value = "AAAA".repeat(MAX_VALUE_BASE64 / 4 - 1) + "AAA=";
        assert_eq!(value.len(), MAX_VALUE_BASE64);
        for index in 0..13 {
            put(db, &format!("k{index:02}"), &value);
        }
        let first = ok(
            db,
            json!({"op": "list", "collection": "default", "namespace": "n", "limit": 10000}),
        );
        assert_eq!(first["items"].as_array().unwrap().len(), 12);
        assert_eq!(first["next_after"], "k11");
        assert!(first.to_string().len() <= PAGE_BYTES);
        let second = ok(
            db,
            json!({"op": "list", "collection": "default", "namespace": "n",
                "after": first["next_after"]}),
        );
        assert_eq!(second["items"].as_array().unwrap().len(), 1);
        assert_eq!(second["items"][0]["key"], "k12");
        assert_eq!(second["items"][0]["value_base64"], value);
        assert_eq!(second["next_after"], Value::Null);
        close(db);
    }
}
