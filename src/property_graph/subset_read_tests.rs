use crate::property_graph::engine::GraphError;
use crate::property_graph::engine::codec::{GraphBody, GraphEntry, TemporalEntry};
use crate::property_graph::engine::model::{EdgeId, GraphId, NodeId, Properties, PropertyValue};
use crate::property_graph::engine::subset::eval::OutputValue;
use crate::property_graph::engine::subset::meter::{MeterClock, ReadBounds};
use crate::property_graph::engine::subset::plan::ParameterEntry;
use crate::property_graph::engine::subset::read::{ReadError, ReadResult, execute};
use crate::property_graph::engine::{Graph, GraphState};
use serde_json::{Map, Value, json};

const FIXTURE: &str = include_str!("../../tests/fixtures/graph_subset/read-cases.json");

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

#[test]
fn all_read_cases_match_the_independent_golden_corpus() {
    let document: Value = serde_json::from_str(FIXTURE).expect("read fixture JSON");
    let graphs = document["graphs"].as_object().expect("graph map");
    let cases = document["cases"].as_array().expect("read cases");
    assert_eq!(cases.len(), 218);
    let mut compared = 0;
    let mut not_compared = Vec::new();
    for case in cases {
        match assert_case(graphs, case) {
            Ok(()) => compared += 1,
            Err(reason) => {
                let id = case["id"].as_str().unwrap_or("unknown");
                eprintln!("NOT COMPARED: {id}: {reason}");
                not_compared.push(id.to_owned());
            }
        }
    }
    assert_eq!(compared, 217);
    assert_eq!(not_compared, ["read-id-above-i64"]);
    eprintln!(
        "read fixture comparison: {compared} compared, {} not compared",
        not_compared.len()
    );
}

#[test]
fn expired_endpoints_are_not_traversed() {
    assert_fixture_case("read-expired-source");
}

#[test]
fn endpoint_anchors_are_enforced() {
    assert_fixture_case("read-endpoint-anchor");
}

