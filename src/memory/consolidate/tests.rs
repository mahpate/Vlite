//! Exact replay of the accepted C1 golden
//! (`tests/fixtures/agent_memory/c1_consolidation_golden.json`) against the
//! private Rust implementation. Every expectation comes from the golden; the
//! harness only ports the oracle's literal descriptor expansion, case catalog,
//! one-leaf repairs and second-defect injections.
//!
//! Fixture-to-engine mappings (test-only):
//! * Setup sources, records, owners, raw receipt corruption, `forget` and
//!   `reopen` are raw graph writes or database operations.
//! * The engine commit clock is max(graph high-water, wall, C). Cases whose
//!   clocks interact with expiry (source/summary TTL, wall/high-water,
//!   visibility) run at a fixed offset `off = now + 1h`, so the real wall
//!   clock never dominates, and fixture wall/high-water become the graph
//!   high-water. The receipt then stores the shifted `expires_at_ms`; its
//!   decimal-digit growth is the only expectation adjustment, applied to the
//!   receipt entry and frame byte counts.
//! * `fixture:EndpointRefused` is the engine's preappend endpoint refusal,
//!   surfaced as `Storage(GraphNodeNotFound)`.
//! * A record in a foreign scope is a record attached to a VliteMemory owner
//!   in that scope; a named non-memory owner is a `FixtureRecordOwner` node.
//! * Receipt revisions and sequences are ordinals of this case's C1 commits,
//!   mapped from real commit sequences (harness writes are excluded).

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use serde_json::{Map, Value, json};

use super::super::{
    LABEL_MEMORY, LABEL_RECEIPT, LABEL_RECEIPT_OUTCOME, LABEL_SCOPE, MemoryError, MemoryEvidence,
    MemoryMut, MemoryProperties, MemoryScope, Provenance, content_digest, decode_sources,
    evidence_record_id, hex, memory_key, memory_properties, operation_receipt_key,
    receipt_outcome_key,
};
use super::{
    ConsolidateInput, ConsolidateRequest, ConsolidateSummary, canonical_decimal, decode_outcome,
    digest, normalize,
};
use crate::{
    Condition, Database, Expected, Metadata, MetadataValue,
    property_graph::{
        Direction, EdgeRef, FindNodes, GraphLimits, GraphOp, GraphRef, Neighbors, NodeSelector,
        NodeView, ReadOptions,
        engine::{GraphBody, GraphEntry, PropertyValue, decode_body, encode_body},
    },
};

const GOLDEN: &str =
    include_str!("../../../tests/fixtures/agent_memory/c1_consolidation_golden.json");
const GRAPH: &str = "memory";
const OWNER_LABEL: &str = "FixtureRecordOwner";
const I64_MAX: u64 = i64::MAX as u64;

fn golden() -> Value {
    let fixture: Value = serde_json::from_str(GOLDEN).unwrap();
    assert_eq!(fixture["schema"], "fixture.c1-consolidation.v1");
    fixture
}

// ---------------------------------------------------------------------------
// Literal fixture descriptors (ported from the oracle; inputs only).

fn expand(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(expand).collect()),
        Value::Object(map) => {
            let keys: BTreeSet<&str> = map.keys().map(String::as_str).collect();
            if keys == BTreeSet::from(["$text", "bytes"]) {
                let pattern = map["$text"].as_str().unwrap().as_bytes();
                let length = map["bytes"].as_u64().unwrap() as usize;
                assert!(!pattern.is_empty() && length <= 16_777_216 + 1024);
                let raw: Vec<u8> = pattern.iter().copied().cycle().take(length).collect();
                return Value::String(String::from_utf8(raw).unwrap());
            }
            if keys.contains("$repeat") {
                assert!(keys.is_subset(&BTreeSet::from(["$repeat", "count", "enumerate"])));
                let count = map["count"].as_u64().unwrap();
                assert!(count <= 5000);
                let enumerate: Vec<&str> = map
                    .get("enumerate")
                    .map(|keys| keys.as_array().unwrap().as_slice())
                    .unwrap_or_default()
                    .iter()
                    .map(|key| key.as_str().unwrap())
                    .collect();
                return Value::Array(
                    (0..count)
                        .map(|index| {
                            let mut item = expand(&map["$repeat"]);
                            for key in &enumerate {
                                let text = format!("{}{index}", item[*key].as_str().unwrap());
                                item[*key] = Value::String(text);
                            }
                            item
                        })
                        .collect(),
                );
            }
            if keys == BTreeSet::from(["$number"]) {
                // JSON cannot carry NaN/inf; `number` converts the descriptor.
                return value.clone();
            }
            if keys == BTreeSet::from(["$ordered", "reverse"]) {
                let Value::Array(mut items) = expand(&map["$ordered"]) else {
                    panic!("$ordered requires a list")
                };
                if map["reverse"].as_bool().unwrap() {
                    items.reverse();
                }
                return Value::Array(items);
            }
            Value::Object(
                map.iter()
                    .map(|(key, item)| (key.clone(), expand(item)))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

fn merge(base: &Value, update: &Value) -> Value {
    let mut result = base.clone();
    for (key, value) in update.as_object().unwrap() {
        let recurse = value.is_object()
            && result.get(key).is_some_and(Value::is_object)
            && !value
                .as_object()
                .unwrap()
                .keys()
                .any(|name| name.starts_with('$'));
        let merged = if recurse {
            merge(&result[key], value)
        } else {
            value.clone()
        };
        result.as_object_mut().unwrap().insert(key.clone(), merged);
    }
    result
}

fn lookup<'a>(mut value: &'a Value, path: &[Value]) -> Option<&'a Value> {
    for key in path {
        value = match key {
            Value::String(name) => value.as_object()?.get(name)?,
            Value::Number(index) => value.as_array()?.get(index.as_u64()? as usize)?,
            _ => return None,
        };
    }
    Some(value)
}

fn edit(value: &mut Value, change: &Value) {
    let path = change["path"].as_array().unwrap();
    assert!(!path.is_empty());
    let mut parent = value;
    for key in &path[..path.len() - 1] {
        parent = match key {
            Value::String(name) => parent.as_object_mut().unwrap().get_mut(name).unwrap(),
            Value::Number(index) => {
                let index = index.as_u64().unwrap() as usize;
                parent.as_array_mut().unwrap().get_mut(index).unwrap()
            }
            other => panic!("bad edit path element {other}"),
        };
    }
    let delete = change.get("delete") == Some(&Value::Bool(true));
    match (&path[path.len() - 1], parent) {
        (Value::String(key), Value::Object(map)) => {
            assert_ne!(key, "$only");
            if delete {
                assert!(map.remove(key).is_some(), "deleting a missing key");
            } else {
                map.insert(key.clone(), change["set"].clone());
            }
        }
        (Value::Number(index), Value::Array(items)) => {
            let index = index.as_u64().unwrap() as usize;
            if delete {
                items.remove(index);
            } else {
                items[index] = change["set"].clone();
            }
        }
        (key, parent) => panic!("cannot edit {key} in {parent}"),
    }
}

fn project(actual: &Value, expected: &Value) -> Value {
    match expected {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let item = actual
                        .get(key)
                        .unwrap_or_else(|| panic!("projection: missing {key} in {actual}"));
                    (key.clone(), project(item, value))
                })
                .collect(),
        ),
        _ => actual.clone(),
    }
}

