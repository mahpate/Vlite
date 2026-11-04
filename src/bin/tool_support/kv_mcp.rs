//! Local KV requests shared by MCP and the shell.

use serde::{Deserialize, Deserializer, de};
use serde_json::{Value, json};
use vlite::{
    Database, Error, Expected,
    kv::{KvEntry, KvPut, MAX_KV_VALUE_BYTES},
};

const GET: &str = "vlite_kv_get";
const PUT: &str = "vlite_kv_put";
const DELETE: &str = "vlite_kv_delete";
const LIST: &str = "vlite_kv_list";
const PAGE_BYTES: usize = 16 * 1024 * 1024;

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
    matches!(name, GET | PUT | DELETE | LIST)
}

#[allow(dead_code)]
pub(crate) fn shell_tool(command: &str) -> Option<&'static str> {
    match command {
        "kv-get" => Some(GET),
        "kv-put" => Some(PUT),
        "kv-delete" => Some(DELETE),
        "kv-list" => Some(LIST),
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
    if read_only && matches!(name, PUT | DELETE) {
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
        _ => Err(format!("invalid_request: unknown KV tool {name}")),
    }
}

fn entry_json(entry: &KvEntry) -> Value {
    json!({"key":entry.key,"value_base64":encode_base64(&entry.value),
        "version":entry.version.to_string(),"expires_at_ms":entry.expires_at_ms.map(|n|n.to_string())})
}

fn page(db: &Database, request: &List) -> Result<Value, String> {
    let view = db.kv_view(&request.collection).map_err(failure)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("internal_error: {error}"))?
        .as_millis()
        .try_into()
        .map_err(|_| "internal_error: system clock exceeds u64 milliseconds")?;
    batched_page(
        request.after.as_deref(),
        request.limit as usize,
        |after, limit| {
            view.list_at(&request.namespace, &request.prefix, after, limit, now)
                .map_err(failure)
        },
    )
}

fn batched_page(
    after: Option<&str>,
    limit: usize,
    mut fetch: impl FnMut(Option<&str>, usize) -> Result<Vec<KvEntry>, String>,
) -> Result<Value, String> {
    let mut entries = Vec::<KvEntry>::new();
    let mut encoded_bytes = 0;
    // An empty cursor is a lower bound on every possible page envelope.
    let envelope = json!({"items":[],"next_after":""}).to_string().len();
    let target = (limit + 1).min(10_000);
    while entries.len() < target {
        let after = entries.last().map(|entry| entry.key.as_str()).or(after);
        let batch_limit = (target - entries.len()).min(8);
        let batch = fetch(after, batch_limit)?;
        let exhausted = batch.len() < batch_limit;
        for entry in batch {
            encoded_bytes +=
                entry_json(&entry).to_string().len() + usize::from(!entries.is_empty());
            entries.push(entry);
            if envelope + encoded_bytes > PAGE_BYTES {
                // This entry cannot fit even without a cursor, and proves continuation.
                return pack_page(&entries, limit, false);
            }
        }
        if exhausted {
            break;
        }
    }
    let beyond = if entries.len() == 10_000 {
        !fetch(entries.last().map(|entry| entry.key.as_str()), 1)?.is_empty()
    } else {
        false
    };
    pack_page(&entries, limit, beyond)
}

