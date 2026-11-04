//! Replays the independent write corpus (tests/fixtures/graph_subset/
//! write-cases.json, produced by tests/graph_subset_write_oracle.py) against
//! the private write executor over a real `GraphState`.

use crate::property_graph::engine::GraphError;
use crate::property_graph::engine::GraphState;
use crate::property_graph::engine::codec::{GraphBody, GraphEntry, TemporalEntry};
use crate::property_graph::engine::model::{
    EdgeId, GraphId, NodeId, Precondition, Properties, PropertyValue,
};
use crate::property_graph::engine::schema::{
    GraphSchema, LabelDefinition, Multiplicity, PropertyDefinition, PropertyType,
    RelationshipDefinition,
};
use crate::property_graph::engine::subset::eval::OutputValue;
use crate::property_graph::engine::subset::meter::{MeterClock, ReadBounds};
use crate::property_graph::engine::subset::plan::ParameterEntry;
use crate::property_graph::engine::subset::write::{StagedWrite, WriteError, execute};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

const FIXTURE: &str = include_str!("../../tests/fixtures/graph_subset/write-cases.json");

const NOT_MODELED: [&str; 4] = [
    "cancellation-execution-not-modeled",
    "deadline-execution-not-modeled",
    "interval-cancellation-not-modeled",
    "commit-sequence-exhaustion-not-modeled",
];

struct FakeClock {
    value: Value,
}

impl MeterClock for FakeClock {
    fn elapsed_ms(&self, phase: &'static str, work: u64) -> u64 {
        let mut elapsed = self
            .value
            .get("elapsed_before_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if let Some(samples) = self.value.get("monotonic").and_then(Value::as_array) {
            for sample in samples {
                if sample["at_work"].as_u64().unwrap_or(u64::MAX) <= work {
                    elapsed = elapsed.max(sample["elapsed_ms"].as_u64().unwrap_or(0));
                }
            }
        }
        elapsed.max(
            self.value
                .get("elapsed_by_phase")
                .and_then(|phases| phases.get(phase))
                .and_then(Value::as_u64)
                .unwrap_or(0),
        )
    }

