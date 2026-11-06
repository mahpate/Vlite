//! Local KV requests shared by MCP and the shell.

use serde::{Deserialize, Deserializer, de};
use serde_json::{Value, json};
use std::{collections::HashMap, ops::ControlFlow};
use vlite::{
    Database, Error, Expected,
    kv::{
        KvEntry, KvOp, KvOutcome, KvPut, MAX_KV_BATCH_OPERATIONS, MAX_KV_BATCH_VALUE_BYTES,
        MAX_KV_GET_MANY_KEYS, MAX_KV_VALUE_BYTES,
    },
};

const GET: &str = "vlite_kv_get";
const PUT: &str = "vlite_kv_put";
const DELETE: &str = "vlite_kv_delete";
const LIST: &str = "vlite_kv_list";
const GET_MANY: &str = "vlite_kv_get_many";
const BATCH: &str = "vlite_kv_batch";
const PAGE_BYTES: usize = 16 * 1024 * 1024;

// An entry always fits a page together with the cursor that names it, so a page
// is never empty for want of room: the largest value as base64, the key twice
// (item and cursor) at six bytes per key byte when escaped, and the field names.
// The key is bounded by the 16-bit length a stored record holds, not by
// `MAX_KV_KEY_BYTES`: a record planted through another route need not keep to it.
const _: () =
    assert!(MAX_KV_VALUE_BYTES.div_ceil(3) * 4 + 2 * 6 * (u16::MAX as usize) + 512 < PAGE_BYTES);

#[cfg(test)]
thread_local! {
    /// Value bytes this thread has base64-encoded for a response.
    static ENCODED_VALUE_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Get {
    collection: String,
    namespace: String,
    key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Put {
    collection: String,
    namespace: String,
    key: String,
    value_base64: String,
    #[serde(default)]
    expected: Expectation,
    expires_at_ms: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Delete {
    collection: String,
    namespace: String,
    key: String,
    #[serde(default)]
    expected: Expectation,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    collection: String,
    namespace: String,
    #[serde(default)]
    prefix: String,
    #[serde(default, deserialize_with = "present_string")]
    after: Option<String>,
    #[serde(default = "default_limit")]
    limit: u64,
}

fn present_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

fn default_limit() -> u64 {
    100
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Key {
    namespace: String,
    key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetMany {
    collection: String,
    keys: Vec<Key>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    collection: String,
    operations: Vec<Operation>,
}

/// A batch put has no `expires_at_ms`: the batch is one engine frame, which
/// has no deadline, so the field is unknown and refused.
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
enum Operation {
    Put {
        namespace: String,
        key: String,
        value_base64: String,
        #[serde(default)]
        expected: Expectation,
    },
    Delete {
        namespace: String,
        key: String,
        #[serde(default)]
        expected: Expectation,
    },
}

#[derive(Default)]
enum AnyAbsent {
    #[default]
    Any,
    Absent,
}

impl<'de> Deserialize<'de> for AnyAbsent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
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
    fn expected(&self) -> Result<Expected, String> {
        Ok(match self {
            Self::Name(AnyAbsent::Any) => Expected::Any,
            Self::Name(AnyAbsent::Absent) => Expected::Absent,
            Self::Revision(value) => Expected::Revision(decimal(&value.revision)?),
        })
    }
}

fn invalid(message: impl std::fmt::Display) -> String {
    format!("invalid_input: {message}")
}

fn decimal(text: &str) -> Result<u64, String> {
    if text.is_empty()
        || (text.len() > 1 && text.starts_with('0'))
        || !text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid("expected a canonical decimal u64 string"));
    }
    text.parse()
        .map_err(|_| invalid("decimal exceeds u64 range"))
}

fn bounded(text: &str, name: &str, min: usize, max: usize) -> Result<(), String> {
    if !(min..=max).contains(&text.len()) {
        return Err(invalid(format!("{name} must be {min}..{max} UTF-8 bytes")));
    }
    Ok(())
}

fn address(collection: &str, namespace: &str, key: Option<&str>) -> Result<(), String> {
    if collection.is_empty() {
        return Err(invalid("collection must not be empty"));
    }
    bounded(namespace, "namespace", 1, 255)?;
    if let Some(key) = key {
        bounded(key, "key", 1, 1024)?;
    }
    Ok(())
}

fn failure(error: Error) -> String {
    let code = match error {
        Error::CollectionNotFound => "collection_not_found",
        Error::NotKvCollection => "not_kv_collection",
        Error::KvSlotsExhausted => "kv_slots_exhausted",
        Error::InvalidKvKey(_) | Error::PayloadTooLarge(_) | Error::InvalidCollectionName => {
            "invalid_input"
        }
        Error::ExpiryRequiresUpgrade
        | Error::BatchRequiresUpgrade
        | Error::MetadataRequiresUpgrade => "upgrade_required",
        Error::RevisionMismatch { .. } => "precondition_failed",
        Error::ReadOnly => "read_only",
        Error::Io(_) => "io_error",
        Error::Locked => "locked",
        Error::CommitIndeterminate(_) => "commit_indeterminate",
        Error::Poisoned => "poisoned",
        Error::Corrupt { .. } => "corrupt",
        Error::IncompleteTail { .. } => "incomplete_tail",
        Error::GraphAttachedRecordExpiry => "graph_attached_record_expiry",
        Error::GraphAttachedRecord => "graph_attached_record",
        _ => "database_error",
    };
    format!("{code}: {error}")
}

pub(crate) fn is_tool(name: &str) -> bool {
    matches!(name, GET | PUT | DELETE | LIST | GET_MANY | BATCH)
}

#[allow(dead_code)]
pub(crate) fn shell_tool(command: &str) -> Option<&'static str> {
    match command {
        "kv-get" => Some(GET),
        "kv-put" => Some(PUT),
        "kv-delete" => Some(DELETE),
        "kv-list" => Some(LIST),
        "kv-get-many" => Some(GET_MANY),
        "kv-batch" => Some(BATCH),
        _ => None,
    }
}

pub(crate) fn call(
    db: &mut Database,
    read_only: bool,
    name: &str,
    args: &Value,
) -> Option<Result<Value, String>> {
    is_tool(name).then(|| run(db, read_only, name, &args.to_string()))
}

fn parse<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T, String> {
    serde_json::from_str(text).map_err(|error| format!("invalid_request: {error}"))
}

pub(crate) fn run(
    db: &mut Database,
    read_only: bool,
    name: &str,
    text: &str,
) -> Result<Value, String> {
    if read_only && matches!(name, PUT | DELETE | BATCH) {
        return Err(format!(
            "read_only: database is read-only; {name} is unavailable"
        ));
    }
    match name {
        GET => {
            let request: Get = parse(text)?;
            address(&request.collection, &request.namespace, Some(&request.key))?;
            let entry = db
                .kv_view(&request.collection)
                .map_err(failure)?
                .get(&request.namespace, &request.key)
                .map_err(failure)?
                .ok_or("kv_not_found: key is absent or expired")?;
            Ok(entry_json(&entry))
        }
        PUT => {
            let request: Put = parse(text)?;
            address(&request.collection, &request.namespace, Some(&request.key))?;
            let expected = request.expected.expected()?;
            let expires_at_ms = request.expires_at_ms.as_deref().map(decimal).transpose()?;
            if request.value_base64.len() > 1_396_388 {
                return Err(invalid("value exceeds 1047290 decoded bytes"));
            }
            let value = decode_base64(&request.value_base64)?;
            if value.len() > MAX_KV_VALUE_BYTES {
                return Err(invalid("value exceeds 1047290 decoded bytes"));
            }
            // kv_store creates missing collections; the serving surfaces must not.
            db.kv_view(&request.collection).map_err(failure)?;
            let version = db
                .kv_store(&request.collection)
                .map_err(failure)?
                .put(
                    &request.namespace,
                    &request.key,
                    &value,
                    KvPut {
                        expected,
                        expires_at_ms,
                    },
                )
                .map_err(failure)?;
            Ok(
                json!({"version":version.to_string(),"expires_at_ms":expires_at_ms.map(|n|n.to_string())}),
            )
        }
        DELETE => {
            let request: Delete = parse(text)?;
            address(&request.collection, &request.namespace, Some(&request.key))?;
            let expected = request.expected.expected()?;
            db.kv_view(&request.collection).map_err(failure)?;
            let deleted = db
                .kv_store(&request.collection)
                .map_err(failure)?
                .delete(&request.namespace, &request.key, expected)
                .map_err(failure)?;
            Ok(json!({"deleted":deleted}))
        }
        LIST => {
            let request: List = parse(text)?;
            address(&request.collection, &request.namespace, None)?;
            bounded(&request.prefix, "prefix", 0, 1024)?;
            if let Some(after) = &request.after {
                bounded(after, "after", 1, 1024)?;
            }
            if !(1..=10_000).contains(&request.limit) {
                return Err(invalid("limit must be 1..10000"));
            }
            page(db, &request)
        }
        GET_MANY => {
            let request: GetMany = parse(text)?;
            if request.collection.is_empty() {
                return Err(invalid("collection must not be empty"));
            }
            if request.keys.len() > MAX_KV_GET_MANY_KEYS {
                return Err(invalid(format!(
                    "a get-many takes at most {MAX_KV_GET_MANY_KEYS} keys"
                )));
            }
            for (index, key) in request.keys.iter().enumerate() {
                address(&request.collection, &key.namespace, Some(&key.key))
                    .map_err(|error| at("keys", index, error))?;
            }
            many_page(db, &request)
        }
        BATCH => {
            let request: Batch = parse(text)?;
            if request.collection.is_empty() {
                return Err(invalid("collection must not be empty"));
            }
            if request.operations.len() > MAX_KV_BATCH_OPERATIONS {
                return Err(invalid(format!(
                    "a batch takes at most {MAX_KV_BATCH_OPERATIONS} operations"
                )));
            }
            let checked = batch_checked(&request)?;
            // kv_store creates missing collections; the serving surfaces must not.
            db.kv_view(&request.collection).map_err(failure)?;
            let operations: Vec<KvOp<'_>> = checked.iter().map(Checked::operation).collect();
            let outcomes = db
                .kv_store(&request.collection)
                .map_err(failure)?
                .write_batch(&operations)
                .map_err(failure)?;
            let results = outcomes
                .iter()
                .map(|outcome| match outcome {
                    KvOutcome::Put { version } => Ok(json!({"version":version.to_string()})),
                    KvOutcome::Deleted(deleted) => Ok(json!({"deleted":deleted})),
                    _ => Err("internal_error: unknown KV batch outcome".to_owned()),
                })
                .collect::<Result<Vec<Value>, String>>()?;
            Ok(json!({"results":results}))
        }
        _ => Err(format!("invalid_request: unknown KV tool {name}")),
    }
}

/// Prefixes the message of a request error with the position of the part of
/// the request it is about.
fn at(list: &str, index: usize, error: String) -> String {
    match error.strip_prefix("invalid_input: ") {
        Some(message) => format!("invalid_input: {list}[{index}]: {message}"),
        None => error,
    }
}

/// One operation of a batch request that passed every check that needs no
/// store.
struct Checked<'a> {
    namespace: &'a str,
    key: &'a str,
    /// The decoded value of a put; `None` for a delete.
    value: Option<Vec<u8>>,
    expected: Expected,
}