fn fixture_cases(fixture: &Value) -> Vec<Value> {
    let mut cases = fixture["cases"].as_array().unwrap().clone();
    for rejection in fixture["rejections"].as_array().unwrap() {
        let mut defect = rejection["repair"].clone();
        defect["tag"] = rejection["name"].clone();
        cases.push(json!({
            "name": rejection["name"],
            "setup": rejection.get("setup").cloned().unwrap_or_else(|| json!({})),
            "steps": [{
                "request": rejection["request"],
                "expect": {"error": rejection["error"]},
                "op": rejection.get("op").cloned().unwrap_or_else(|| json!("write")),
                "defect": defect,
            }],
        }));
    }
    for corruption in fixture["corruptions"].as_array().unwrap() {
        let variants: &[bool] = if corruption.get("expired") == Some(&Value::Bool(false)) {
            &[false]
        } else {
            &[false, true]
        };
        for &expired in variants {
            let name = corruption["name"].as_str().unwrap();
            let request = if expired {
                json!({"C": 110, "summary": {"id": "y", "content": "new"}})
            } else {
                json!({"C": 11})
            };
            let error = corruption
                .get("error")
                .cloned()
                .unwrap_or_else(|| json!("InvalidBundle"));
            cases.push(json!({
                "name": format!("{name}-{}", if expired { "expired" } else { "live" }),
                "steps": [
                    {"expect": {"status": "created"}},
                    {
                        "request": request,
                        "corruption": {"enabled": true, "change": corruption["change"]},
                        "expect": {"error": error},
                        "defect": {"tag": name, "path": ["steps", 1, "corruption", "enabled"],
                                   "set": false},
                    },
                ],
            }));
        }
    }
    cases
}

fn case_name(case: &Value) -> &str {
    case["name"].as_str().unwrap()
}

// ---------------------------------------------------------------------------
// JSON request -> private Rust request.

fn scope_of(value: &Value) -> MemoryScope {
    let map = value.as_object().unwrap();
    assert!(
        map.keys()
            .all(|key| ["tenant", "agent", "user", "session"].contains(&key.as_str()))
    );
    let text = |key: &str| optional_string(map.get(key));
    MemoryScope {
        tenant: text("tenant").unwrap_or_default(),
        agent: text("agent").unwrap_or_default(),
        user: text("user"),
        session: text("session"),
    }
}

fn optional_string(value: Option<&Value>) -> Option<String> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.clone()),
        Some(other) => panic!("unsupported optional string {other}"),
    }
}

fn number(value: &Value) -> f64 {
    if let Some(name) = value.get("$number") {
        return match name.as_str().unwrap() {
            "nan" => f64::NAN,
            "inf" => f64::INFINITY,
            "zero" => 0.0,
            other => panic!("unknown $number {other}"),
        };
    }
    value.as_f64().unwrap()
}

fn assert_fields(map: &Map<String, Value>, allowed: &[&str], what: &str) {
    for key in map.keys() {
        assert!(
            allowed.contains(&key.as_str()),
            "unsupported {what} field {key}"
        );
    }
}

/// `off` shifts the execution, deadline and summary expiry clocks (expiry
/// only when it fits i64; far-future u64 expiries stay literal).
fn request_of(raw: &Value, off: u64) -> ConsolidateRequest {
    let map = raw.as_object().unwrap();
    assert_fields(
        map,
        &[
            "scope",
            "idempotency_key",
            "C",
            "receipt_window_ms",
            "input_budget",
            "inputs",
            "summary",
            "transformation",
            "evidence",
            "allow_future_observations",
            "deadline",
        ],
        "request",
    );
    let shift = |value: u64| value.checked_add(off).expect("clock offset overflow");
    let (now_ms, clock_valid) = match raw["C"].as_u64() {
        Some(value) if value <= I64_MAX => (shift(value), true),
        Some(value) => {
            assert_eq!(off, 0);
            (value, true)
        }
        None => (0, false),
    };
    let summary = raw["summary"].as_object().unwrap();
    assert_fields(
        summary,
        &[
            "id",
            "source",
            "content",
            "importance",
            "observed_at_ms",
            "expiry_ms",
            "provenance",
        ],
        "summary",
    );
    let (expiry_ms, expiry_valid) = match summary.get("expiry_ms") {
        None | Some(Value::Null) => (None, true),
        Some(value) => match value.as_u64() {
            Some(expiry) if expiry <= I64_MAX => (Some(shift(expiry)), true),
            Some(expiry) => (Some(expiry), true),
            None => (None, false),
        },
    };
    let provenance = summary
        .get("provenance")
        .map(|entries| entries.as_array().unwrap().as_slice())
        .unwrap_or_default()
        .iter()
        .map(|entry| {
            let entry = entry.as_object().unwrap();
            assert_fields(
                entry,
                &["source", "kind", "uri", "transformation"],
                "provenance",
            );
            Provenance {
                source: entry["source"].as_str().unwrap().to_owned(),
                kind: optional_string(entry.get("kind")),
                uri: optional_string(entry.get("uri")),
                transformation: optional_string(entry.get("transformation")),
            }
        })
        .collect();
    let inputs = raw["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|input| {
            let input = input.as_object().unwrap();
            assert_fields(
                input,
                &["id", "expected_memory_revision", "disposition"],
                "input",
            );
            ConsolidateInput {
                id: input["id"].as_str().unwrap().to_owned(),
                expected_memory_revision: input["expected_memory_revision"].as_u64(),
                retain: input["disposition"] == "retain",
            }
        })
        .collect();
    let evidence = match raw.get("evidence") {
        None | Some(Value::Null) => None,
        Some(evidence) => {
            let map = evidence.as_object().unwrap();
            assert_fields(map, &["id", "scope"], "evidence");
            Some(MemoryEvidence {
                id: map["id"].as_str().unwrap().to_owned(),
                scope: scope_of(map.get("scope").unwrap_or(&json!({}))),
            })
        }
    };
    ConsolidateRequest {
        scope: scope_of(&raw["scope"]),
        idempotency_key: raw["idempotency_key"].as_str().unwrap().to_owned(),
        now_ms,
        clock_valid,
        receipt_window_ms: raw["receipt_window_ms"].as_u64().unwrap(),
        deadline_ms: raw
            .get("deadline")
            .map(|value| shift(value.as_u64().unwrap())),
        input_budget: raw.get("input_budget").map(|value| value.as_u64().unwrap()),
        inputs,
        summary: ConsolidateSummary {
            id: summary["id"].as_str().unwrap().to_owned(),
            content: summary["content"].as_str().unwrap().to_owned(),
            source: summary["source"].as_str().unwrap().to_owned(),
            observed_at_ms: summary["observed_at_ms"].as_i64(),
            importance: summary.get("importance").map(number),
            expiry_ms,
            expiry_valid,
            provenance,
        },
        transformation: raw["transformation"].as_str().unwrap().to_owned(),
        evidence,
        allow_future_observations: raw
            .get("allow_future_observations")
            .is_some_and(|value| value.as_bool().unwrap()),
    }
}

fn error_name(error: &MemoryError) -> String {
    match error {
        MemoryError::Storage(crate::Error::GraphNodeNotFound) => "fixture:EndpointRefused".into(),
        other => format!("{other:?}"),
    }
}

fn digest_step(raw: &Value) -> Value {
    match normalize(&request_of(raw, 0)) {
        Err(error) => json!({"error": error_name(&error)}),
        Ok(normalized) => json!({
            "request_digest": digest(&normalized.request_bytes),
            "source_digest": digest(&normalized.source_bytes),
            "request_bytes": normalized.request_bytes.len(),
            "request_hex": hex(&normalized.request_bytes),
            "source_bytes_hex": hex(&normalized.source_bytes),
        }),
    }
}

// ---------------------------------------------------------------------------
// Value conversions.

fn metadata_of(value: &Value) -> Metadata {
    value
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| {
            let value = match value {
                Value::String(text) => MetadataValue::String(text.clone()),
                Value::Bool(flag) => MetadataValue::Boolean(*flag),
                Value::Number(number) if number.is_f64() => {
                    MetadataValue::Float(number.as_f64().unwrap())
                }
                Value::Number(number) => MetadataValue::Integer(number.as_i64().unwrap()),
                other => panic!("unsupported property {other}"),
            };
            (key.clone(), value)
        })
        .collect()
}