    fn cancelled(&self, phase: &'static str, work: u64) -> bool {
        self.value
            .get("cancelled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || self
                .value
                .get("cancel_at_work")
                .and_then(Value::as_u64)
                .is_some_and(|at| work >= at)
            || self
                .value
                .get("cancel_phases")
                .and_then(Value::as_array)
                .is_some_and(|phases| phases.iter().any(|value| value == phase))
    }
}

enum Outcome {
    Compared,
    NotModeled,
}

#[test]
fn all_write_cases_match_the_independent_golden_corpus() {
    let document: Value = serde_json::from_str(FIXTURE).expect("write fixture JSON");
    let graphs = document["graphs"].as_object().expect("graph map");
    let cases = document["cases"].as_array().expect("write cases");
    assert_eq!(cases.len(), 164);
    let mut compared = 0;
    let mut not_modeled = Vec::new();
    let mut failures = Vec::new();
    for case in cases {
        let id = case["id"].as_str().unwrap_or("unknown");
        match assert_case(graphs, case) {
            Ok(Outcome::Compared) => compared += 1,
            Ok(Outcome::NotModeled) => not_modeled.push(id.to_owned()),
            Err(difference) => failures.push(format!("{id}: {difference}")),
        }
    }
    eprintln!(
        "write fixture comparison: {compared} compared, {} not modeled, {} failed",
        not_modeled.len(),
        failures.len()
    );
    for failure in &failures {
        eprintln!("FAILED: {failure}");
    }
    assert!(failures.is_empty(), "{} write cases failed", failures.len());
    assert_eq!(compared, 160);
    assert_eq!(not_modeled, NOT_MODELED);
}

/// One corpus case: `Err` carries the first difference.
fn assert_case(graphs: &Map<String, Value>, case: &Value) -> Result<Outcome, String> {
    let id = case["id"].as_str().unwrap_or("unknown");
    let input = expand_graph(&case["graph"], graphs);
    let text = expand_text(&case["text"]);
    let empty = json!({});
    let clock_json = case.get("clock").unwrap_or(&empty).clone();
    let ttl_clock_ms = clock_json
        .get("ttl_clock_ms")
        .unwrap_or(&input["ttl_clock_ms"])
        .clone();
    let (mut state, collections) = graph_state(&input);
    let before = render_graph(&state, &input, &collections);
    if before != input {
        return Err(format!(
            "fixture graph does not round-trip: {before} != {input}"
        ));
    }
    let actual = run(
        &mut state,
        &input,
        &text,
        &case["params"],
        case.get("limits").unwrap_or(&empty),
        clock_json,
        decimal(&ttl_clock_ms),
    );
    let spec = &case["expect"];
    if spec.get("not_modeled").is_some() {
        return not_modeled(id, actual, &state, &input, &collections);
    }
    let actual = actual_json(actual, &mut state, &input, &collections);
    let expected = expected_json(spec, &input, &ttl_clock_ms, graphs);
    if exact_equal(&actual, &expected) {
        Ok(Outcome::Compared)
    } else {
        Err(difference(&actual, &expected, "$"))
    }
}

fn run(
    state: &mut GraphState,
    input: &Value,
    text: &str,
    params: &Value,
    limits: &Value,
    clock: Value,
    ttl_clock_ms: u64,
) -> Result<Option<StagedWrite>, WriteError> {
    let parameters = parameters(text, params).expect("corpus parameters fit ParameterEntry");
    let bounds = bounds(limits).map_err(WriteError::Graph)?;
    let clock = FakeClock { value: clock };
    execute(
        state,
        GraphId(0),
        decimal(&input["view_sequence"]),
        input["temporal_capable"].as_bool().expect("temporal flag"),
        ttl_clock_ms,
        text,
        &parameters,
        bounds,
        &clock,
    )
}

/// Execution controls and sequence exhaustion are outside the oracle; assert
/// the Rust behaviour instead and that nothing changed.
fn not_modeled(
    id: &str,
    actual: Result<Option<StagedWrite>, WriteError>,
    state: &GraphState,
    input: &Value,
    collections: &[String],
) -> Result<Outcome, String> {
    let observed = match &actual {
        Err(WriteError::Graph(GraphError::InvalidQuery(reason))) => reason.to_string(),
        Err(WriteError::SequenceExhausted) => "SequenceExhausted".into(),
        Ok(Some(staged)) => format!("staged {} ops", staged.result.counters.operations),
        other => format!("{other:?}"),
    };
    let wanted = match id {
        "cancellation-execution-not-modeled" => "subset cancelled",
        "deadline-execution-not-modeled" => "subset deadline exceeded",
        // MATCH by key plus one SET charges fewer than 64 work units, so the
        // interval check never runs: the statement plans normally.
        "interval-cancellation-not-modeled" => "staged 1 ops",
        "commit-sequence-exhaustion-not-modeled" => "SequenceExhausted",
        _ => return Err(format!("unexpected NOT_MODELED case {id}")),
    };
    if observed != wanted {
        return Err(format!(
            "not-modeled behaviour {observed:?}, wanted {wanted:?}"
        ));
    }
    if render_graph(state, input, collections) != *input {
        return Err("not-modeled case changed the graph".into());
    }
    Ok(Outcome::NotModeled)
}

fn actual_json(
    actual: Result<Option<StagedWrite>, WriteError>,
    state: &mut GraphState,
    input: &Value,
    collections: &[String],
) -> Value {
    match actual {
        Ok(None) => json!({"outcome": "NOT_MODELED"}),
        Ok(Some(staged)) => {
            let result = &staged.result;
            let operations = operations_json(&staged);
            let mut frames = 0;
            if let Some(revision) = result.revision {
                if let Err(error) = state.apply(revision, &staged.body) {
                    return json!({"outcome": "apply failed", "error": format!("{error:?}")});
                }
                frames = 1;
            }
            json!({
                "outcome": "result",
                "columns": result.columns,
                "rows": result.rows.iter().map(|row| row.iter().map(output_value).collect::<Vec<_>>()).collect::<Vec<_>>(),
                "ttl_clock_ms": result.ttl_clock_ms.to_string(),
                "as_of_ms": result.as_of_ms.map(|value| value.to_string()),
                "view_sequence": result.view_sequence.to_string(),
                "revision": result.revision.map(|value| value.to_string()),
                "operations": operations,
                "commit_frames": frames,
                "counters": {
                    "operations": result.counters.operations,
                    "body_bytes": result.counters.body_bytes,
                    "reclaim_entries": result.counters.reclaim_entries,
                    "binding_rows": result.counters.binding_rows,
                    "detach_edges_examined": result.counters.detach_edges_examined
                },
                "graph": render_graph(state, input, collections)
            })
        }
        Err(error) => json!({
            "outcome": "reject",
            "error": write_error_json(error),
            "operations": [],
            "commit_frames": 0,
            "graph": render_graph(state, input, collections)
        }),
    }
}

fn expected_json(spec: &Value, input: &Value, ttl: &Value, graphs: &Map<String, Value>) -> Value {
    if let Some(error) = spec.get("error") {
        return json!({
            "outcome": "reject",
            "error": error,
            "operations": [],
            "commit_frames": 0,
            "graph": input
        });
    }
    let graph = expand_graph(&spec["graph"], graphs);
    let operations = expand_data(&spec["operations"], None);
    let any = !operations.as_array().expect("operations").is_empty();
    json!({
        "outcome": "result",
        "columns": spec.get("columns").cloned().unwrap_or_else(|| json!([])),
        "rows": spec.get("rows").cloned().unwrap_or_else(|| json!([])),
        "ttl_clock_ms": ttl,
        "as_of_ms": if input["temporal_capable"] == json!(true) { ttl.clone() } else { Value::Null },
        "view_sequence": input["view_sequence"],
        "revision": if any { graph["view_sequence"].clone() } else { Value::Null },
        "operations": operations,
        "commit_frames": u64::from(any),
        "counters": {
            "operations": operations.as_array().unwrap().len(),
            "body_bytes": spec["body_bytes"],
            "reclaim_entries": spec.get("reclaim_entries").cloned().unwrap_or_else(|| json!(0)),
            "binding_rows": spec.get("binding_rows").cloned().unwrap_or_else(|| json!(1)),
            "detach_edges_examined": spec.get("detach_edges_examined").cloned().unwrap_or_else(|| json!(0))
        },
        "graph": graph
    })
}

fn write_error_json(error: WriteError) -> Value {
    match error {
        WriteError::Query(error) => json!({
            "kind": language_kind(error.kind),
            "offset": error.position
        }),
        WriteError::Metadata(error) => {
            let mut diagnostic = error_json(&error);
            diagnostic["offset"] = json!(0);
            diagnostic
        }
        WriteError::Graph(error) => error_json(&error),
        WriteError::SequenceExhausted => json!({"kind": "SequenceExhausted"}),
    }
}

const REASONS: [&str; 16] = [
    "subset visited-node budget exhausted",
    "subset edge budget exhausted",
    "subset degree budget exhausted",
    "subset deadline exceeded",
    "subset cancelled",
    "subset frontier budget exhausted",
    "subset binding-row budget exhausted",
    "subset result-row budget exhausted",
    "subset output budget exhausted",
    "subset work budget exhausted",
    "subset scratch budget exhausted",
    "subset comparison type mismatch",
    "subset hop bound exceeds max_hops",
    "subset work limits must be positive",
    "subset binding was deleted",
    "subset temporal mutation unsupported",
];

fn error_json(error: &GraphError) -> Value {
    match error {
        GraphError::LimitExceedsCeiling { .. } => json!({"kind": "GraphLimitExceedsCeiling"}),
        GraphError::InvalidProperties(_) => json!({"kind": "InvalidMetadata"}),
        GraphError::NodeKeyExists(_) => json!({"kind": "GraphKeyExists"}),
        GraphError::EdgeKeyConflict(_) | GraphError::EdgeKeyExists(_) => {
            json!({"kind": "GraphKeyConflict"})
        }
        GraphError::NodeHasEdges { .. } => json!({"kind": "GraphNodeHasEdges"}),
        GraphError::SchemaViolation { .. } => json!({"kind": "GraphSchemaViolation"}),
        GraphError::BatchSize(_) => json!({"kind": "BatchTooLarge"}),
        GraphError::IdSpaceExhausted => json!({"kind": "IdSpaceExhausted"}),
        GraphError::InvalidQuery(reason) if REASONS.contains(reason) => {
            json!({"kind": "InvalidQuery", "reason": reason})
        }
        GraphError::InvalidQuery(_)
        | GraphError::InvalidNodeKey
        | GraphError::InvalidEdgeKey
        | GraphError::InvalidLabel
        | GraphError::TooManyLabels
        | GraphError::InvalidRelationshipType => json!({"kind": "InvalidQuery"}),
        other => json!({"kind": format!("unexpected {other:?}")}),
    }
}

fn language_kind(kind: crate::property_graph::engine::language::LanguageErrorKind) -> &'static str {
    use crate::property_graph::engine::language::LanguageErrorKind::*;
    match kind {
        Syntax => "Syntax",
        Unsupported => "Unsupported",
        Parameter => "Parameter",
        Limit => "Limit",
    }
}