#[test]
fn spaced_hop_bounds_report_the_upper_numeral() {
    let document: Value = serde_json::from_str(FIXTURE).unwrap();
    let graphs = document["graphs"].as_object().unwrap();
    let baseline = document["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["id"] == "read-hop-plus-one")
        .unwrap();
    for bound in ["*1 ..3", "* 1 .. 3", "*1\t..\n3"] {
        let mut case = baseline.clone();
        let text = baseline["text"].as_str().unwrap().replace("*1..3", bound);
        assert!(text.contains(bound));
        case["expect"]["error"]["offset"] = json!(text.find('3').unwrap());
        case["text"] = json!(text);
        assert_case(graphs, &case).unwrap();
    }
}

#[test]
fn frontier_exhaustion_returns_no_partial_rows() {
    let document: Value = serde_json::from_str(FIXTURE).unwrap();
    let graphs = document["graphs"].as_object().unwrap();
    let mut case = document["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["id"] == "read-parallel")
        .unwrap()
        .clone();
    case["limits"] = json!({"max_frontier_entries":1});
    case["expect"] = json!({"outcome":"reject","error":{
        "kind":"InvalidQuery","reason":"subset frontier budget exhausted"}});
    assert_case(graphs, &case).unwrap();
}

#[test]
fn execute_validates_unvalidated_bounds_before_any_clock_or_graph_work() {
    let document: Value = serde_json::from_str(FIXTURE).unwrap();
    let graph = graph_from_fixture(&document["graphs"]["empty"]);
    let clock = FakeClock {
        value: json!({"cancelled":true}),
    };
    let mut bounds = ReadBounds::DEFAULT;
    bounds.max_scratch_bytes = u64::MAX;
    assert!(matches!(
        execute(
            &graph,
            7,
            false,
            100,
            "MATCH (n) RETURN n",
            &[],
            bounds,
            &clock,
            fixture_as_of
        ),
        Err(ReadError::Graph(GraphError::LimitExceedsCeiling {
            limit: "max_scratch_bytes",
            ..
        }))
    ));
}

#[test]
fn real_database_subset_adapter_uses_one_view_and_temporal_capability() {
    use crate::property_graph::read::GraphTextError;
    use crate::property_graph::subset::SubsetLimits;
    use crate::property_graph::temporal::{Activation, enable_temporal};
    use crate::{Database, Error, Metadata};
    let activation = Activation::on();
    let dir = tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target")).unwrap();
    let source = dir.path().join("subset-source.vlite");
    let mut db = Database::create(&source, 2).unwrap();
    db.create_graph("g").unwrap();
    db.graph_mut("g")
        .unwrap()
        .create_node("n", &["N"], &Metadata::new())
        .unwrap();
    let parameters = [ParameterEntry {
        name: "t".into(),
        value: PropertyValue::Integer(0),
        position: 16,
    }];
    let graph = db.graph("g").unwrap();
    let result = graph
        .read_subset("MATCH (n) RETURN n", &[], SubsetLimits::DEFAULT, None)
        .unwrap()
        .unwrap();
    assert_eq!(result.view_sequence, graph.view_sequence());
    assert_eq!(result.rows.len(), 1);
    let error = graph
        .read_subset(
            "MATCH (n) AS OF $t RETURN n",
            &parameters,
            SubsetLimits::DEFAULT,
            None,
        )
        .unwrap_err();
    assert!(matches!(
        error.error,
        GraphTextError::Read(Error::UnsupportedFeature(64))
    ));
    let mut limits = SubsetLimits::DEFAULT;
    limits.max_binding_rows = 0;
    assert!(matches!(
        graph
            .read_subset("MATCH (n) RETURN n", &[], limits, None)
            .unwrap_err()
            .error,
        GraphTextError::Read(Error::InvalidQuery("subset work limits must be positive"))
    ));
    db.close().unwrap();
    let temporal = dir.path().join("subset-temporal.vlite");
    enable_temporal(&source, &temporal, None).unwrap();
    let db = Database::open(&temporal).unwrap();
    let graph = db.graph("g").unwrap();
    let result = graph
        .read_subset(
            "MATCH (n) AS OF $t RETURN n",
            &parameters,
            SubsetLimits::DEFAULT,
            None,
        )
        .unwrap()
        .unwrap();
    assert_eq!(result.as_of_ms, Some(0));
    drop(activation);
    let error = graph
        .read_subset(
            "MATCH (n) AS OF $t RETURN n",
            &parameters,
            SubsetLimits::DEFAULT,
            None,
        )
        .unwrap_err();
    assert!(matches!(
        error.error,
        GraphTextError::Read(Error::UnsupportedFeature(64))
    ));
}

fn assert_fixture_case(case_id: &str) {
    let document: Value = serde_json::from_str(FIXTURE).expect("read fixture JSON");
    let graphs = document["graphs"].as_object().expect("graph map");
    let case = document["cases"]
        .as_array()
        .expect("read cases")
        .iter()
        .find(|case| case["id"] == case_id)
        .unwrap_or_else(|| panic!("missing fixture case {case_id}"));
    assert_case(graphs, case).expect("case is representable by the typed executor");
}

fn assert_case(graphs: &serde_json::Map<String, Value>, case: &Value) -> Result<(), String> {
    let graph_json = match case["graph"].as_str() {
        Some(name) => &graphs[name],
        None => &case["graph"],
    };
    let text = expand_text(&case["text"]);
    let parameters = parameters(&text, &case["params"])?;
    let graph = graph_from_fixture(graph_json);
    let bounds = bounds(&case["limits"]);
    let clock_json = case["clock"].clone();
    let clock = FakeClock { value: clock_json };
    let ttl_clock_ms = case["clock"]
        .get("ttl_clock_ms")
        .and_then(Value::as_str)
        .map(|value| value.parse().expect("u64 clock"))
        .unwrap_or_else(|| {
            graph_json["ttl_clock_ms"]
                .as_str()
                .expect("fixture TTL clock")
                .parse()
                .expect("u64 clock")
        });
    let temporal_capable = graph_json["temporal_capable"]
        .as_bool()
        .expect("temporal capability");
    let actual = match bounds {
        Ok(bounds) => execute(
            &graph,
            graph_json["view_sequence"]
                .as_str()
                .expect("view sequence")
                .parse()
                .expect("u64 sequence"),
            temporal_capable,
            ttl_clock_ms,
            &text,
            &parameters,
            bounds,
            &clock,
            fixture_as_of,
        ),
        Err(error) => Err(ReadError::Graph(error)),
    };
    assert_eq!(
        result_json(actual),
        case["expect"],
        "read fixture case {}",
        case["id"].as_str().unwrap_or("unknown")
    );
    Ok(())
}

fn graph_from_fixture(graph_json: &Value) -> Graph {
    let state = graph_state(graph_json);
    state.graph(GraphId(0)).expect("fixture graph").clone()
}

fn fixture_as_of(
    as_of: Option<i64>,
    ttl_clock_ms: u64,
    temporal_capable: bool,
) -> Result<Option<i64>, ReadError> {
    if as_of.is_some() && !temporal_capable {
        return Err(ReadError::UnsupportedFeature(64));
    }
    if temporal_capable {
        match as_of {
            Some(value) => Ok(Some(value)),
            None => i64::try_from(ttl_clock_ms).map(Some).map_err(|_| {
                ReadError::Graph(GraphError::InvalidQuery(
                    "TTL clock does not fit a default validity instant; specify as_of",
                ))
            }),
        }
    } else {
        Ok(None)
    }
}

fn result_json(result: Result<Option<ReadResult>, ReadError>) -> Value {
    match result {
        Ok(None) => json!({"outcome": "NOT_MODELED"}),
        Ok(Some(result)) => json!({
            "outcome": "result",
            "columns": result.columns,
            "rows": result.rows.iter().map(|row| row.iter().map(output_value).collect::<Vec<_>>()).collect::<Vec<_>>(),
            "as_of_ms": result.as_of_ms.map(|value| value.to_string()),
            "ttl_clock_ms": result.ttl_clock_ms.to_string(),
            "view_sequence": result.view_sequence.to_string()
        }),
        Err(ReadError::Query(error)) => json!({
            "outcome": "reject",
            "error": {
                "kind": language_kind(error.kind),
                "offset": error.position
            }
        }),
        Err(ReadError::At { error, position }) => {
            let mut diagnostic = error_json(error);
            diagnostic["offset"] = json!(position);
            json!({"outcome": "reject", "error": diagnostic})
        }
        Err(ReadError::Graph(error)) => {
            json!({"outcome": "reject", "error": error_json(error)})
        }
        Err(ReadError::Runtime(reason)) => json!({
            "outcome": "reject",
            "error": {"kind": "InvalidQuery", "reason": reason}
        }),
        Err(ReadError::UnsupportedFeature(feature)) => json!({
            "outcome": "reject",
            "error": {"kind": "UnsupportedFeature", "feature": feature}
        }),
    }
}

fn error_json(error: GraphError) -> Value {
    match error {
        GraphError::LimitExceedsCeiling { .. } => {
            json!({"kind": "GraphLimitExceedsCeiling"})
        }
        GraphError::InvalidProperties(_) => json!({"kind": "InvalidMetadata"}),
        GraphError::InvalidQuery(reason)
            if reason.starts_with("subset ")
                && matches!(
                    reason,
                    "subset visited-node budget exhausted"
                        | "subset edge budget exhausted"
                        | "subset degree budget exhausted"
                        | "subset deadline exceeded"
                        | "subset cancelled"
                        | "subset frontier budget exhausted"
                        | "subset binding-row budget exhausted"
                        | "subset result-row budget exhausted"
                        | "subset output budget exhausted"
                        | "subset work budget exhausted"
                        | "subset scratch budget exhausted"
                        | "subset comparison type mismatch"
                        | "subset hop bound exceeds max_hops"
                        | "subset work limits must be positive"
                        | "subset binding was deleted"
                        | "subset temporal mutation unsupported"
                ) =>
        {
            json!({"kind": "InvalidQuery", "reason": reason})
        }
        GraphError::InvalidQuery(_) | GraphError::InvalidNodeKey | GraphError::InvalidEdgeKey => {
            json!({"kind": "InvalidQuery"})
        }
        other => panic!("unexpected read error: {other:?}"),
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

fn graph_state(graph: &Value) -> GraphState {
    let _activation = graph["temporal_capable"]
        .as_bool()
        .unwrap()
        .then(crate::property_graph::temporal::Activation::on);
    let graph_id = GraphId(0);
    let mut entries = vec![GraphEntry::CreateGraph {
        name: "fixture".into(),
    }];
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
        if let Some(expiry) = node.get("expires_at_ms").and_then(Value::as_str) {
            entries.push(GraphEntry::NodeExpiry {
                node_id: NodeId(decimal(&node["id"])),
                expires_at_ms: Some(expiry.parse().expect("node expiry")),
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
        if let Some(expiry) = edge.get("expires_at_ms").and_then(Value::as_str) {
            entries.push(GraphEntry::EdgeExpiry {
                edge_id: EdgeId(decimal(&edge["id"])),
                expires_at_ms: Some(expiry.parse().expect("edge expiry")),
            });
        }
    }
    let next_node_id = nodes
        .last()
        .map_or(0, |node| decimal(&node["id"]).checked_add(1).unwrap());
    let next_edge_id = edges
        .last()
        .map_or(0, |edge| decimal(&edge["id"]).checked_add(1).unwrap());
    entries.push(GraphEntry::GraphHighWater {
        next_node_id,
        next_edge_id,
    });
    let sequence = decimal(&graph["view_sequence"]);
    let mut state = GraphState::from_snapshot(sequence, [GraphBody { graph_id, entries }])
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
    state
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
            Err(_) => return Err(
                "integer is outside the typed ParameterEntry i64 domain; caller envelope validation owns this case"
                    .into(),
            ),
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