fn json_of_value(value: &MetadataValue) -> Value {
    match value {
        MetadataValue::String(text) => json!(text),
        MetadataValue::Integer(number) => json!(number),
        MetadataValue::Float(number) => json!(number),
        MetadataValue::Boolean(flag) => json!(flag),
        other => panic!("unsupported property {other:?}"),
    }
}

fn json_of_property(value: &PropertyValue) -> Value {
    match value {
        PropertyValue::String(text) => json!(text),
        PropertyValue::Integer(number) => json!(number),
        PropertyValue::Float(number) => json!(number),
        PropertyValue::Boolean(flag) => json!(flag),
        other => panic!("unsupported property {other:?}"),
    }
}

fn node_json(node: &NodeView) -> Value {
    json!({
        "key": node.key,
        "labels": node.labels.iter().collect::<Vec<_>>(),
        "properties": Value::Object(node
            .properties
            .iter()
            .map(|(key, value)| (key.clone(), json_of_value(value)))
            .collect()),
    })
}

fn text_of<'a>(node: &'a NodeView, name: &str) -> &'a str {
    match node.properties.get(name) {
        Some(MetadataValue::String(value)) => value,
        other => panic!("{} property {name}: {other:?}", node.key),
    }
}

fn read_options() -> ReadOptions<'static> {
    ReadOptions {
        limits: GraphLimits::CEILING,
        ..ReadOptions::default()
    }
}

fn find_all(read: &GraphRef<'_>, label: &str, conditions: Vec<Condition>) -> Vec<NodeView> {
    let mut cursor = None;
    let mut nodes = Vec::new();
    loop {
        let page = read
            .find_nodes(
                &FindNodes {
                    label: Some(label.into()),
                    conditions: conditions.clone(),
                    limit: Some(1_000),
                    cursor,
                },
                &read_options(),
            )
            .unwrap();
        nodes.extend(page.output);
        match page.cursor {
            Some(next) => cursor = Some(next),
            None => {
                assert!(page.complete);
                return nodes;
            }
        }
    }
}

fn eq(field: &str, value: &str) -> Condition {
    Condition::Eq {
        field: field.into(),
        value: MetadataValue::String(value.into()),
    }
}

fn digits(value: u64) -> i64 {
    value.to_string().len() as i64
}

/// Whether the case's golden clocks interact with real TTL expiry.
fn case_needs_offset(case: &Value, base: &Value) -> bool {
    let setup = expand(case.get("setup").unwrap_or(&json!({})));
    let source_ttl = setup
        .get("sources")
        .and_then(Value::as_array)
        .is_some_and(|sources| {
            sources.iter().any(|source| {
                source
                    .get("expiry_ms")
                    .is_some_and(|value| !value.is_null())
            })
        });
    let clocks = setup.get("wall").is_some() || setup.get("high_water").is_some();
    // A summary expiry needs real time only when it is still live at C;
    // born-expired requests are refused before any commit clock exists.
    let steps = case["steps"].as_array().unwrap().iter().any(|step| {
        let c = step
            .pointer("/request/C")
            .map_or(base["C"].as_u64(), Value::as_u64)
            .unwrap_or(0);
        step["op"] == "visible"
            || step
                .pointer("/request/summary/expiry_ms")
                .and_then(Value::as_u64)
                .is_some_and(|expiry| expiry > c && expiry <= I64_MAX)
    });
    source_ttl || clocks || steps
}

// ---------------------------------------------------------------------------
// The harness over a real database file.

struct Harness {
    _dir: tempfile::TempDir,
    path: PathBuf,
    db: Option<Database>,
    off: u64,
    base: Value,
    bindings: Vec<(MemoryScope, Option<String>)>,
    sources: Vec<Value>,
    records: Vec<Value>,
    collections: BTreeSet<String>,
    source_revisions: BTreeMap<String, u64>,
    /// Real commit sequence -> (C1 commit ordinal, receipt write ordinal).
    commits: BTreeMap<u64, (u64, u64)>,
    receipt_writes: BTreeMap<String, u64>,
    sequence: u64,
    /// A corruption the engine refuses to persist (string over 4096 bytes).
    unstorable_outcome: Option<String>,
}