fn expected_json_precondition(expected: Option<Precondition>) -> Value {
    match expected {
        Some(Precondition::Revision(revision)) => json!({"revision": revision.to_string()}),
        Some(Precondition::Any) => json!("any"),
        other => json!(format!("unexpected {other:?}")),
    }
}

fn operations_json(staged: &StagedWrite) -> Value {
    let revision = staged
        .result
        .revision
        .map_or_else(String::new, |value| value.to_string());
    let entries = staged
        .body
        .entries
        .iter()
        .filter(|entry| !matches!(entry, GraphEntry::Reclaim { .. }))
        .collect::<Vec<_>>();
    if entries.len() != staged.expected.len() {
        return json!("operation/precondition length mismatch");
    }
    let operations = entries
        .into_iter()
        .zip(&staged.expected)
        .map(|(entry, expected)| match entry {
            GraphEntry::PutNode {
                node_id,
                key,
                labels,
                properties,
            } => json!({
                "op": "CreateNode",
                "id": node_id.0.to_string(),
                "key": key,
                "labels": labels.iter().collect::<Vec<_>>(),
                "properties": properties_json(properties),
                "revision": revision
            }),
            GraphEntry::PutEdge {
                edge_id,
                source,
                destination,
                relationship,
                key,
                properties,
            } => json!({
                "op": "CreateEdge",
                "id": edge_id.0.to_string(),
                "key": key,
                "source": source.0.to_string(),
                "target": destination.0.to_string(),
                "type": relationship,
                "properties": properties_json(properties),
                "revision": revision
            }),
            GraphEntry::PatchNode {
                node_id,
                add_labels,
                remove_labels,
                set,
                unset,
            } => json!({
                "op": "PatchNode",
                "id": node_id.0.to_string(),
                "set": properties_json(set),
                "unset": unset.iter().collect::<Vec<_>>(),
                "add_labels": add_labels.iter().collect::<Vec<_>>(),
                "remove_labels": remove_labels.iter().collect::<Vec<_>>(),
                "expected": expected_json_precondition(*expected)
            }),
            GraphEntry::PatchEdge {
                edge_id,
                set,
                unset,
            } => json!({
                "op": "PatchEdge",
                "id": edge_id.0.to_string(),
                "set": properties_json(set),
                "unset": unset.iter().collect::<Vec<_>>(),
                "expected": expected_json_precondition(*expected)
            }),
            GraphEntry::DeleteNode { node_id } => json!({
                "op": "DeleteNode",
                "id": node_id.0.to_string(),
                "expected": expected_json_precondition(*expected)
            }),
            GraphEntry::DeleteEdge { edge_id } => json!({
                "op": "DeleteEdge",
                "id": edge_id.0.to_string(),
                "expected": expected_json_precondition(*expected)
            }),
            other => json!(format!("unexpected entry {other:?}")),
        })
        .collect::<Vec<_>>();
    Value::Array(operations)
}