fn pack_page(entries: &[KvEntry], limit: usize, beyond: bool) -> Result<Value, String> {
    let mut items = Vec::new();
    let mut encoded_bytes = 0;
    for (index, entry) in entries.iter().take(limit).enumerate() {
        let item = entry_json(entry);
        let item_bytes = item.to_string().len();
        let more = index + 1 < entries.len() || beyond;
        let next = more.then_some(entry.key.as_str());
        let envelope = json!({"items":[],"next_after":next}).to_string().len();
        let size = envelope + encoded_bytes + item_bytes + items.len();
        if size > PAGE_BYTES {
            break;
        }
        encoded_bytes += item_bytes;
        items.push(item);
    }
    let more = items.len() < entries.len() || beyond;
    let next = if more {
        items.last().and_then(|item| item["key"].as_str())
    } else {
        None
    };
    Ok(json!({"items":items,"next_after":next}))
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
    fn batched_pages_match_whole_fetch_model() {
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
                    let actual = batched_page(after, limit, |after, batch_limit| {
                        assert!(batch_limit <= 8);
                        Ok(matching
                            .iter()
                            .filter(|entry| after.is_none_or(|after| entry.key.as_str() > after))
                            .take(batch_limit)
                            .cloned()
                            .collect())
                    })
                    .unwrap();
                    assert_eq!(
                        actual.to_string(),
                        expected.to_string(),
                        "count={count}, large={large}, prefix={prefix:?}, after={after:?}, limit={limit}"
                    );
                }
            }
        }
    }

    #[test]
    fn batched_page_stops_cloning_at_the_byte_cut() {
        let mut fetched = 0;
        let actual = batched_page(None, 10_000, |after, limit| {
            assert!(limit <= 8);
            let start = after.map_or(0, |key| key.parse::<usize>().unwrap() + 1);
            fetched += limit;
            Ok((start..start + limit)
                .map(|index| KvEntry {
                    key: format!("{index:05}"),
                    value: vec![0; 1_047_290],
                    version: 1,
                    expires_at_ms: None,
                })
                .collect())
        })
        .unwrap();
        assert_eq!(fetched, 16);
        assert_eq!(actual["items"].as_array().unwrap().len(), 12);
        assert_eq!(actual["next_after"], "00011");
        assert!(actual.to_string().len() <= 16_777_216);
    }

    #[test]
    fn batched_page_propagates_fetch_and_peek_errors() {
        for fail_at in [0, 8, 10_000] {
            let mut fetched = 0;
            let result = batched_page(None, 10_000, |_, limit| {
                if fetched == fail_at {
                    return Err("io_error: injected list failure".into());
                }
                let entries = (fetched..fetched + limit)
                    .map(|index| KvEntry {
                        key: format!("{index:05}"),
                        value: Vec::new(),
                        version: 1,
                        expires_at_ms: None,
                    })
                    .collect();
                fetched += limit;
                Ok(entries)
            });
            assert_eq!(result.unwrap_err(), "io_error: injected list failure");
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
        let full = pack_page(&entries, 13, false).unwrap();
        assert_eq!(full["items"].as_array().unwrap().len(), 12);
        assert_eq!(full["next_after"], "l");
        // A three-byte final key makes the remaining base64 budget divisible by 4:
        // 30 + 12*(64+1396388) + 12 commas + 66 + 19684 = 16777216.
        entries[12].key = "mmm".into();
        entries[12].value = vec![0; 14_763];
        let exact = pack_page(&entries, 13, false).unwrap();
        assert_eq!(exact["items"].as_array().unwrap().len(), 13);
        assert_eq!(exact.to_string().len(), 16_777_216);
        assert_eq!(exact["next_after"], Value::Null);
        // A one-byte key extension crosses the cap by exactly one byte;
        // adding one decoded byte instead grows base64 by four bytes.
        entries[12].key.push('m');
        let over = pack_page(&entries, 13, false).unwrap();
        assert_eq!(over["items"].as_array().unwrap().len(), 12);
        assert_eq!(over["next_after"], "l");
        entries[12].key.pop();

        // The three-byte cursor replaces null (4 bytes) with "mmm" (5 bytes).
        let beyond = pack_page(&entries, 13, true).unwrap();
        assert_eq!(beyond["items"].as_array().unwrap().len(), 12);
        assert_eq!(beyond["next_after"], "l");

        entries[12].value.push(0);
        let over = pack_page(&entries, 13, false).unwrap();
        assert_eq!(over["items"].as_array().unwrap().len(), 12);
        assert_eq!(over["next_after"], "l");
        assert_eq!(
            pack_page(&[], 100, false).unwrap(),
            json!({"items":[],"next_after":null})
        );
    }
}