impl Harness {
    fn new(case: &Value, base: &Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c1.vlite");
        let mut db = Database::create(&path, 2).unwrap();
        db.create_graph(GRAPH).unwrap();
        let off = if case_needs_offset(case, base) {
            crate::current_time_ms() + 3_600_000
        } else {
            0
        };
        let setup = expand(case.get("setup").unwrap_or(&json!({})));
        assert_fields(
            setup.as_object().unwrap(),
            &[
                "sources",
                "records",
                "bindings",
                "edits",
                "wall",
                "high_water",
                "opening_reclaims",
            ],
            "setup",
        );
        let base_scope = base["scope"].clone();
        let bindings: Vec<(MemoryScope, Option<String>)> = setup
            .get("bindings")
            .cloned()
            .unwrap_or_else(|| json!([{"scope": base_scope, "collection": "c"}]))
            .as_array()
            .unwrap()
            .iter()
            .map(|binding| {
                (
                    scope_of(&binding["scope"]),
                    optional_string(binding.get("collection")),
                )
            })
            .collect();
        let source_defaults = json!({
            "scope": base_scope, "revision": 1, "kind": "entity", "expiry_ms": null,
            "content": "source", "source": "origin", "attachments": [],
            "properties": {"retained": "unchanged"}, "malformed": false,
        });
        let sources: Vec<Value> = setup
            .get("sources")
            .cloned()
            .unwrap_or_else(|| json!([{"id": "m"}]))
            .as_array()
            .unwrap()
            .iter()
            .map(|source| merge(&source_defaults, source))
            .collect();
        let record_defaults = json!({"scope": base_scope, "collection": "c", "owners": []});
        let records: Vec<Value> = setup
            .get("records")
            .cloned()
            .unwrap_or_else(|| json!([]))
            .as_array()
            .unwrap()
            .iter()
            .map(|record| merge(&record_defaults, record))
            .collect();
        let mut collections = BTreeSet::new();
        collections.extend(
            bindings
                .iter()
                .filter_map(|(_, collection)| collection.clone()),
        );
        collections.extend(
            records
                .iter()
                .map(|record| record["collection"].as_str().unwrap().to_owned()),
        );
        for collection in &collections {
            db.create_collection(
                collection,
                crate::CollectionSpec {
                    dimensions: 2,
                    settings: crate::CollectionSettings::default(),
                },
            )
            .unwrap();
        }
        for (scope, _) in &bindings {
            MemoryMut::bind(&mut db, GRAPH, None)
                .unwrap()
                .provision_scope(scope)
                .unwrap();
        }

        let mut ops = Vec::new();
        let mut source_keys = BTreeMap::new();
        for source in &sources {
            assert_fields(
                source.as_object().unwrap(),
                &[
                    "id",
                    "scope",
                    "revision",
                    "kind",
                    "expiry_ms",
                    "content",
                    "source",
                    "attachments",
                    "properties",
                    "malformed",
                    // Not reproducible: graph revisions are engine sequences.
                    "graph_revision",
                ],
                "source",
            );
            let scope = scope_of(&source["scope"]);
            let scope_key = scope.key();
            let id = source["id"].as_str().unwrap();
            let content = source["content"].as_str().unwrap();
            let origin = source["source"].as_str().unwrap().to_owned();
            let key = memory_key(&scope_key, id);
            let mut properties = memory_properties(MemoryProperties {
                scope: &scope_key,
                id,
                kind: source["kind"].as_str().unwrap(),
                content,
                digest: &content_digest(content),
                sources: std::slice::from_ref(&origin),
                provenance: &[Provenance {
                    source: origin.clone(),
                    kind: None,
                    uri: None,
                    transformation: None,
                }],
                importance: 0.5,
                created_at_ms: off as i64,
                updated_at_ms: off as i64,
                observed_at_ms: off as i64,
                memory_revision: source["revision"].as_u64().unwrap(),
                reference_count: 0,
                evidence: &[],
            });
            for (name, value) in metadata_of(&source["properties"]) {
                assert!(properties.insert(name, value).is_none());
            }
            if source["malformed"].as_bool().unwrap() {
                properties.insert(
                    "memory_revision".into(),
                    MetadataValue::String("malformed".into()),
                );
            }
            ops.push(GraphOp::CreateNode {
                key: key.clone(),
                labels: vec![LABEL_MEMORY.into()],
                properties,
            });
            if let Some(expiry) = source["expiry_ms"].as_u64() {
                assert_ne!(off, 0);
                ops.push(GraphOp::SetNodeExpiry {
                    key: key.clone(),
                    expires_at_ms: Some(off + expiry),
                    expected: Expected::Any,
                });
            }
            source_keys.insert((scope.encode(), id.to_owned()), key);
        }
        let base_encoded = scope_of(&base_scope).encode();
        let mut owners_created = BTreeSet::new();
        let mut attaches = Vec::new();
        for record in &records {
            assert_fields(
                record.as_object().unwrap(),
                &["id", "scope", "collection", "owners"],
                "record",
            );
            let name = record["id"].as_str().unwrap();
            let collection = record["collection"].as_str().unwrap().to_owned();
            let scope = scope_of(&record["scope"]);
            let record_id = evidence_record_id(name);
            ops.push(GraphOp::Vector {
                collection: collection.clone(),
                mutation: crate::Mutation::Put {
                    id: record_id,
                    vector: vec![1.0, 0.0],
                    payload: Vec::new(),
                },
            });
            let owners = record["owners"].as_array().unwrap();
            let mut owner_keys = Vec::new();
            for owner in owners {
                let owner = owner.as_str().unwrap();
                if let Some(key) = source_keys.get(&(scope.encode(), owner.to_owned())) {
                    owner_keys.push(key.clone());
                } else {
                    let key = format!("fixture-owner:{owner}");
                    if owners_created.insert(key.clone()) {
                        ops.push(GraphOp::CreateNode {
                            key: key.clone(),
                            labels: vec![OWNER_LABEL.into()],
                            properties: Metadata::new(),
                        });
                    }
                    owner_keys.push(key);
                }
            }
            if scope.encode() != base_encoded {
                assert!(owners.is_empty(), "a foreign record scope is its owner");
                let key = memory_key(&scope.key(), &format!("fixture-record-scope:{name}"));
                let mut properties = Metadata::new();
                properties.insert("scope".into(), MetadataValue::String(scope.key()));
                ops.push(GraphOp::CreateNode {
                    key: key.clone(),
                    labels: vec![LABEL_MEMORY.into()],
                    properties,
                });
                owner_keys.push(key);
            }
            assert!(owner_keys.len() <= 1, "a record has at most one owner");
            for key in owner_keys {
                attaches.push(GraphOp::Attach {
                    node: key,
                    collection: collection.clone(),
                    record: record_id,
                });
            }
        }
        for source in &sources {
            let id = &source["id"];
            let owned: Vec<Value> = records
                .iter()
                .filter(|record| record["owners"].as_array().unwrap().contains(id))
                .map(|record| record["id"].clone())
                .collect();
            assert_eq!(source["attachments"], Value::Array(owned), "attachments");
        }
        ops.extend(attaches);
        if !ops.is_empty() {
            db.graph_mut(GRAPH)
                .unwrap()
                .write_batch_at(&ops, off)
                .unwrap();
        }
        for change in setup
            .get("edits")
            .map(|edits| edits.as_array().unwrap().clone())
            .unwrap_or_default()
        {
            assert_eq!(change["path"], json!(["scopes", "$only", "tuple"]));
            assert_eq!(bindings.len(), 1);
            let mut properties = Metadata::new();
            properties.insert(
                "tuple".into(),
                MetadataValue::String(change["set"].as_str().unwrap().to_owned()),
            );
            db.graph_mut(GRAPH)
                .unwrap()
                .write_batch_at(
                    &[GraphOp::UpsertNode {
                        key: bindings[0].0.key(),
                        labels: vec![LABEL_SCOPE.into()],
                        properties,
                        expected: Expected::Any,
                    }],
                    off,
                )
                .unwrap();
        }
        let wall = setup.get("wall").map(|value| value.as_u64().unwrap());
        let high_water = setup.get("high_water").map(|value| value.as_u64().unwrap());
        if wall.is_some() || high_water.is_some() {
            assert_ne!(off, 0);
            let clock = off + wall.unwrap_or(0).max(high_water.unwrap_or(0));
            db.graph_clock = db.graph_clock.max(clock);
        }
        match setup
            .get("opening_reclaims")
            .map(|value| value.as_u64().unwrap())
        {
            None | Some(0) => {}
            Some(1) => {
                assert_eq!(off, 0);
                let due = crate::current_time_ms() + 60_000;
                db.graph_mut(GRAPH)
                    .unwrap()
                    .write_batch(&[
                        GraphOp::CreateNode {
                            key: "fixture-reclaim".into(),
                            labels: vec!["FixtureReclaim".into()],
                            properties: Metadata::new(),
                        },
                        GraphOp::SetNodeExpiry {
                            key: "fixture-reclaim".into(),
                            expires_at_ms: Some(due),
                            expected: Expected::Any,
                        },
                    ])
                    .unwrap();
                // The next commit captures this high-water and reclaims the
                // node first: exactly one opening reclaim.
                db.graph_clock = db.graph_clock.max(due);
            }
            Some(other) => panic!("opening_reclaims {other}"),
        }
        let mut harness = Self {
            _dir: dir,
            path,
            db: Some(db),
            off,
            base: base.clone(),
            bindings,
            sources,
            records,
            collections,
            source_revisions: BTreeMap::new(),
            commits: BTreeMap::new(),
            receipt_writes: BTreeMap::new(),
            sequence: 0,
            unstorable_outcome: None,
        };
        // Setup sources are live at the setup clock even when a later fixture
        // high-water already makes them due.
        let read = harness.db().graph(GRAPH).unwrap().with_clock(off);
        let revisions = source_keys
            .values()
            .map(|key| {
                let node = read.get_node(key).unwrap().output.unwrap();
                (key.clone(), node.revision)
            })
            .collect();
        harness.source_revisions = revisions;
        harness
    }

    fn db(&self) -> &Database {
        self.db.as_ref().unwrap()
    }

    fn db_mut(&mut self) -> &mut Database {
        self.db.as_mut().unwrap()
    }

    /// The clock of the next harness write, so its reads see what it writes.
    fn clock(&self) -> u64 {
        self.db().graph_clock.max(crate::current_time_ms())
    }