fn output_value(value: &OutputValue) -> Value {
    match value {
        OutputValue::Scalar(None) => json!({"tag": "Null"}),
        OutputValue::Scalar(Some(value)) => property_json(value),
        OutputValue::Node(node) => json!({
            "tag": "Node",
            "id": node.id.0.to_string(),
            "key": node.key,
            "labels": node.labels.iter().collect::<Vec<_>>(),
            "properties": properties_json(&node.properties),
            "revision": node.revision.to_string()
        }),
        OutputValue::Edge { edge, properties } => json!({
            "tag": "Edge",
            "id": edge.id.0.to_string(),
            "key": edge.key,
            "type": edge.relationship,
            "source": edge.source.0.to_string(),
            "target": edge.destination.0.to_string(),
            "properties": properties_json(properties),
            "revision": edge.revision.to_string()
        }),
        OutputValue::Path { nodes, edges } => json!({
            "tag": "Path",
            "length": edges.len(),
            "edge_ids": edges.iter().map(u64::to_string).collect::<Vec<_>>(),
            "node_ids": nodes.iter().map(u64::to_string).collect::<Vec<_>>()
        }),
    }
}

fn properties_json(properties: &Properties) -> Value {
    let mut result = Map::new();
    for (name, value) in properties {
        result.insert(name.clone(), property_json(value));
    }
    Value::Object(result)
}