impl Checked<'_> {
    fn operation(&self) -> KvOp<'_> {
        match &self.value {
            Some(value) => KvOp::Put {
                namespace: self.namespace,
                key: self.key,
                value,
                options: KvPut {
                    expected: self.expected,
                    expires_at_ms: None,
                },
            },
            None => KvOp::Delete {
                namespace: self.namespace,
                key: self.key,
                expected: self.expected,
            },
        }
    }
}

/// Checks a whole batch request in operation order, as the REST route does, so
/// the first operation that cannot be valid is the one reported whatever the
/// state of the store: bounds, decoded values, conditions, the total size, and
/// no key named twice.
fn batch_checked(request: &Batch) -> Result<Vec<Checked<'_>>, String> {
    let mut checked = Vec::with_capacity(request.operations.len());
    let mut named: HashMap<(&str, &str), usize> = HashMap::with_capacity(request.operations.len());
    let mut value_bytes = 0usize;
    for (index, operation) in request.operations.iter().enumerate() {
        let (namespace, key, text, condition) = match operation {
            Operation::Put {
                namespace,
                key,
                value_base64,
                expected,
            } => (namespace, key, Some(value_base64), expected),
            Operation::Delete {
                namespace,
                key,
                expected,
            } => (namespace, key, None, expected),
        };
        address(&request.collection, namespace, Some(key))
            .map_err(|error| at("operations", index, error))?;
        let value = match text {
            Some(text) => {
                if text.len() > 1_396_388 {
                    return Err(at(
                        "operations",
                        index,
                        invalid("value exceeds 1047290 decoded bytes"),
                    ));
                }
                let value = decode_base64(text).map_err(|error| at("operations", index, error))?;
                if value.len() > MAX_KV_VALUE_BYTES {
                    return Err(at(
                        "operations",
                        index,
                        invalid("value exceeds 1047290 decoded bytes"),
                    ));
                }
                Some(value)
            }
            None => None,
        };
        value_bytes += value.as_ref().map_or(0, Vec::len);
        if value_bytes > MAX_KV_BATCH_VALUE_BYTES {
            return Err(at(
                "operations",
                index,
                invalid(format!(
                    "the values of a batch total at most {MAX_KV_BATCH_VALUE_BYTES} decoded bytes"
                )),
            ));
        }
        if let Some(first) = named.insert((namespace.as_str(), key.as_str()), index) {
            return Err(at(
                "operations",
                index,
                invalid(format!(
                    "names the same namespace and key as operations[{first}]; a batch names each key once"
                )),
            ));
        }
        let expected = condition
            .expected()
            .map_err(|error| at("operations", index, error))?;
        checked.push(Checked {
            namespace,
            key,
            value,
            expected,
        });
    }
    Ok(checked)
}

fn entry_json(entry: &KvEntry) -> Value {
    item_json(&entry.key, &entry.value, entry.version, entry.expires_at_ms)
}