    fn read(&self) -> GraphRef<'_> {
        self.db().graph(GRAPH).unwrap().with_clock(self.clock())
    }

    fn harness_write(&mut self, ops: &[GraphOp]) {
        let clock = self.clock();
        self.db_mut()
            .graph_mut(GRAPH)
            .unwrap()
            .write_batch_at(ops, clock)
            .unwrap();
    }

    /// Raw removal of a memory node with its edges, chunks and attachments.
    fn remove_memory(&mut self, key: &str) {
        let read = self.read();
        let mut edges = BTreeMap::new();
        let mut cursor = None;
        loop {
            let page = read
                .neighbors(
                    &Neighbors {
                        node: NodeSelector::key(key),
                        direction: Direction::Both,
                        types: None,
                        edge_conditions: Vec::new(),
                        limit: Some(GraphLimits::CEILING.max_output_edges),
                        cursor,
                    },
                    &read_options(),
                )
                .unwrap();
            for edge in page.output.edges {
                edges.insert(edge.id, edge);
            }
            match page.cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        let attached: BTreeSet<u16> = read
            .memory_node_attachments(key)
            .unwrap()
            .into_iter()
            .map(|(collection, _)| collection)
            .collect();
        let mut ops = Vec::new();
        for collection in &self.collections {
            if attached.contains(&self.db().resolve(collection).unwrap()) {
                ops.push(GraphOp::Detach {
                    node: key.to_owned(),
                    collection: collection.clone(),
                });
            }
        }
        for edge in edges.values() {
            ops.push(GraphOp::DeleteEdge {
                edge: EdgeRef::Id(edge.id),
                expected: Expected::Any,
            });
        }
        for edge in edges.values() {
            if edge.relationship == "HAS_CONTENT" && edge.source_key == key {
                ops.push(GraphOp::DeleteNode {
                    key: edge.target_key.clone(),
                    expected: Expected::Any,
                });
            }
        }
        ops.push(GraphOp::DeleteNode {
            key: key.to_owned(),
            expected: Expected::Any,
        });
        self.harness_write(&ops);
    }

    fn corrupt(&mut self, change: &Value) {
        let read = self.read();
        let receipts = find_all(&read, LABEL_RECEIPT, Vec::new());
        assert_eq!(receipts.len(), 1, "corruption requires one receipt");
        let receipt = &receipts[0];
        let outcome_key = receipt_outcome_key(&receipt.key, 0);
        let outcome = read.get_node(&outcome_key).unwrap().output.unwrap();
        let mut projected = json!({"node": node_json(receipt), "outcomes": [node_json(&outcome)]});
        edit(&mut projected, &expand(change));
        let deletes = vec![
            GraphOp::DeleteNode {
                key: receipt.key.clone(),
                expected: Expected::Any,
            },
            GraphOp::DeleteNode {
                key: outcome_key,
                expected: Expected::Any,
            },
        ];
        let creates: Vec<GraphOp> = std::iter::once(&projected["node"])
            .chain(projected["outcomes"].as_array().unwrap())
            .map(|node| GraphOp::CreateNode {
                key: node["key"].as_str().unwrap().to_owned(),
                labels: node["labels"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|label| label.as_str().unwrap().to_owned())
                    .collect(),
                properties: metadata_of(&node["properties"]),
            })
            .collect();
        let oversized = projected["outcomes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|node| node["properties"]["outcome"].as_str())
            .find(|outcome| outcome.len() > super::super::MAX_STRING_VALUE_BYTES);
        if let Some(outcome) = oversized {
            // The engine cannot persist this corruption: prove the refusal and
            // leave storage untouched; the step then decodes the value itself.
            let clock = self.clock();
            let before = (self.db().sequence, self.db().end);
            let refused = self
                .db_mut()
                .graph_mut(GRAPH)
                .unwrap()
                .write_batch_at(&creates, clock);
            assert!(matches!(refused, Err(crate::Error::InvalidMetadata(_))));
            assert_eq!(before, (self.db().sequence, self.db().end));
            self.unstorable_outcome = Some(outcome.to_owned());
            return;
        }
        self.harness_write(&deletes);
        self.harness_write(&creates);
    }

    fn execute(&mut self, step: &Value) -> Value {
        for change in step
            .get("edits")
            .map(|edits| edits.as_array().unwrap().clone())
            .unwrap_or_default()
        {
            assert_eq!(expand(&change), json!({"path": ["sources"], "set": []}));
            let keys: Vec<String> = self.source_revisions.keys().cloned().collect();
            for key in keys {
                if self.read().get_node(&key).unwrap().output.is_some() {
                    self.remove_memory(&key);
                }
            }
            self.sources.clear();
        }
        if let Some(corruption) = step.get("corruption")
            && corruption["enabled"].as_bool().unwrap()
        {
            self.corrupt(&corruption["change"]);
        }
        let op = step.get("op").and_then(Value::as_str).unwrap_or("write");
        match op {
            "reopen" => {
                let clock = self.db().graph_clock;
                drop(self.db.take());
                let mut db = Database::open(&self.path).unwrap();
                db.graph_clock = db.graph_clock.max(clock);
                self.db = Some(db);
                json!({"reopened": true})
            }
            "forget" => {
                let id = step["id"].as_str().unwrap();
                let keys: Vec<String> = find_all(&self.read(), LABEL_MEMORY, vec![eq("id", id)])
                    .into_iter()
                    .map(|node| node.key)
                    .collect();
                for key in keys {
                    self.remove_memory(&key);
                }
                self.sources.retain(|source| source["id"] != id);
                json!({"forgotten": id})
            }
            "inspect" => self.inspect(step),
            "visible" => self.visible(step),
            // Intentionally only the oracle's literal relationship filter.
            "references" => json!({"references": step["edges"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|edge| edge["relationship"] == "REFERENCES")
                .cloned()
                .collect::<Vec<_>>()}),
            "digest" | "write" => {
                let mut raw = expand(&merge(
                    &self.base,
                    step.get("request").unwrap_or(&json!({})),
                ));
                for change in step
                    .get("request_edits")
                    .map(|edits| edits.as_array().unwrap().clone())
                    .unwrap_or_default()
                {
                    edit(&mut raw, &expand(&change));
                }
                if op == "digest" {
                    digest_step(&raw)
                } else {
                    self.write(&raw)
                }
            }
            other => panic!("unsupported op {other}"),
        }
    }

    fn inspect(&self, step: &Value) -> Value {
        assert_fields(
            step["expect"].as_object().unwrap(),
            &["sources", "summaries", "records", "sequence"],
            "inspect",
        );
        let read = self.read();
        let mut sources = Vec::new();
        for source in &self.sources {
            let scope_key = scope_of(&source["scope"]).key();
            let id = source["id"].as_str().unwrap();
            let key = memory_key(&scope_key, id);
            let Some(node) = read.get_node(&key).unwrap().output else {
                continue;
            };
            // Consolidation never rewrites a source node.
            assert_eq!(node.revision, self.source_revisions[&key], "{id} mutated");
            assert_eq!(text_of(&node, "scope"), scope_key);
            let mut object = Map::new();
            for name in source.as_object().unwrap().keys() {
                let value = match name.as_str() {
                    "scope" => source["scope"].clone(),
                    "id" => json!(text_of(&node, "id")),
                    "revision" => json!(text_of(&node, "memory_revision").parse::<u64>().unwrap()),
                    "kind" => json!(text_of(&node, "kind")),
                    "expiry_ms" => json!(node.expires_at_ms.map(|expiry| expiry - self.off)),
                    "content" => json!(text_of(&node, "content")),
                    "source" => {
                        let origins = decode_sources(text_of(&node, "sources")).unwrap();
                        assert_eq!(origins.len(), 1);
                        json!(origins[0])
                    }
                    "attachments" => json!(
                        read.memory_node_attachments(&key)
                            .unwrap()
                            .iter()
                            .map(|(_, record)| self.record_name(*record))
                            .collect::<Vec<_>>()
                    ),
                    "properties" => Value::Object(
                        source["properties"]
                            .as_object()
                            .unwrap()
                            .keys()
                            .map(|field| (field.clone(), json_of_value(&node.properties[field])))
                            .collect(),
                    ),
                    "malformed" => {
                        json!(text_of(&node, "memory_revision").parse::<u64>().is_err())
                    }
                    "graph_revision" => continue,
                    other => panic!("inspect cannot project source field {other}"),
                };
                object.insert(name.clone(), value);
            }
            sources.push(Value::Object(object));
        }
        let summaries: Vec<Value> = find_all(&read, LABEL_MEMORY, Vec::new())
            .iter()
            .filter(|node| node.properties.contains_key("c1_schema"))
            .map(|node| json!({"id": text_of(node, "id")}))
            .collect();
        let mut records = Vec::new();
        for record in &self.records {
            let name = record["id"].as_str().unwrap();
            let collection = record["collection"].as_str().unwrap();
            let mut owners = Vec::new();
            if let Some((graph, key)) = self
                .db()
                .memory_attachment_owner(collection, evidence_record_id(name), self.clock())
                .unwrap()
            {
                assert_eq!(graph, GRAPH);
                let owner = read.get_node(&key).unwrap().output.unwrap();
                if owner.labels.contains(OWNER_LABEL) {
                    owners.push(json!(key.strip_prefix("fixture-owner:").unwrap()));
                } else if owner.properties.contains_key("id") {
                    owners.push(json!(text_of(&owner, "id")));
                }
                // Otherwise: the harness owner standing for a foreign scope.
            }
            records.push(json!({
                "scope": record["scope"], "collection": collection, "owners": owners, "id": name,
            }));
        }
        json!({
            "sources": sources, "summaries": summaries, "records": records,
            "sequence": self.sequence,
        })
    }

    fn record_name(&self, record: u64) -> String {
        self.records
            .iter()
            .map(|candidate| candidate["id"].as_str().unwrap())
            .find(|name| evidence_record_id(name) == record)
            .unwrap()
            .to_owned()
    }

    fn visible(&self, step: &Value) -> Value {
        assert_ne!(self.off, 0);
        let clock = self.off + step["clock"].as_u64().unwrap();
        let scope = scope_of(step.get("scope").unwrap_or(&self.base["scope"]));
        let scope_key = scope.key();
        let read = self.db().graph(GRAPH).unwrap().with_clock(clock);
        let live: BTreeSet<String> = find_all(&read, LABEL_MEMORY, vec![eq("scope", &scope_key)])
            .iter()
            .map(|node| text_of(node, "id").to_owned())
            .collect();
        let collection = self
            .bindings
            .iter()
            .find(|(binding, _)| binding.encode() == scope.encode())
            .and_then(|(_, collection)| collection.clone());
        let mut evidence = Vec::new();
        for record in &self.records {
            let name = record["id"].as_str().unwrap();
            if scope_of(&record["scope"]).encode() != scope.encode()
                || Some(record["collection"].as_str().unwrap()) != collection.as_deref()
            {
                continue;
            }
            let owner = self
                .db()
                .memory_attachment_owner(
                    collection.as_deref().unwrap(),
                    evidence_record_id(name),
                    clock,
                )
                .unwrap();
            if let Some((graph, key)) = owner
                && graph == GRAPH
                && let Some(owner) = read.get_node(&key).unwrap().output
                && owner.labels.contains(LABEL_MEMORY)
                && text_of(&owner, "scope") == scope_key
                && live.contains(text_of(&owner, "id"))
            {
                evidence.push(name.to_owned());
            }
        }
        evidence.sort();
        json!({"memories": live.into_iter().collect::<Vec<_>>(), "evidence": evidence})
    }

    fn write(&mut self, raw: &Value) -> Value {
        let request = request_of(raw, self.off);
        if let Some(outcome) = self.unstorable_outcome.take() {
            // Stateless validation still runs first (second-defect checks).
            if let Err(error) = normalize(&request) {
                return json!({"error": error_name(&error)});
            }
            let error = decode_outcome(&outcome).unwrap_err();
            return json!({"error": error_name(&error)});
        }
        let collection = self
            .bindings
            .iter()
            .find(|(scope, _)| scope.encode() == request.scope.encode())
            .and_then(|(_, collection)| collection.clone());
        let before = (self.db().sequence, self.db().end);
        let result = MemoryMut::bind(self.db_mut(), GRAPH, collection.as_deref())
            .unwrap()
            .consolidate(&request);
        let after = (self.db().sequence, self.db().end);
        let result = match result {
            Err(error) => {
                assert_eq!(before, after, "a rejection appended");
                return json!({"error": error_name(&error)});
            }
            Ok(result) => result,
        };
        let outcome = json!({
            "id": result.summary_id, "revision": result.summary_revision,
            "N": result.source_count, "source_digest": result.source_digest,
        });
        let receipt_key = operation_receipt_key(
            "consolidate",
            &request.scope.key(),
            &request.idempotency_key,
        );
        let (receipt, outcome_node) = {
            let read = self.read();
            let receipt = read.get_node(&receipt_key).unwrap().output.unwrap();
            let outcome_node = read
                .get_node(&receipt_outcome_key(&receipt_key, 0))
                .unwrap()
                .output
                .unwrap();
            (receipt, outcome_node)
        };
        assert!(outcome_node.labels.contains(LABEL_RECEIPT_OUTCOME));
        let outcome_ascii = text_of(&outcome_node, "outcome").to_owned();
        let request_digest = text_of(&receipt, "digest").to_owned();
        if result.replayed {
            assert_eq!(before, after, "a replay appended");
            assert_eq!(result.logical_operations, 0);
            assert_eq!(result.receipt_revision, receipt.revision);
            assert_eq!(result.commit_sequence, receipt.revision);
            let (sequence, revision) = self.commits[&result.receipt_revision];
            return json!({
                "status": "replayed", "outcome": outcome, "outcome_ascii": outcome_ascii,
                "receipt_revision": revision, "sequence": sequence,
                "request_digest": request_digest,
            });
        }
        assert_eq!(after.0, before.0 + 1, "exactly one commit");
        assert_eq!(result.commit_sequence, after.0);
        assert_eq!(result.receipt_revision, after.0);
        assert_eq!(receipt.revision, after.0);
        self.sequence += 1;
        let writes = self.receipt_writes.entry(receipt_key).or_default();
        *writes += 1;
        self.commits.insert(after.0, (self.sequence, *writes));

        // The database holds a byte-range lock on Windows; read through it.
        let mut file = self.db().file.try_clone().unwrap();
        let mut frame = vec![0; (after.1 - before.1) as usize];
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(before.1)).unwrap();
        std::io::Read::read_exact(&mut file, &mut frame).unwrap();
        let frame = frame.as_slice();
        assert_eq!(&frame[..4], b"VREC");
        assert_eq!(frame[4], 12, "a graph frame");
        let count = u32::from_le_bytes(frame[24..28].try_into().unwrap());
        let payload = u32::from_le_bytes(frame[28..32].try_into().unwrap()) as usize;
        assert_eq!(frame.len(), 36 + payload + 16, "one frame per commit");
        let body = decode_body(&frame[36..36 + payload], count).unwrap();
        let read = self.read();
        let mut keys = BTreeMap::new();
        for entry in &body.entries {
            if let GraphEntry::PutNode { node_id, key, .. } = entry {
                keys.insert(node_id.0, key.clone());
            }
        }
        let key_of = |id: u64| -> String {
            keys.get(&id)
                .cloned()
                .unwrap_or_else(|| read.get_node_by_id(id).unwrap().output.unwrap().key)
        };
        let properties_json = |properties: &crate::property_graph::engine::Properties| {
            Value::Object(
                properties
                    .iter()
                    .map(|(name, value)| (name.clone(), json_of_property(value)))
                    .collect(),
            )
        };
        let mut entries = Vec::new();
        let mut sizes = Vec::new();
        for (index, entry) in body.entries.iter().enumerate() {
            let single = GraphBody {
                graph_id: body.graph_id,
                entries: vec![entry.clone()],
            };
            sizes.push(encode_body(&single).unwrap().len() - 4);
            entries.push(match entry {
                GraphEntry::PutNode {
                    key,
                    labels,
                    properties,
                    ..
                } => {
                    assert_eq!(labels.len(), 1);
                    let role = match labels.iter().next().unwrap().as_str() {
                        "VliteMemory" => "summary",
                        "VliteMemoryContent" => "content",
                        "VliteMemoryReceiptOutcome" => "outcome",
                        "VliteMemoryReceipt" => "receipt",
                        other => panic!("unexpected label {other}"),
                    };
                    json!({
                        "op": "PutNode", "role": role, "key": key,
                        "labels": labels.iter().collect::<Vec<_>>(),
                        "properties": properties_json(properties),
                    })
                }
                GraphEntry::PutEdge {
                    source,
                    destination,
                    relationship,
                    key,
                    properties,
                    ..
                } => {
                    let role = match relationship.as_str() {
                        "HAS_CONTENT" => "content_edge",
                        "DERIVED_FROM" => "derived",
                        other => panic!("unexpected relationship {other}"),
                    };
                    json!({
                        "op": "PutEdge", "role": role, "key": key, "relationship": relationship,
                        "from": key_of(source.0), "to": key_of(destination.0),
                        "properties": properties_json(properties),
                    })
                }
                GraphEntry::Attach { record_id, .. } => {
                    json!({"op": "Attach", "role": "attach", "id": self.record_name(*record_id)})
                }
                GraphEntry::NodeExpiry {
                    expires_at_ms: Some(_),
                    ..
                } => json!({"op": "SetNodeExpiry", "role": "expiry"}),
                GraphEntry::Reclaim { .. } => json!({
                    "op": "Reclaim",
                    "role": if index == 0 { "opening_reclaim" } else { "due_reclaim" },
                }),
                other => panic!("unexpected entry {other:?}"),
            });
        }
        assert_eq!(4 + sizes.iter().sum::<usize>(), payload);
        // Undo only the decimal growth of the shifted receipt expiry.
        let delta = if self.off == 0 {
            0
        } else {
            let end = raw["C"].as_u64().unwrap() + raw["receipt_window_ms"].as_u64().unwrap();
            digits(self.off + end) - digits(end)
        };
        let mut entry_bytes: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        for (entry, size) in entries.iter().zip(&sizes) {
            let role = entry["role"].as_str().unwrap();
            let size = *size as i64 - if role == "receipt" { delta } else { 0 };
            entry_bytes.entry(role.to_owned()).or_default().push(size);
        }
        let normalized = normalize(&request).unwrap();
        json!({
            "status": "created", "outcome": outcome, "outcome_ascii": outcome_ascii,
            "outcome_bytes": outcome_ascii.len(),
            "receipt_revision": self.commits[&after.0].1, "sequence": self.sequence,
            "request_digest": request_digest, "source_digest": result.source_digest,
            "request_bytes": normalized.request_bytes.len(),
            "source_bytes_hex": hex(&normalized.source_bytes),
            "physical_ops": entries.iter().filter(|entry| entry["op"] != "Reclaim").count(),
            "logical_ops": result.logical_operations,
            "frame_bytes": payload as i64 - delta,
            "entry_bytes": entry_bytes,
            "chunk_bytes": entries
                .iter()
                .filter(|entry| entry["role"] == "content")
                .map(|entry| entry["properties"]["content"].as_str().unwrap().len())
                .collect::<Vec<_>>(),
            "entries": entries,
        })
    }
}