fn property_json(value: &PropertyValue) -> Value {
    match value {
        PropertyValue::String(value) => json!({"tag": "String", "value": value}),
        PropertyValue::Integer(value) => {
            json!({"tag": "Integer", "value": value.to_string()})
        }
        PropertyValue::Float(value) => json!({"tag": "Float", "value": value}),
        PropertyValue::Boolean(value) => json!({"tag": "Boolean", "value": value}),
        PropertyValue::GeoPoint {
            latitude,
            longitude,
        } => json!({
            "tag": "GeoPoint",
            "latitude": latitude,
            "longitude": longitude
        }),
    }
}

/// Renders the graph in the corpus shape. Capability, clock, records and
/// schema are not graph-state facts the executor can change; they are copied.
fn render_graph(state: &GraphState, input: &Value, collections: &[String]) -> Value {
    let graph = state.graph(GraphId(0)).expect("fixture graph");
    let nodes = graph
        .nodes()
        .map(|node| {
            let mut value = json!({
                "id": node.id.0.to_string(),
                "key": node.key,
                "labels": node.labels.iter().collect::<Vec<_>>(),
                "properties": properties_json(&node.properties),
                "revision": node.revision.to_string()
            });
            if let Some(expiry) = graph.node_expiry(node.id) {
                value["expires_at_ms"] = json!(expiry.to_string());
            }
            value
        })
        .collect::<Vec<_>>();
    let edges = graph
        .edges()
        .map(|edge| {
            let mut value = json!({
                "id": edge.id.0.to_string(),
                "key": edge.key,
                "source": edge.source.0.to_string(),
                "target": edge.destination.0.to_string(),
                "type": edge.relationship,
                "properties": properties_json(&edge.properties),
                "revision": edge.revision.to_string()
            });
            if let Some(expiry) = graph.edge_expiry(edge.id) {
                value["expires_at_ms"] = json!(expiry.to_string());
            }
            if let Some(segments) = graph.segments(edge.id) {
                value["segments"] = segments
                    .iter()
                    .map(|segment| {
                        json!({
                            "from_ms": segment.valid_from.map(|value| value.to_string()),
                            "to_ms": segment.valid_to.map(|value| value.to_string()),
                            "properties": properties_json(&segment.properties)
                        })
                    })
                    .collect();
            }
            value
        })
        .collect::<Vec<_>>();
    let attachments = graph
        .nodes()
        .flat_map(|node| {
            graph.attachments(node.id).map(move |(collection, record)| {
                json!({
                    "node_id": node.id.0.to_string(),
                    "collection": collections[usize::from(collection) - 1],
                    "record_id": record.to_string()
                })
            })
        })
        .collect::<Vec<_>>();
    canonical_graph(json!({
        "nodes": nodes,
        "edges": edges,
        "temporal_capable": input["temporal_capable"],
        "ttl_clock_ms": input["ttl_clock_ms"],
        "view_sequence": state.last_sequence().to_string(),
        "next_node_id": graph.next_node_id().to_string(),
        "next_edge_id": graph.next_edge_id().to_string(),
        "attachments": attachments,
        "records": input["records"],
        "schema": input["schema"]
    }))
}