/// One entry of a response: the key, the base64 value, and the decimal
/// version and deadline.
fn item_json(key: &str, value: &[u8], version: u64, expires_at_ms: Option<u64>) -> Value {
    #[cfg(test)]
    ENCODED_VALUE_BYTES.with(|bytes| bytes.set(bytes.get() + value.len()));
    json!({"key":key,"value_base64":encode_base64(value),
        "version":version.to_string(),"expires_at_ms":expires_at_ms.map(|n|n.to_string())})
}

/// The compact JSON size of `item_json`, found from the length of the value alone
/// so that a value is never encoded just to be measured: the item with an empty
/// value plus four base64 characters for every started group of three value
/// bytes.
fn item_size(key: &str, value_len: usize, version: u64, expires_at_ms: Option<u64>) -> usize {
    json!({"key":key,"value_base64":"","version":version.to_string(),
        "expires_at_ms":expires_at_ms.map(|n|n.to_string())})
    .to_string()
    .len()
        + value_len.div_ceil(3) * 4
}

fn page(db: &Database, request: &List) -> Result<Value, String> {
    let view = db.kv_view(&request.collection).map_err(failure)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("internal_error: {error}"))?
        .as_millis()
        .try_into()
        .map_err(|_| "internal_error: system clock exceeds u64 milliseconds")?;
    visit_page(
        request.after.as_deref(),
        request.limit as usize,
        |after, limit, visit| {
            view.visit_at(
                &request.namespace,
                &request.prefix,
                after,
                limit,
                now,
                |entry| {
                    visit(Lent {
                        key: entry.key,
                        value: entry.value,
                        version: entry.version,
                        expires_at_ms: entry.expires_at_ms,
                        has_next: entry.has_next,
                    })
                },
            )
            .map_err(failure)
        },
    )
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
    scan: impl FnOnce(
        Option<&str>,
        usize,
        &mut dyn FnMut(Lent<'_>) -> ControlFlow<()>,
    ) -> Result<(), String>,
) -> Result<Value, String> {
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

/// One get-many response of at most `PAGE_BYTES` of compact JSON: the items of
/// the longest prefix of the keys whose response, closed after it, fits. `scan`
/// calls its visitor with each key's position and entry, in order; the visitor
/// stops it at the first key whose item does not fit. A value is measured from
/// the borrow and only encoded when its item is added.
fn visit_many_page(
    total: usize,
    scan: impl FnOnce(&mut dyn FnMut(usize, Option<Lent<'_>>) -> ControlFlow<()>) -> Result<(), String>,
) -> Result<Value, String> {
    let mut items = Vec::new();
    let mut item_bytes = 0;
    let mut full = false;
    scan(&mut |index, entry| {
        let bytes = match &entry {
            Some(entry) => item_size(
                entry.key,
                entry.value.len(),
                entry.version,
                entry.expires_at_ms,
            ),
            None => "null".len(),
        };
        // As the last item of the response it carries the index of the key
        // that follows, when one does.
        let next = (index + 1 < total).then_some(index + 1);
        let envelope = json!({"items":[],"next_index":next}).to_string().len();
        if envelope + item_bytes + bytes + items.len() > PAGE_BYTES {
            full = true;
            return ControlFlow::Break(());
        }
        item_bytes += bytes;
        items.push(match entry {
            Some(entry) => item_json(entry.key, entry.value, entry.version, entry.expires_at_ms),
            None => Value::Null,
        });
        ControlFlow::Continue(())
    })?;
    if full && items.is_empty() {
        return Err("internal_error: one KV entry exceeds the page boundary".to_owned());
    }
    // The first key not read: this response ends where the next must begin.
    let next_index = full.then_some(items.len());
    let mut body = serde_json::Map::with_capacity(2);
    body.insert("items".to_owned(), Value::Array(items));
    body.insert(
        "next_index".to_owned(),
        next_index.map_or(Value::Null, |index| json!(index)),
    );
    Ok(Value::Object(body))
}

fn many_page(db: &Database, request: &GetMany) -> Result<Value, String> {
    let view = db.kv_view(&request.collection).map_err(failure)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("internal_error: {error}"))?
        .as_millis()
        .try_into()
        .map_err(|_| "internal_error: system clock exceeds u64 milliseconds")?;
    let keys: Vec<(&str, &str)> = request
        .keys
        .iter()
        .map(|key| (key.namespace.as_str(), key.key.as_str()))
        .collect();
    visit_many_page(keys.len(), |visit| {
        view.visit_many_at(&keys, now, |index, entry| {
            visit(
                index,
                entry.map(|entry| Lent {
                    key: entry.key,
                    value: entry.value,
                    version: entry.version,
                    expires_at_ms: entry.expires_at_ms,
                    has_next: entry.has_next,
                }),
            )
        })
        .map_err(failure)
    })
}

pub(crate) fn tool_definitions(read_only: bool) -> Vec<Value> {
    let decimal = json!({"type":"string","pattern":"^(0|[1-9][0-9]{0,19})$","x-maximumUint64":"18446744073709551615"});
    let expected = json!({"oneOf":[{"enum":["any","absent"]},
        {"type":"object","properties":{"revision":decimal},"required":["revision"],"additionalProperties":false}],"default":"any"});
    let mut tools = Vec::new();
    for (name, description) in [
        (GET, "Fetch a live namespaced KV entry."),
        (
            PUT,
            "Commit a KV value with an atomic expected-version check; synced only under full durability.",
        ),
        (
            DELETE,
            "Delete a live KV entry with an atomic expected-version check.",
        ),
        (
            LIST,
            "List live KV entries in UTF-8 byte order with exclusive pagination.",
        ),
    ] {
        let write = matches!(name, PUT | DELETE);
        if read_only && write {
            continue;
        }
        let mut properties = json!({
            "collection":{"type":"string","minLength":1,"description":"Required existing one-dimensional collection; never created implicitly."},
            "namespace":{"type":"string","minLength":1,"x-maxUtf8Bytes":255}
        });
        let object = properties.as_object_mut().expect("object");
        let mut required = vec!["collection", "namespace"];
        if name == LIST {
            object.insert(
                "prefix".into(),
                json!({"type":"string","default":"","x-maxUtf8Bytes":1024}),
            );
            object.insert(
                "after".into(),
                json!({"type":"string","minLength":1,"x-maxUtf8Bytes":1024}),
            );
            object.insert(
                "limit".into(),
                json!({"type":"integer","minimum":1,"maximum":10000,"default":100}),
            );
        } else {
            required.push("key");
            object.insert(
                "key".into(),
                json!({"type":"string","minLength":1,"x-maxUtf8Bytes":1024}),
            );
        }
        if write {
            object.insert("expected".into(), expected.clone());
        }
        if name == PUT {
            required.push("value_base64");
            object.insert("value_base64".into(), json!({"type":"string","contentEncoding":"base64","maxLength":1396388,"x-maxDecodedBytes":1047290}));
            object.insert(
                "expires_at_ms".into(),
                json!({"anyOf":[decimal,{"type":"null"}],"default":null}),
            );
        }
        tools.push(json!({"name":name,"description":description,
            "inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
            "annotations":{"readOnlyHint":!write,"destructiveHint":write,"idempotentHint":name!=PUT}}));
    }
    let collection = json!({"type":"string","minLength":1,"description":"Required existing one-dimensional collection; never created implicitly."});
    let namespace = json!({"type":"string","minLength":1,"x-maxUtf8Bytes":255});
    let key = json!({"type":"string","minLength":1,"x-maxUtf8Bytes":1024});
    tools.push(json!({"name":GET_MANY,
        "description":"Fetch up to 1000 live KV entries by key in one call, in the order given; a key that is absent or expired is null. A response that would pass 16 MiB stops early and next_index names the first key not read. Calls share no snapshot.",
        "inputSchema":{"type":"object","properties":{
            "collection":collection,
            "keys":{"type":"array","maxItems":MAX_KV_GET_MANY_KEYS,"items":{"type":"object",
                "properties":{"namespace":namespace,"key":key},
                "required":["namespace","key"],"additionalProperties":false}}},
            "required":["collection","keys"],"additionalProperties":false},
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true}}));
    if !read_only {
        tools.push(json!({"name":BATCH,
            "description":"Commit up to 1000 puts and deletes of one collection atomically, all or none, as one frame; synced only under full durability. Operations behave as if run one at a time in order; a key may be named once. A put has no expires_at_ms and clears the deadline of the entry it replaces; every put gets the same version.",
            "inputSchema":{"type":"object","properties":{
                "collection":collection,
                "operations":{"type":"array","maxItems":MAX_KV_BATCH_OPERATIONS,"items":{"oneOf":[
                    {"type":"object","properties":{"op":{"const":"put"},"namespace":namespace,"key":key,
                        "value_base64":{"type":"string","contentEncoding":"base64","maxLength":1396388,"x-maxDecodedBytes":1047290},
                        "expected":expected},
                        "required":["op","namespace","key","value_base64"],"additionalProperties":false},
                    {"type":"object","properties":{"op":{"const":"delete"},"namespace":namespace,"key":key,
                        "expected":expected},
                        "required":["op","namespace","key"],"additionalProperties":false}]}}},
                "required":["collection","operations"],"additionalProperties":false},
            "annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false}}));
    }
    tools
}

pub(crate) fn decode_base64(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(4) {
        return Err(invalid("value_base64 must use padded standard base64"));
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    for (index, chunk) in text.as_bytes().as_chunks::<4>().0.iter().enumerate() {
        let last = index + 1 == text.len() / 4;
        let digit = |byte: u8| -> Result<u8, String> {
            match byte {
                b'A'..=b'Z' => Ok(byte - b'A'),
                b'a'..=b'z' => Ok(byte - b'a' + 26),
                b'0'..=b'9' => Ok(byte - b'0' + 52),
                b'+' => Ok(62),
                b'/' => Ok(63),
                _ => Err(invalid("invalid base64 value")),
            }
        };
        let a = digit(chunk[0])?;
        let b = digit(chunk[1])?;
        let c = (chunk[2] != b'=').then(|| digit(chunk[2])).transpose()?;
        let d = (chunk[3] != b'=').then(|| digit(chunk[3])).transpose()?;
        if (!last && (c.is_none() || d.is_none())) || (c.is_none() && d.is_some()) {
            return Err(invalid("invalid base64 padding"));
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
        return Err(invalid("value_base64 must be canonical padded base64"));
    }
    Ok(out)
}

pub(crate) fn encode_base64(bytes: &[u8]) -> String {
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

// MCP first decodes into Value, which otherwise silently discards duplicate keys.
pub(crate) fn unique_json(text: &str) -> Result<(), String> {
    struct Unique;
    impl<'de> Deserialize<'de> for Unique {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserializer.deserialize_any(Unique)
        }
    }
    impl<'de> de::Visitor<'de> for Unique {
        type Value = Unique;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("JSON without duplicate keys")
        }
        fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut keys = std::collections::BTreeSet::new();
            while let Some((key, _)) = map.next_entry::<String, Unique>()? {
                if !keys.insert(key) {
                    return Err(de::Error::custom("duplicate argument field"));
                }
            }
            Ok(Unique)
        }
        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Unique, A::Error> {
            while seq.next_element::<Unique>()?.is_some() {}
            Ok(Unique)
        }
        fn visit_str<E: de::Error>(self, _: &str) -> Result<Unique, E> {
            Ok(Unique)
        }
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Unique, E> {
            Ok(Unique)
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<Unique, E> {
            Ok(Unique)
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<Unique, E> {
            Ok(Unique)
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<Unique, E> {
            Ok(Unique)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Unique, E> {
            Ok(Unique)
        }
    }
    let _: Unique = parse(text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn whole_fetch_model(matching: &[KvEntry], limit: usize) -> Value {
        let fetched = &matching[..matching.len().min((limit + 1).min(10_000))];
        let beyond = fetched.len() == 10_000 && matching.len() > fetched.len();
        let mut items = Vec::new();
        let mut item_bytes = 0;
        for (index, entry) in fetched.iter().take(limit).enumerate() {
            // Generated values contain only zero bytes: RFC 4648 gives these
            // groups directly, without invoking the production codec.
            let mut encoded = "AAAA".repeat(entry.value.len() / 3);
            encoded.push_str(match entry.value.len() % 3 {
                0 => "",
                1 => "AA==",
                _ => "AAA=",
            });
            let item = json!({
                "key":entry.key,
                "value_base64":encoded,
                "version":entry.version.to_string(),
                "expires_at_ms":entry.expires_at_ms.map(|value| value.to_string())
            });
            let next = if index + 1 < fetched.len() || beyond {
                Some(entry.key.as_str())
            } else {
                None
            };
            let bytes = item.to_string().len();
            let envelope = json!({"items":[],"next_after":next}).to_string().len();
            if envelope + item_bytes + bytes + items.len() > 16_777_216 {
                break;
            }
            item_bytes += bytes;
            items.push(item);
        }
        let next = if items.len() < fetched.len() || beyond {
            items.last().map(|item| item["key"].clone())
        } else {
            None
        };
        json!({"items":items,"next_after":next})
    }

    #[test]
    fn pages_match_whole_fetch_model() {
        for (count, large) in [
            (0, false),
            (1, false),
            (7, false),
            (8, false),
            (9, false),
            (10_000, false),
            (10_001, false),
            (20, true),
        ] {
            let entries: Vec<_> = (0..count)
                .map(|index| KvEntry {
                    key: format!(
                        "{}{index:05}\"\\\u{e9}",
                        if index % 3 == 0 { "q/" } else { "p/" }
                    ),
                    value: vec![
                        0;
                        if large {
                            [1_047_290, 1_047_289, 900_001, 14_763][index % 4]
                        } else {
                            [0, 1, 2, 3, 7, 8, 9][index % 7]
                        }
                    ],
                    version: if index % 2 == 0 { 1 } else { u64::MAX },
                    expires_at_ms: (index % 2 == 0).then_some(u64::MAX),
                })
                .collect();
            let mut entries = entries;
            entries.sort_by(|a, b| a.key.cmp(&b.key));
            for (prefix, after) in [
                ("", None),
                ("p/", None),
                ("p/", Some("p/00003")),
                ("p/", Some("q/")),
                ("missing", None),
                ("", Some("z")),
            ] {
                let matching: Vec<_> = entries
                    .iter()
                    .filter(|entry| {
                        entry.key.starts_with(prefix)
                            && after.is_none_or(|after| entry.key.as_str() > after)
                    })
                    .cloned()
                    .collect();
                for limit in [1, 7, 8, 9, 100, 9_999, 10_000] {
                    let expected = whole_fetch_model(&matching, limit);
                    let lent = std::cell::Cell::new(0);
                    let actual =
                        visit_page(after, limit, scan_over(&entries, prefix, &lent)).unwrap();
                    assert_eq!(
                        actual.to_string(),
                        expected.to_string(),
                        "count={count}, large={large}, prefix={prefix:?}, after={after:?}, limit={limit}"
                    );
                }
            }
        }
    }

    /// A scan shaped like the `scan` argument of `visit_page`.
    type Scan<'a> = Box<
        dyn FnOnce(
                Option<&str>,
                usize,
                &mut dyn FnMut(Lent<'_>) -> ControlFlow<()>,
            ) -> Result<(), String>
            + 'a,
    >;

    /// `KvView::visit_at` as documented, over `entries` sorted by key: the
    /// entries that start with `prefix` and follow `after`, at most `count`,
    /// each with `has_next` set when another matching entry follows within
    /// `count + 1`. `lent` counts the entries handed to the visitor.
    fn scan_over<'a>(
        entries: &'a [KvEntry],
        prefix: &'a str,
        lent: &'a std::cell::Cell<usize>,
    ) -> Scan<'a> {
        Box::new(move |after, count, visit| {
            let matching: Vec<&KvEntry> = entries
                .iter()
                .filter(|entry| {
                    entry.key.starts_with(prefix)
                        && after.is_none_or(|after| entry.key.as_str() > after)
                })
                .collect();
            let total = matching.len().min(count + 1);
            for (index, entry) in matching.iter().take(count).enumerate() {
                lent.set(lent.get() + 1);
                let borrowed = Lent {
                    key: &entry.key,
                    value: &entry.value,
                    version: entry.version,
                    expires_at_ms: entry.expires_at_ms,
                    has_next: index + 1 < total,
                };
                if visit(borrowed).is_break() {
                    break;
                }
            }
            Ok(())
        })
    }

    // 16 MiB of response holds 12 values of the largest size. Reading is
    // bounded by the page: the entry that does not fit is measured, never
    // encoded, and nothing beyond it is touched.
    #[test]
    fn a_page_reads_only_what_it_emits() {
        let entries: Vec<_> = (0..20)
            .map(|index| KvEntry {
                key: format!("{index:05}"),
                value: vec![0; 1_047_290],
                version: 1,
                expires_at_ms: None,
            })
            .collect();
        let lent = std::cell::Cell::new(0);
        ENCODED_VALUE_BYTES.with(|bytes| bytes.set(0));
        let actual = visit_page(None, 10_000, scan_over(&entries, "", &lent)).unwrap();
        let emitted = actual["items"].as_array().unwrap().len();
        assert_eq!(emitted, 12);
        assert_eq!(actual["next_after"], "00011");
        assert_eq!(
            ENCODED_VALUE_BYTES.with(std::cell::Cell::get),
            emitted * 1_047_290,
            "only emitted values are encoded"
        );
        assert_eq!(
            lent.get(),
            emitted + 1,
            "emitted entries plus the one refused"
        );
        assert!(actual.to_string().len() <= 16_777_216);
    }

    #[test]
    fn a_failed_scan_returns_its_error_and_no_page() {
        let result = visit_page(None, 10_000, |_, _, _| {
            Err("io_error: injected list failure".into())
        });
        assert_eq!(result.unwrap_err(), "io_error: injected list failure");
    }

    #[test]
    fn a_first_entry_larger_than_a_page_ends_the_page_empty() {
        // No stored entry is this large (the largest item is about 1.4 MB), so
        // this pins the documented result rather than a reachable one: an empty
        // page without a cursor, and the oversized value measured, never encoded.
        let entries = [KvEntry {
            key: "k".into(),
            value: vec![0; 13_000_000],
            version: 1,
            expires_at_ms: None,
        }];
        let lent = std::cell::Cell::new(0);
        ENCODED_VALUE_BYTES.with(|bytes| bytes.set(0));
        let page = visit_page(None, 10, scan_over(&entries, "", &lent)).unwrap();
        assert_eq!(page, json!({"items":[],"next_after":null}));
        assert_eq!(lent.get(), 1);
        assert_eq!(ENCODED_VALUE_BYTES.with(std::cell::Cell::get), 0);
    }

    #[test]
    fn item_size_is_the_compact_size_of_the_item() {
        let keys = ["k", "a/b", "\"quoted\\\u{1}\n", "\u{e9}\u{4e2d}\u{1f600}"];
        let versions = [0, 9, 10, u64::MAX];
        let expiries = [None, Some(0), Some(u64::MAX)];
        for (round, key) in keys.iter().enumerate() {
            for length in (0..=40usize).chain([1_000, 1_047_289, 1_047_290]) {
                let value: Vec<u8> = (0..length).map(|index| (index * 7 + round) as u8).collect();
                for (&version, &expires_at_ms) in versions.iter().zip(expiries.iter().cycle()) {
                    assert_eq!(
                        item_size(key, length, version, expires_at_ms),
                        item_json(key, &value, version, expires_at_ms)
                            .to_string()
                            .len(),
                        "key {key:?}, {length} value bytes, version {version}, expiry {expires_at_ms:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn canonical_decimals_and_base64() {
        for text in ["0", "9007199254740993", "18446744073709551615"] {
            assert!(decimal(text).is_ok(), "{text}");
        }
        for text in ["", "00", "+1", "-1", " 1", "1.0", "18446744073709551616"] {
            assert!(decimal(text).unwrap_err().starts_with("invalid_input: "));
        }
        for (text, bytes) in [
            ("", &b""[..]),
            ("AA==", &[0][..]),
            ("AAE=", &[0, 1][..]),
            ("AAEC", &[0, 1, 2][..]),
            ("+w==", &[251][..]),
            ("/w==", &[255][..]),
            ("+/8=", &[251, 255][..]),
            ("AA+/", &[0, 15, 191][..]),
        ] {
            assert_eq!(decode_base64(text).unwrap(), bytes);
            assert_eq!(encode_base64(bytes), text);
        }
        for text in ["AB==", "AAF=", "AA", "AA==AAAA", "AA=A", "AA-_", " AA="] {
            assert!(decode_base64(text).is_err(), "{text}");
        }
    }

    #[test]
    fn argument_types_unknown_duplicate_and_utf8_bounds() {
        assert!(parse::<Put>(r#"{"collection":"c","namespace":"n","key":"k","value_base64":"","expires_at_ms":0}"#).is_err());
        assert!(
            parse::<Delete>(
                r#"{"collection":"c","namespace":"n","key":"k","expected":{"revision":0}}"#
            )
            .is_err()
        );
        assert!(parse::<Get>(r#"{"collection":"c","namespace":"n","key":"k","extra":0}"#).is_err());
        assert!(
            parse::<Put>(
                r#"{"collection":"c","namespace":"n","key":"k","value_base64":"","extra":0}"#
            )
            .is_err()
        );
        assert!(parse::<List>(r#"{"collection":"c","namespace":"n","extra":0}"#).is_err());
        for expected in ["Absent", "none", "ANY", ""] {
            let put = json!({"collection":"c","namespace":"n","key":"k","value_base64":"","expected":expected});
            let delete = json!({"collection":"c","namespace":"n","key":"k","expected":expected});
            assert!(parse::<Put>(&put.to_string()).is_err(), "{expected:?}");
            assert!(
                parse::<Delete>(&delete.to_string()).is_err(),
                "{expected:?}"
            );
        }
        assert!(
            parse::<Get>(r#"{"collection":"c","namespace":"n","key":"k","key":"other"}"#).is_err()
        );
        for text in [
            r#"{"a":1,"a":2}"#,
            r#"{"a":{"revision":"0","revision":"1"}}"#,
            r#"[{"a":null,"a":false}]"#,
        ] {
            assert!(unique_json(text).is_err(), "{text}");
        }
        assert!(unique_json(r#"{"a":[true,null,0,-1,1.5,"str",{}]}"#).is_ok());
        assert!(bounded(&"\u{e9}".repeat(127), "namespace", 1, 255).is_ok());
        assert!(bounded(&"\u{e9}".repeat(128), "namespace", 1, 255).is_err());
        assert!(bounded(&"\u{1f600}".repeat(256), "key", 1, 1024).is_ok());
        assert!(bounded(&"\u{1f600}".repeat(257), "key", 1, 1024).is_err());
        assert!(parse::<List>(r#"{"collection":"c","namespace":"n","after":null}"#).is_err());
        assert!(
            parse::<Delete>(r#"{"collection":"c","namespace":"n","key":"k","expected":null}"#)
                .is_err()
        );
        assert!(parse::<Delete>(r#"{"collection":"c","namespace":"n","key":"k","expected":{"revision":"1","extra":true}}"#).is_err());
        assert!(
            parse::<Delete>(
                r#"{"collection":"c","namespace":"n","key":"k","expected":{"any":null}}"#
            )
            .is_err()
        );
        assert!(
            parse::<Delete>(
                r#"{"collection":"c","namespace":"n","key":"k","expected":{"absent":null}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn commit_indeterminate_keeps_its_distinct_error_code() {
        let error = failure(Error::CommitIndeterminate(std::io::Error::other(
            "injected sync failure",
        )));
        assert!(error.starts_with("commit_indeterminate: "), "{error}");
        let error = failure(Error::Io(std::io::Error::other("injected read failure")));
        assert!(error.starts_with("io_error: "), "{error}");
    }

    #[test]
    fn page_exact_encoded_boundary() {
        let page = |entries: &[KvEntry], limit: usize| {
            let lent = std::cell::Cell::new(0);
            visit_page(None, limit, scan_over(entries, "", &lent)).unwrap()
        };
        // Each entry has 63 fixed JSON bytes plus its key and encoded value.
        // Empty-page envelope is 30 bytes with null, 29 with a one-byte cursor.
        let mut entries: Vec<_> = (0..13)
            .map(|index| KvEntry {
                key: char::from(b'a' + index).to_string(),
                value: vec![0; 1_047_290],
                version: 1,
                expires_at_ms: None,
            })
            .collect();
        let full = page(&entries, 13);
        assert_eq!(full["items"].as_array().unwrap().len(), 12);
        assert_eq!(full["next_after"], "l");
        // A three-byte final key makes the remaining base64 budget divisible by 4:
        // 30 + 12*(64+1396388) + 12 commas + 66 + 19684 = 16777216.
        entries[12].key = "mmm".into();
        entries[12].value = vec![0; 14_763];
        let exact = page(&entries, 13);
        assert_eq!(exact["items"].as_array().unwrap().len(), 13);
        assert_eq!(exact.to_string().len(), 16_777_216);
        assert_eq!(exact["next_after"], Value::Null);
        // A one-byte key extension crosses the cap by exactly one byte;
        // adding one decoded byte instead grows base64 by four bytes.
        entries[12].key.push('m');
        let over = page(&entries, 13);
        assert_eq!(over["items"].as_array().unwrap().len(), 12);
        assert_eq!(over["next_after"], "l");
        entries[12].key.pop();

        // A deadline is part of the item: it no longer fits by the bytes it adds.
        entries[12].expires_at_ms = Some(u64::MAX);
        let timed = page(&entries, 13);
        assert_eq!(timed["items"].as_array().unwrap().len(), 12);
        assert_eq!(timed["next_after"], "l");
        entries[12].expires_at_ms = None;
        // A further entry after the thirteenth makes it carry a cursor: the
        // three-byte cursor replaces null (4 bytes) with "mmm" (5 bytes).
        let mut beyond_entries = entries.clone();
        beyond_entries.push(KvEntry {
            key: "n".into(),
            value: Vec::new(),
            version: 1,
            expires_at_ms: None,
        });
        let beyond = page(&beyond_entries, 13);
        assert_eq!(beyond["items"].as_array().unwrap().len(), 12);
        assert_eq!(beyond["next_after"], "l");

        entries[12].value.push(0);
        let over = page(&entries, 13);
        assert_eq!(over["items"].as_array().unwrap().len(), 12);
        assert_eq!(over["next_after"], "l");
        assert_eq!(page(&[], 100), json!({"items":[],"next_after":null}));
    }

    /// A scan shaped like the `scan` argument of `visit_many_page`.
    type ManyScan<'a> = Box<
        dyn FnOnce(&mut dyn FnMut(usize, Option<Lent<'_>>) -> ControlFlow<()>) -> Result<(), String>
            + 'a,
    >;

    /// `KvView::visit_many_at` as documented: each requested key in order with
    /// its entry, or `None`, and `has_next` set while a later key follows.
    /// `lent` counts the keys handed to the visitor.
    fn scan_many<'a>(
        entries: &'a [Option<KvEntry>],
        lent: &'a std::cell::Cell<usize>,
    ) -> ManyScan<'a> {
        Box::new(move |visit| {
            for (index, entry) in entries.iter().enumerate() {
                lent.set(lent.get() + 1);
                let borrowed = entry.as_ref().map(|entry| Lent {
                    key: &entry.key,
                    value: &entry.value,
                    version: entry.version,
                    expires_at_ms: entry.expires_at_ms,
                    has_next: index + 1 < entries.len(),
                });
                if visit(index, borrowed).is_break() {
                    break;
                }
            }
            Ok(())
        })
    }

    fn many_of(entries: &[Option<KvEntry>]) -> Value {
        let lent = std::cell::Cell::new(0);
        visit_many_page(entries.len(), scan_many(entries, &lent)).unwrap()
    }

    /// The longest prefix of the items whose whole response, closed with the
    /// index of the key that follows, fits: each item by its specified JSON
    /// and each response measured whole. Generated values are zero bytes.
    fn whole_many_model(entries: &[Option<KvEntry>]) -> Value {
        let items: Vec<Value> = entries
            .iter()
            .map(|entry| match entry {
                None => Value::Null,
                Some(entry) => {
                    let mut encoded = "AAAA".repeat(entry.value.len() / 3);
                    encoded.push_str(match entry.value.len() % 3 {
                        0 => "",
                        1 => "AA==",
                        _ => "AAA=",
                    });
                    json!({
                        "key":entry.key,
                        "value_base64":encoded,
                        "version":entry.version.to_string(),
                        "expires_at_ms":entry.expires_at_ms.map(|value| value.to_string())
                    })
                }
            })
            .collect();
        let mut kept = 0;
        for count in 1..=items.len() {
            let next = (count < items.len()).then_some(count);
            let size = json!({"items":[],"next_index":next}).to_string().len()
                + items[..count]
                    .iter()
                    .map(|item| item.to_string().len())
                    .sum::<usize>()
                + (count - 1);
            if size > 16_777_216 {
                break;
            }
            kept = count;
        }
        json!({"items":&items[..kept],"next_index":(kept < items.len()).then_some(kept)})
    }

    #[test]
    fn many_pages_match_a_whole_model() {
        for (count, large) in [
            (0, false),
            (1, false),
            (7, false),
            (1_000, false),
            (30, true),
        ] {
            let entries: Vec<Option<KvEntry>> = (0..count)
                .map(|index| {
                    // Every fifth key is absent, and stays in the page as null.
                    (index % 5 != 2).then(|| KvEntry {
                        key: format!("k{index:05}\"\\\u{e9}"),
                        value: vec![
                            0;
                            if large {
                                [1_047_290, 1_047_289, 900_001, 14_763][index % 4]
                            } else {
                                [0, 1, 2, 3, 7, 8, 9][index % 7]
                            }
                        ],
                        version: if index % 2 == 0 { 1 } else { u64::MAX },
                        expires_at_ms: (index % 2 == 0).then_some(u64::MAX),
                    })
                })
                .collect();
            assert_eq!(
                many_of(&entries).to_string(),
                whole_many_model(&entries).to_string(),
                "count={count}, large={large}"
            );
        }
        // Nothing is read for no keys.
        assert_eq!(many_of(&[]), json!({"items":[],"next_index":null}));
    }

    // 16 MiB of response holds 12 values of the largest size. Reading is
    // bounded by the page: the key that does not fit is measured, never
    // encoded, and no key beyond it is touched.
    #[test]
    fn a_many_page_reads_only_what_it_emits() {
        let entries: Vec<Option<KvEntry>> = (0..20)
            .map(|index| {
                Some(KvEntry {
                    key: format!("{index:05}"),
                    value: vec![0; 1_047_290],
                    version: 1,
                    expires_at_ms: None,
                })
            })
            .collect();
        let lent = std::cell::Cell::new(0);
        ENCODED_VALUE_BYTES.with(|bytes| bytes.set(0));
        let actual = visit_many_page(entries.len(), scan_many(&entries, &lent)).unwrap();
        let emitted = actual["items"].as_array().unwrap().len();
        assert_eq!(emitted, 12);
        assert_eq!(actual["next_index"], 12);
        assert_eq!(
            ENCODED_VALUE_BYTES.with(std::cell::Cell::get),
            emitted * 1_047_290,
            "only emitted values are encoded"
        );
        assert_eq!(lent.get(), emitted + 1, "emitted keys plus the one refused");
        assert!(actual.to_string().len() <= 16_777_216);
    }

    #[test]
    fn a_failed_many_scan_returns_its_error_and_no_page() {
        let result = visit_many_page(3, |_| Err("io_error: injected get-many failure".into()));
        assert_eq!(result.unwrap_err(), "io_error: injected get-many failure");
    }

    #[test]
    fn a_first_key_larger_than_a_page_is_an_internal_error() {
        // No stored entry is this large; this pins the answer for the
        // unreachable case, as for a list page.
        let entries = [Some(KvEntry {
            key: "k".into(),
            value: vec![0; 13_000_000],
            version: 1,
            expires_at_ms: None,
        })];
        let lent = std::cell::Cell::new(0);
        ENCODED_VALUE_BYTES.with(|bytes| bytes.set(0));
        let error = visit_many_page(1, scan_many(&entries, &lent)).unwrap_err();
        assert!(error.starts_with("internal_error: "), "{error}");
        assert_eq!(ENCODED_VALUE_BYTES.with(std::cell::Cell::get), 0);
    }

    #[test]
    fn many_page_exact_encoded_boundary() {
        let fixed = item_json("", &[], 1, None).to_string().len();
        // The same boundary with and without an absent key among the entries
        // (it stays in the response as `null`, which has bytes of its own), and
        // with and without one more key after the entry at the boundary: that
        // entry then ends the response with the index of the key that follows,
        // which has fewer bytes than `null`.
        for (with_null, trailing) in [(false, false), (false, true), (true, false), (true, true)] {
            let mut entries: Vec<Option<KvEntry>> = (0..12)
                .map(|index| {
                    Some(KvEntry {
                        key: format!("{index:02}"),
                        value: vec![0; 1_047_290],
                        version: 1,
                        expires_at_ms: None,
                    })
                })
                .collect();
            if with_null {
                entries.insert(0, None);
            }
            let count = entries.len();
            let next = trailing.then_some(count + 1);
            let head = json!({
                "items":entries
                    .iter()
                    .map(|entry| entry.as_ref().map_or(Value::Null, entry_json))
                    .collect::<Vec<_>>(),
                "next_index":next
            })
            .to_string()
            .len();
            for boundary in [PAGE_BYTES - 1, PAGE_BYTES, PAGE_BYTES + 1] {
                // The last item brings the whole response to `boundary`: the
                // items before it and their envelope, a comma, then its own
                // item. Its key takes one to four bytes so that its base64 fills
                // groups of four.
                let need = boundary - head - 1 - fixed;
                let key = format!("m{}", "x".repeat((need - 1) % 4));
                let value = vec![0; (need - key.len()) / 4 * 3];
                entries.push(Some(KvEntry {
                    key,
                    value,
                    version: 1,
                    expires_at_ms: None,
                }));
                if trailing {
                    entries.push(None);
                }
                let response = many_of(&entries);
                entries.truncate(count);
                if boundary <= PAGE_BYTES {
                    assert_eq!(
                        response.to_string().len(),
                        boundary,
                        "{with_null} {trailing}"
                    );
                    assert_eq!(response["items"].as_array().unwrap().len(), count + 1);
                    assert_eq!(response["next_index"], json!(next));
                } else {
                    assert_eq!(response["items"].as_array().unwrap().len(), count);
                    assert_eq!(response["next_index"], count);
                }
            }
        }
    }

    fn memory() -> Database {
        let mut db = Database::create_in_memory(1, vlite::CreateOptions::default()).unwrap();
        db.kv_store("default").unwrap();
        db
    }

    fn put_text(namespace: &str, key: &str, value_base64: &str) -> Value {
        json!({"op":"put","namespace":namespace,"key":key,"value_base64":value_base64})
    }

    fn delete_text(namespace: &str, key: &str) -> Value {
        json!({"op":"delete","namespace":namespace,"key":key})
    }

    fn batch_text(operations: Vec<Value>) -> String {
        json!({"collection":"default","operations":operations}).to_string()
    }

    #[test]
    fn a_batch_is_checked_as_a_whole_before_the_store_is_touched() {
        let mut db = memory();
        let stale = json!({"op":"put","namespace":"n","key":"live","value_base64":"",
            "expected":{"revision":"999"}});
        // The operation that would fail its condition comes first; the request
        // is refused for the invalid one after it.
        for (case, bad, expected) in [
            (
                "an empty namespace",
                delete_text("", "k"),
                "invalid_input: operations[1]: ",
            ),
            (
                "an empty key",
                delete_text("n", ""),
                "invalid_input: operations[1]: ",
            ),
            (
                "base64 without padding",
                put_text("n", "k", "AA"),
                "invalid_input: operations[1]: ",
            ),
            (
                "a repeated key",
                put_text("n", "live", ""),
                "invalid_input: operations[1]: names the same namespace and key as operations[0]",
            ),
        ] {
            let error =
                run(&mut db, false, BATCH, &batch_text(vec![stale.clone(), bad])).unwrap_err();
            assert!(error.starts_with(expected), "{case}: {error}");
        }
        // An expectation that is neither any nor absent never parses, as for a
        // single call.
        let unknown = json!({"op":"delete","namespace":"n","key":"k","expected":"later"});
        let error = run(
            &mut db,
            false,
            BATCH,
            &batch_text(vec![stale.clone(), unknown]),
        )
        .unwrap_err();
        assert!(error.starts_with("invalid_request: "), "{error}");
        // The stale operation alone is a failed condition and writes nothing.
        let error = run(&mut db, false, BATCH, &batch_text(vec![stale.clone()])).unwrap_err();
        assert!(error.starts_with("precondition_failed: "), "{error}");
        assert!(
            db.kv_view("default")
                .unwrap()
                .list("n", "", None, 10)
                .unwrap()
                .is_empty()
        );

        let many: Vec<Value> = (0..=MAX_KV_BATCH_OPERATIONS)
            .map(|index| delete_text("n", &format!("k{index}")))
            .collect();
        let error = run(&mut db, false, BATCH, &batch_text(many)).unwrap_err();
        assert_eq!(
            error,
            format!("invalid_input: a batch takes at most {MAX_KV_BATCH_OPERATIONS} operations")
        );
        // Exactly that many are taken.
        let exactly: Vec<Value> = (0..MAX_KV_BATCH_OPERATIONS)
            .map(|index| delete_text("n", &format!("k{index}")))
            .collect();
        let done = run(&mut db, false, BATCH, &batch_text(exactly)).unwrap();
        assert_eq!(
            done["results"].as_array().unwrap().len(),
            MAX_KV_BATCH_OPERATIONS
        );
        // The values of a batch total at most 4,194,304 bytes.
        let part = encode_base64(&vec![1; 1_000_000]);
        let last = |extra: usize| encode_base64(&vec![1; 194_304 + extra]);
        let sized = |extra: usize| {
            let mut operations: Vec<Value> = (0..4)
                .map(|index| put_text("n", &format!("v{index}"), &part))
                .collect();
            operations.push(put_text("n", "v4", &last(extra)));
            batch_text(operations)
        };
        let error = run(&mut db, false, BATCH, &sized(1)).unwrap_err();
        assert!(
            error.starts_with("invalid_input: operations[4]: "),
            "{error}"
        );
        assert_eq!(
            run(&mut db, false, BATCH, &sized(0)).unwrap()["results"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
    }

    #[test]
    fn a_batch_has_no_deadline_and_a_closed_shape() {
        for text in [
            r#"{"collection":"c","operations":[{"op":"put","namespace":"n","key":"k","value_base64":"","expires_at_ms":"1"}]}"#,
            r#"{"collection":"c","operations":[{"op":"delete","namespace":"n","key":"k","value_base64":""}]}"#,
            r#"{"collection":"c","operations":[{"op":"swap","namespace":"n","key":"k"}]}"#,
            r#"{"collection":"c","operations":[{"namespace":"n","key":"k"}]}"#,
            r#"{"collection":"c","operations":[{"op":"put","namespace":"n","key":"k"}]}"#,
            r#"{"collection":"c","operations":[],"extra":0}"#,
            r#"{"collection":"c"}"#,
            r#"{"operations":[]}"#,
            r#"{"collection":"c","operations":[],"operations":[]}"#,
            r#"{"collection":"c","operations":[{"op":"delete","namespace":"n","key":"k","expected":null}]}"#,
        ] {
            assert!(parse::<Batch>(text).is_err(), "{text}");
        }
        assert!(
            parse::<Batch>(
                r#"{"collection":"c","operations":[{"op":"put","namespace":"n","key":"k","value_base64":"","expected":{"revision":"7"}}]}"#
            )
            .is_ok()
        );
        for text in [
            r#"{"collection":"c","keys":[{"namespace":"n","key":"k","extra":0}]}"#,
            r#"{"collection":"c","keys":[{"namespace":"n"}]}"#,
            r#"{"collection":"c","keys":[],"limit":1}"#,
            r#"{"keys":[]}"#,
            r#"{"collection":"c"}"#,
        ] {
            assert!(parse::<GetMany>(text).is_err(), "{text}");
        }
    }

    #[test]
    fn get_many_checks_its_keys_before_reading_and_the_gates_follow_the_tool() {
        let mut db = memory();
        let keys = |keys: Vec<Value>| json!({"collection":"default","keys":keys}).to_string();
        let key = |namespace: &str, key: &str| json!({"namespace":namespace,"key":key});
        let error = run(
            &mut db,
            false,
            GET_MANY,
            &keys(vec![key("n", "a"), key("n", "b"), key("", "c")]),
        )
        .unwrap_err();
        assert!(error.starts_with("invalid_input: keys[2]: "), "{error}");
        let too_many: Vec<Value> = (0..=MAX_KV_GET_MANY_KEYS)
            .map(|index| key("n", &format!("k{index}")))
            .collect();
        assert_eq!(
            run(&mut db, false, GET_MANY, &keys(too_many)).unwrap_err(),
            format!("invalid_input: a get-many takes at most {MAX_KV_GET_MANY_KEYS} keys")
        );
        // Exactly that many are taken.
        let exactly: Vec<Value> = (0..MAX_KV_GET_MANY_KEYS)
            .map(|index| key("n", &format!("k{index}")))
            .collect();
        let page = run(&mut db, false, GET_MANY, &keys(exactly)).unwrap();
        assert_eq!(
            page["items"].as_array().unwrap().len(),
            MAX_KV_GET_MANY_KEYS
        );
        assert_eq!(
            run(&mut db, false, GET_MANY, &keys(vec![])).unwrap(),
            json!({"items":[],"next_index":null})
        );
        // Reading is allowed on a read-only database; writing a batch is not.
        assert!(run(&mut db, true, GET_MANY, &keys(vec![key("n", "a")])).is_ok());
        let error = run(&mut db, true, BATCH, &batch_text(vec![])).unwrap_err();
        assert!(error.starts_with("read_only: "), "{error}");
        // Neither tool creates a collection.
        let missing = json!({"collection":"missing","keys":[]}).to_string();
        let error = run(&mut db, false, GET_MANY, &missing).unwrap_err();
        assert!(error.starts_with("collection_not_found: "), "{error}");
        let missing = json!({"collection":"missing","operations":[]}).to_string();
        let error = run(&mut db, false, BATCH, &missing).unwrap_err();
        assert!(error.starts_with("collection_not_found: "), "{error}");
        assert!(db.collection("missing").is_err());
    }
}