fn run_case(case: &Value, base: &Value) -> Vec<Value> {
    let mut harness = Harness::new(case, base);
    case["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| harness.execute(step))
        .collect()
}

// ---------------------------------------------------------------------------
// Golden replays.

#[test]
fn c1_golden_cases_replay_exactly() {
    let fixture = golden();
    let base = &fixture["base_request"];
    let cases = fixture_cases(&fixture);
    let mut names = BTreeSet::new();
    let mut steps = 0;
    for case in &cases {
        assert!(names.insert(case_name(case).to_owned()), "duplicate case");
        let results = run_case(case, base);
        for (index, (step, actual)) in case["steps"]
            .as_array()
            .unwrap()
            .iter()
            .zip(&results)
            .enumerate()
        {
            let expected = &step["expect"];
            assert_eq!(
                &project(actual, expected),
                expected,
                "{} step {index}",
                case_name(case)
            );
            steps += 1;
        }
    }
    let counts = (
        fixture["cases"].as_array().unwrap().len(),
        fixture["rejections"].as_array().unwrap().len(),
        fixture["corruptions"].as_array().unwrap().len(),
    );
    assert_eq!(counts, (45, 33, 39));
    assert_eq!(cases.len(), 45 + 33 + 75);
    println!(
        "C1 golden: {} cases (45 literal, 33 rejections, 75 variants of 39 corruptions); \
         {steps} steps replayed",
        cases.len()
    );
}

#[test]
fn c1_golden_exact_nodes_property_sets_and_chunk_keys() {
    let fixture = golden();
    let base = &fixture["base_request"];
    let cases = fixture_cases(&fixture);
    let find = |name: &str| cases.iter().find(|case| case_name(case) == name).unwrap();
    let type_name = |value: &Value| match value {
        Value::String(_) => "String",
        Value::Number(number) if number.is_f64() => "Float",
        Value::Number(_) => "Integer",
        other => panic!("unexpected property {other}"),
    };
    let shape = |entry: &Value| {
        Value::Object(
            entry["properties"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(name, value)| (name.clone(), json!(type_name(value))))
                .collect(),
        )
    };
    let baseline = run_case(find("exact-inline-schema"), base).remove(0);
    let entries = baseline["entries"].as_array().unwrap();
    let exact = fixture["exact_nodes"].as_array().unwrap();
    assert_eq!(entries.len(), exact.len());
    for (entry, expected) in entries.iter().zip(exact) {
        assert_eq!(&project(entry, expected), expected);
        let role = entry["role"].as_str().unwrap();
        assert_eq!(shape(entry), fixture["property_sets"][role], "{role}");
    }
    let chunked = run_case(find("utf8-packing"), base).remove(0);
    let chunked = chunked["entries"].as_array().unwrap();
    let mut shapes = 0;
    for entry in chunked {
        let role = entry["role"].as_str().unwrap();
        if role == "content" || role == "content_edge" {
            assert_eq!(shape(entry), fixture["property_sets"][role], "{role}");
            shapes += 1;
        }
    }
    for (role, expected) in fixture["chunk_keys"].as_object().unwrap() {
        let entry = chunked
            .iter()
            .find(|entry| entry["role"] == role.as_str())
            .unwrap();
        assert_eq!(&project(entry, expected), expected, "{role}");
    }
    assert_eq!(fixture["property_sets"].as_object().unwrap().len(), 6);
    println!(
        "C1 golden: {} exact nodes, 6 property sets ({shapes} chunk entries), {} chunk keys",
        exact.len(),
        fixture["chunk_keys"].as_object().unwrap().len()
    );
}

#[test]
fn c1_golden_hash_probes() {
    let fixture = golden();
    let base = expand(&fixture["base_request"]);
    let mut count = 0;
    for probe in fixture["hash_probes"].as_array().unwrap() {
        let mut reference = base.clone();
        for change in probe
            .get("base_edits")
            .map(|edits| edits.as_array().unwrap().clone())
            .unwrap_or_default()
        {
            edit(&mut reference, &expand(&change));
        }
        let mut changed = reference.clone();
        for change in probe["edits"].as_array().unwrap() {
            edit(&mut changed, &expand(change));
        }
        let original = digest_step(&reference);
        let result = digest_step(&changed);
        let name = probe["name"].as_str().unwrap();
        assert!(original.get("error").is_none() && result.get("error").is_none());
        assert_eq!(
            result["request_digest"] == original["request_digest"],
            probe["equal"].as_bool().unwrap(),
            "hash probe {name}"
        );
        assert_eq!(
            result["source_digest"] == original["source_digest"],
            probe["source_equal"].as_bool().unwrap(),
            "source hash probe {name}"
        );
        count += 1;
    }
    assert_eq!(count, 33);
    println!("C1 golden: {count} hash probes");
}

#[test]
fn c1_golden_single_defect_repairs() {
    let fixture = golden();
    let base = &fixture["base_request"];
    let mut repairs = 0;
    for case in fixture_cases(&fixture) {
        for (index, step) in case["steps"].as_array().unwrap().iter().enumerate() {
            let rejected = step["expect"].get("error").is_some();
            assert_eq!(
                rejected,
                step.get("defect").is_some(),
                "{}",
                case_name(&case)
            );
            if !rejected {
                continue;
            }
            let defect = &step["defect"];
            let path = defect["path"].as_array().unwrap();
            let mut repaired = case.clone();
            repaired["steps"]
                .as_array_mut()
                .unwrap()
                .truncate(index + 1);
            // One scalar/empty-container/descriptor leaf cannot hide two defects.
            let old = lookup(&repaired, path);
            assert!(
                old.is_none_or(|old| match old {
                    Value::Object(map) => map.is_empty(),
                    Value::Array(items) => items.is_empty(),
                    _ => true,
                }),
                "repair replaces non-leaf"
            );
            edit(&mut repaired, defect);
            let results = run_case(&repaired, base);
            for (position, (candidate, actual)) in repaired["steps"]
                .as_array()
                .unwrap()
                .iter()
                .zip(&results)
                .enumerate()
            {
                if position == index {
                    assert!(
                        actual.get("error").is_none(),
                        "{} repair {} left {actual}",
                        case_name(&case),
                        defect["tag"]
                    );
                } else {
                    assert_eq!(
                        actual.get("error").is_some(),
                        candidate["expect"].get("error").is_some(),
                        "{} repair changed preceding acceptance",
                        case_name(&case)
                    );
                }
            }
            repairs += 1;
        }
    }
    assert!(repairs >= 33 + 75);
    println!("C1 golden: {repairs} single-defect rejections repaired");
}

#[test]
fn c1_golden_second_defect_survives_repair() {
    let fixture = golden();
    let base = &fixture["base_request"];
    let mut count = 0;
    for case in fixture_cases(&fixture) {
        if case_name(&case) == "key-empty" {
            continue;
        }
        for (index, step) in case["steps"].as_array().unwrap().iter().enumerate() {
            let Some(defect) = step.get("defect") else {
                continue;
            };
            let mut repaired = case.clone();
            edit(&mut repaired, defect);
            let mut harness = Harness::new(&repaired, base);
            for candidate in &repaired["steps"].as_array().unwrap()[..index] {
                harness.execute(candidate);
            }
            let mut candidate = repaired["steps"][index].clone();
            candidate
                .as_object_mut()
                .unwrap()
                .entry("request_edits")
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .unwrap()
                .push(json!({"path": ["idempotency_key"], "set": ""}));
            let actual = harness.execute(&candidate);
            assert_eq!(
                actual,
                json!({"error": "InvalidKey"}),
                "{} second defect escaped",
                case_name(&case)
            );
            count += 1;
        }
    }
    println!("C1 golden: {count} second-defect injections");
}

#[test]
fn c1_golden_ordered_precedence_probes() {
    // The oracle's deliberately multi-defect probes (contract section 6 order).
    let fixture = golden();
    let base = &fixture["base_request"];
    let probes = [
        (
            json!({}),
            json!({"scope": {"session": "bad"}, "idempotency_key": ""}),
            "InvalidScope",
        ),
        (
            json!({}),
            json!({"idempotency_key": "", "C": -1}),
            "InvalidKey",
        ),
        (
            json!({}),
            json!({"C": -1, "receipt_window_ms": 0}),
            "InvalidClock",
        ),
        (
            json!({}),
            json!({"receipt_window_ms": 0, "inputs": []}),
            "InvalidReceiptWindow",
        ),
        (
            json!({}),
            json!({"inputs": [], "summary": {"id": ""}}),
            "InvalidBundle",
        ),
        (
            json!({}),
            json!({"summary": {"id": "", "content": ""}}),
            "InvalidIdentity",
        ),
        (
            json!({}),
            json!({"summary": {"content": "", "importance": 2}}),
            "InvalidContent",
        ),
        (
            json!({}),
            json!({"summary": {"importance": 2, "observed_at_ms": true}}),
            "InvalidImportance",
        ),
        (
            json!({}),
            json!({"summary": {"observed_at_ms": true, "expiry_ms": -1}}),
            "InvalidObservation",
        ),
        (
            json!({}),
            json!({"summary": {"expiry_ms": -1}, "transformation": ""}),
            "InvalidExpiry",
        ),
        (
            json!({}),
            json!({"transformation": "", "evidence": {"id": ""}}),
            "InvalidProvenance",
        ),
        (
            json!({}),
            json!({"evidence": {"id": "", "scope": {}}}),
            "InvalidEvidence",
        ),
        (
            json!({}),
            json!({"deadline": 10, "summary": {"observed_at_ms": 11, "expiry_ms": 0}}),
            "DeadlineExceeded",
        ),
        (
            json!({}),
            json!({"summary": {"observed_at_ms": 11, "expiry_ms": 0}}),
            "FutureObservation",
        ),
        (
            json!({"sources": []}),
            json!({"summary": {"expiry_ms": 10}}),
            "MissingMemory",
        ),
        (
            json!({"sources": [{"id": "m", "malformed": true}]}),
            json!({"summary": {"expiry_ms": 10}}),
            "MissingMemory",
        ),
        (
            json!({"sources": [{"id": "m", "kind": "fact", "revision": 2}]}),
            json!({}),
            "UnsupportedKind",
        ),
        (
            json!({"sources": [{"id": "m", "malformed": true, "kind": "fact"}]}),
            json!({}),
            "InvalidBundle",
        ),
        (
            json!({"sources": [{"id": "m", "malformed": true, "expiry_ms": 10}]}),
            json!({}),
            "MissingMemory",
        ),
    ];
    for (setup, request, expected) in &probes {
        let case = json!({"name": expected, "setup": setup,
                          "steps": [{"request": request, "expect": {}}]});
        let actual = run_case(&case, base).remove(0);
        assert_eq!(actual, json!({"error": expected}), "precedence {request}");
    }
    assert_eq!(probes.len(), 19);
    println!("C1 golden: {} ordered precedence probes", probes.len());
}

#[test]
fn c1_outcome_decoder_and_decimal_boundaries() {
    let digest_text = format!("v1:{}", "0".repeat(64));
    let id = "61".repeat(256);
    let valid = format!("consolidate:v1\n{id}\n1\n256\n{digest_text}\n");
    assert_eq!(valid.len(), 602);
    let decoded = decode_outcome(&valid).unwrap();
    assert_eq!((decoded.id.len(), decoded.count), (256, 256));
    let invalid = [
        valid.replace("\n256\n", "\n257\n"),
        valid.replace("\n256\n", "\n0\n"),
        valid.replace("\n256\n", "\n+1\n"),
        valid.replace(&id, &"61".repeat(257)),
        valid.replace(&id, ""),
        valid.replace(&id, "a"),
        valid.replace("\n1\n", "\n01\n"),
        valid.replace(&digest_text, &format!("v1:{}", "A".repeat(64))),
        valid.replace("consolidate:v1", "consolidate:v2"),
        format!("{valid}x"),
    ];
    for value in &invalid {
        assert!(matches!(
            decode_outcome(value),
            Err(MemoryError::InvalidBundle)
        ));
    }
    for (value, expected) in [
        ("1", Some(1)),
        ("18446744073709551615", Some(u64::MAX)),
        ("18446744073709551616", None),
        ("0", None),
        ("0110", None),
        ("+110", None),
        ("110 ", None),
        ("", None),
    ] {
        assert_eq!(canonical_decimal(value, 20), expected, "{value:?}");
    }
}