/// Loads a corpus graph; returns the state and the collection names whose
/// 1-based positions are the collection IDs.
fn graph_state(graph: &Value) -> (GraphState, Vec<String>) {
    let _activation = graph["temporal_capable"]
        .as_bool()
        .unwrap()
        .then(crate::property_graph::temporal::Activation::on);
    let graph_id = GraphId(0);
    let mut entries = vec![GraphEntry::CreateGraph {
        name: "fixture".into(),
    }];
    let schema = schema(&graph["schema"]);
    if !schema.is_empty() {
        entries.push(GraphEntry::PutSchema { schema });
    }
    let mut nodes = graph["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .collect::<Vec<_>>();
    nodes.sort_by_key(|node| decimal(&node["id"]));
    for node in &nodes {
        entries.push(GraphEntry::PutNode {
            node_id: NodeId(decimal(&node["id"])),
            key: node["key"].as_str().unwrap().into(),
            labels: node["labels"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap().to_owned())
                .collect(),
            properties: properties(&node["properties"]),
        });
        if let Some(expiry) = node.get("expires_at_ms") {
            entries.push(GraphEntry::NodeExpiry {
                node_id: NodeId(decimal(&node["id"])),
                expires_at_ms: Some(decimal(expiry)),
            });
        }
    }
    let mut edges = graph["edges"]
        .as_array()
        .expect("edges")
        .iter()
        .collect::<Vec<_>>();
    edges.sort_by_key(|edge| decimal(&edge["id"]));
    for edge in &edges {
        if let Some(segments) = edge.get("segments").and_then(Value::as_array) {
            entries.push(GraphEntry::Temporal(TemporalEntry::PutEdge {
                edge_id: EdgeId(decimal(&edge["id"])),
                revision: decimal(&edge["revision"]),
                source: NodeId(decimal(&edge["source"])),
                destination: NodeId(decimal(&edge["target"])),
                relationship: edge["type"].as_str().unwrap().into(),
                key: edge["key"].as_str().map(str::to_owned),
                segments: segments
                    .iter()
                    .map(|segment| crate::property_graph::engine::temporal::Segment {
                        valid_from: signed_decimal(&segment["from_ms"]),
                        valid_to: signed_decimal(&segment["to_ms"]),
                        properties: properties(&segment["properties"]),
                    })
                    .collect(),
            }));
        } else {
            entries.push(GraphEntry::PutEdge {
                edge_id: EdgeId(decimal(&edge["id"])),
                source: NodeId(decimal(&edge["source"])),
                destination: NodeId(decimal(&edge["target"])),
                relationship: edge["type"].as_str().unwrap().into(),
                key: edge["key"].as_str().map(str::to_owned),
                properties: properties(&edge["properties"]),
            });
        }
        if let Some(expiry) = edge.get("expires_at_ms") {
            entries.push(GraphEntry::EdgeExpiry {
                edge_id: EdgeId(decimal(&edge["id"])),
                expires_at_ms: Some(decimal(expiry)),
            });
        }
    }
    let attachments = graph["attachments"].as_array().expect("attachments");
    let collections = attachments
        .iter()
        .map(|attachment| &attachment["collection"])
        .chain(
            graph["records"]
                .as_array()
                .expect("records")
                .iter()
                .map(|record| &record["collection"]),
        )
        .map(|name| name.as_str().unwrap().to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut attached = attachments
        .iter()
        .map(|attachment| {
            let name = attachment["collection"].as_str().unwrap();
            (
                decimal(&attachment["node_id"]),
                u16::try_from(collections.iter().position(|c| c == name).unwrap() + 1).unwrap(),
                decimal(&attachment["record_id"]),
            )
        })
        .collect::<Vec<_>>();
    attached.sort_unstable();
    for (node_id, collection, record_id) in attached {
        entries.push(GraphEntry::Attach {
            node_id: NodeId(node_id),
            collection,
            record_id,
        });
    }
    entries.push(GraphEntry::GraphHighWater {
        next_node_id: decimal(&graph["next_node_id"]),
        next_edge_id: decimal(&graph["next_edge_id"]),
    });
    let sequence = decimal(&graph["view_sequence"]);
    let mut state = GraphState::from_snapshot(
        sequence,
        entries.chunks(4096).map(|chunk| GraphBody {
            graph_id,
            entries: chunk.to_vec(),
        }),
    )
    .expect("load fixture snapshot");
    state
        .restore_revisions(
            graph_id,
            &nodes
                .iter()
                .map(|node| (decimal(&node["id"]), decimal(&node["revision"])))
                .collect::<Vec<_>>(),
            &edges
                .iter()
                .map(|edge| (decimal(&edge["id"]), decimal(&edge["revision"])))
                .collect::<Vec<_>>(),
        )
        .expect("restore fixture entity revisions");
    (state, collections)
}

fn schema(value: &Value) -> GraphSchema {
    let mut schema = GraphSchema::default();
    for rule in value["nodes"].as_array().expect("node rules") {
        let mut label = LabelDefinition::new("");
        for (name, field) in rule["properties"].as_object().unwrap() {
            let mut definition = PropertyDefinition::new(property_type(&field["type"]));
            if field["required"] == json!(true) {
                definition = definition.required();
            }
            if field["unique"] == json!(true) {
                definition = definition.unique();
            }
            label = label.property(name.clone(), definition);
        }
        schema = schema.label(rule["label"].as_str().unwrap(), label);
    }
    for rule in value["edges"].as_array().expect("edge rules") {
        let mut relationship = RelationshipDefinition::new("");
        for (name, field) in rule["properties"].as_object().unwrap() {
            let mut definition = PropertyDefinition::new(property_type(&field["type"]));
            if field["required"] == json!(true) {
                definition = definition.required();
            }
            relationship = relationship.property(name.clone(), definition);
        }
        for pair in rule["endpoints"].as_array().unwrap() {
            relationship =
                relationship.endpoint(pair[0].as_str().unwrap(), pair[1].as_str().unwrap());
        }
        if rule["unique_triple"] == json!(true) {
            relationship = relationship.multiplicity(Multiplicity::UniqueTriple);
        }
        schema = schema.relationship(rule["type"].as_str().unwrap(), relationship);
    }
    schema
}

fn property_type(value: &Value) -> PropertyType {
    match value.as_str().unwrap() {
        "String" => PropertyType::String,
        "Integer" => PropertyType::Integer,
        "Float" => PropertyType::Float,
        "Boolean" => PropertyType::Boolean,
        "GeoPoint" => PropertyType::GeoPoint,
        other => panic!("unexpected schema type {other}"),
    }
}

/// `expand_graph` of the oracle: names, overlays and `replace`, canonical.
fn expand_graph(spec: &Value, graphs: &Map<String, Value>) -> Value {
    if let Some(name) = spec.as_str() {
        return expand_graph(&graphs[name], graphs);
    }
    let Some(base) = spec.get("base") else {
        return canonical_graph(expand_data(spec, None));
    };
    let mut result = expand_graph(base, graphs);
    for name in ["nodes", "edges"] {
        let drop = expand_data(spec.get(format!("drop_{name}")).unwrap_or(&json!([])), None);
        let put = expand_data(spec.get(format!("put_{name}")).unwrap_or(&json!([])), None);
        let mut ids = drop
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap().to_owned())
            .collect::<BTreeSet<_>>();
        ids.extend(
            put.as_array()
                .unwrap()
                .iter()
                .map(|entity| entity["id"].as_str().unwrap().to_owned()),
        );
        let mut kept = result[name]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entity| !ids.contains(entity["id"].as_str().unwrap()))
            .cloned()
            .collect::<Vec<_>>();
        kept.extend(put.as_array().unwrap().iter().cloned());
        result[name] = Value::Array(kept);
    }
    if let Some(replace) = spec.get("replace").and_then(Value::as_object) {
        for (name, value) in replace {
            result[name] = expand_data(value, None);
        }
    }
    canonical_graph(result)
}

/// `expand_data` of the oracle: `{i}` series and repeated-string values.
fn expand_data(value: &Value, index: Option<u64>) -> Value {
    match value {
        Value::String(text) => match index {
            Some(index) => Value::String(text.replace("{i}", &index.to_string())),
            None => value.clone(),
        },
        Value::Array(items) => {
            let mut result = Vec::new();
            for item in items {
                if let Some(count) = item.get("series").and_then(Value::as_u64) {
                    let start = item["start"].as_u64().unwrap();
                    for i in start..start + count {
                        result.push(expand_data(&item["template"], Some(i)));
                    }
                } else {
                    result.push(expand_data(item, index));
                }
            }
            Value::Array(result)
        }
        Value::Object(map) => {
            let mut result = Map::new();
            for (key, child) in map {
                result.insert(key.clone(), expand_data(child, index));
            }
            if result.get("tag") == Some(&json!("String")) {
                let text = expand_text(&result["value"]);
                result.insert("value".into(), json!(text));
            }
            Value::Object(result)
        }
        _ => value.clone(),
    }
}

fn canonical_graph(mut graph: Value) -> Value {
    for name in ["nodes", "edges"] {
        graph[name]
            .as_array_mut()
            .unwrap()
            .sort_by_key(|entity| decimal(&entity["id"]));
    }
    for node in graph["nodes"].as_array_mut().unwrap() {
        node["labels"]
            .as_array_mut()
            .unwrap()
            .sort_by(|a, b| a.as_str().unwrap().cmp(b.as_str().unwrap()));
    }
    graph["attachments"]
        .as_array_mut()
        .unwrap()
        .sort_by(|a, b| {
            (decimal(&a["node_id"]), a["collection"].as_str().unwrap())
                .cmp(&(decimal(&b["node_id"]), b["collection"].as_str().unwrap()))
        });
    graph["records"].as_array_mut().unwrap().sort_by(|a, b| {
        (a["collection"].as_str().unwrap(), decimal(&a["id"]))
            .cmp(&(b["collection"].as_str().unwrap(), decimal(&b["id"])))
    });
    graph
}

/// The oracle's `exact_equal`: floats compare bit-exactly and integers never
/// equal floats.
fn exact_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.get(key).is_some_and(|other| exact_equal(value, other)))
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| exact_equal(x, y))
        }
        (Value::Number(a), Value::Number(b)) => match (a.as_f64(), b.as_f64()) {
            _ if a.is_f64() != b.is_f64() => false,
            (Some(x), Some(y)) if a.is_f64() => x.to_bits() == y.to_bits(),
            _ => a == b,
        },
        _ => left == right,
    }
}

fn difference(actual: &Value, expected: &Value, path: &str) -> String {
    match (actual, expected) {
        (Value::Object(a), Value::Object(b)) if a.len() == b.len() => {
            for (key, value) in a {
                match b.get(key) {
                    Some(other) if exact_equal(value, other) => {}
                    Some(other) => return difference(value, other, &format!("{path}.{key}")),
                    None => break,
                }
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (index, (x, y)) in a.iter().zip(b).enumerate() {
                if !exact_equal(x, y) {
                    return difference(x, y, &format!("{path}[{index}]"));
                }
            }
        }
        _ => {}
    }
    let mut actual = actual.to_string();
    let mut expected = expected.to_string();
    actual.truncate(600);
    expected.truncate(600);
    format!("{path}: actual {actual} != expected {expected}")
}

fn parameters(text: &str, values: &Value) -> Result<Vec<ParameterEntry>, String> {
    values
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, value)| {
            property(value).map(|value| ParameterEntry {
                name: name.clone(),
                value,
                position: text.find(&format!("${name}")).unwrap_or(0),
            })
        })
        .collect()
}

fn property(value: &Value) -> Result<PropertyValue, String> {
    Ok(match value["tag"].as_str().unwrap() {
        "String" => PropertyValue::String(expand_text(&value["value"])),
        "Integer" => match value["value"].as_str().unwrap().parse() {
            Ok(value) => PropertyValue::Integer(value),
            Err(_) => return Err("integer is outside the typed ParameterEntry i64 domain".into()),
        },
        "Float" => PropertyValue::Float(value["value"].as_f64().unwrap()),
        "Boolean" => PropertyValue::Boolean(value["value"].as_bool().unwrap()),
        "GeoPoint" => PropertyValue::GeoPoint {
            latitude: value["latitude"].as_f64().unwrap(),
            longitude: value["longitude"].as_f64().unwrap(),
        },
        tag => panic!("unexpected property tag {tag}"),
    })
}

fn properties(value: &Value) -> Properties {
    value
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, value)| (name.clone(), property(value).expect("valid graph property")))
        .collect()
}

fn bounds(limits: &Value) -> Result<ReadBounds, GraphError> {
    let names = [
        "max_hops",
        "max_visited_nodes",
        "max_edges_examined",
        "max_edges_per_node",
        "deadline_ms",
        "max_output_nodes",
        "max_output_edges",
        "max_output_paths",
        "max_binding_rows",
        "max_frontier_entries",
        "max_returned_rows",
        "max_work_units",
        "max_scratch_bytes",
        "write_deadline_ms",
    ];
    let values = names.map(|name| limits.get(name).and_then(Value::as_u64));
    ReadBounds::DEFAULT.with_overrides(values)
}

fn expand_text(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    let prefix = value["prefix"].as_str().unwrap();
    let unit = value["unit"].as_str().unwrap();
    let count = value["count"].as_u64().unwrap() as usize;
    let suffix = value["suffix"].as_str().unwrap();
    format!("{prefix}{}{suffix}", unit.repeat(count))
}

fn decimal(value: &Value) -> u64 {
    value.as_str().unwrap().parse().unwrap()
}

fn signed_decimal(value: &Value) -> Option<i64> {
    value.as_str().map(|value| value.parse().unwrap())
}
