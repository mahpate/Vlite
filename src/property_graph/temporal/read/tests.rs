use std::fs;

use serde_json::Value;

use super::super::{Activation, enable_temporal};
use crate::property_graph::retrieve::{EvidenceSpec, Expansion, Pivot, Retrieve};
use crate::property_graph::{
    Direction, EdgeRef, FindNodes, GraphLimits, GraphOp, GraphOutput, GraphQuery, Neighbors,
    NodeSelector, PathOutcome, PropertyProjection, ReadOptions, ShortestPath, TemporalOp,
    TemporalSegment, Traverse, TruncationReason,
};
use crate::{
    Condition, CreateOptions, Database, EncryptionKey, Error, Expected, Metadata, MetadataValue,
};
use std::collections::HashMap;

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/temporal_intervals/golden.json"
);
const READ_CASES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/temporal_intervals/read-cases.json"
);

fn properties(value: i64) -> Metadata {
    Metadata::from([("v".into(), MetadataValue::Integer(value))])
}

fn temporal_db(dir: &std::path::Path, name: &str) -> (std::path::PathBuf, Database) {
    let source = dir.join(format!("{name}.source"));
    let mut db = Database::create(&source, 2).unwrap();
    db.create_graph("g").unwrap();
    {
        let mut graph = db.graph_mut("g").unwrap();
        for key in ["a", "b", "c", "d", "e", "f", "g", "h"] {
            graph.create_node(key, &["N"], &Metadata::new()).unwrap();
        }
    }
    db.close().unwrap();
    let path = dir.join(name);
    enable_temporal(&source, &path, None).unwrap();
    (path.clone(), Database::open(path).unwrap())
}

fn create_edge(
    key: &str,
    source: &str,
    relationship: &str,
    target: &str,
    segments: Vec<TemporalSegment>,
) -> GraphOp {
    GraphOp::Temporal(TemporalOp::CreateEdge {
        key: Some(key.into()),
        source: source.into(),
        relationship: relationship.into(),
        destination: target.into(),
        segments,
    })
}

fn segment(from: Option<i64>, to: Option<i64>, value: i64) -> TemporalSegment {
    TemporalSegment {
        valid_from: from,
        valid_to: to,
        properties: properties(value),
    }
}

fn json_property(value: &Value) -> MetadataValue {
    match value {
        Value::String(value) => MetadataValue::String(value.clone()),
        Value::Bool(value) => MetadataValue::Boolean(*value),
        Value::Number(value) => value
            .as_i64()
            .map(MetadataValue::Integer)
            .unwrap_or_else(|| MetadataValue::Float(value.as_f64().unwrap())),
        _ => MetadataValue::String(value.to_string()),
    }
}

fn json_properties(value: &Value) -> Metadata {
    value
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| (key.clone(), json_property(value)))
        .collect()
}

fn fixture_graph(
    dir: &std::path::Path,
    case: &Value,
) -> (Database, HashMap<u64, u64>, HashMap<u64, u64>, u64) {
    let source = dir.join("oracle.source");
    let mut db = Database::create(&source, 2).unwrap();
    db.create_graph("g").unwrap();
    db.close().unwrap();
    let path = dir.join("oracle.vlite");
    enable_temporal(&source, &path, None).unwrap();
    let mut db = Database::open(path).unwrap();
    let input = &case["input"];
    // Shift logical TTL values beyond the writer's wall clock; translate C
    // and deadlines equally so fixture ordering and boundary answers persist.
    let clock_offset = crate::current_time_ms().saturating_add(60_000);
    let mut node_ids = HashMap::new();
    let mut node_keys = HashMap::new();
    {
        let mut graph = db.graph_mut("g").unwrap();
        for (index, node) in input["nodes"].as_array().unwrap().iter().enumerate() {
            let logical = node["id"].as_u64().unwrap();
            let key = format!("fixture-node-{index}");
            graph.create_node(&key, &[], &Metadata::new()).unwrap();
            node_ids.insert(logical, index as u64);
            node_keys.insert(logical, key);
        }
    }

    let mut edge_ids = HashMap::new();
    let mut edge_keys = HashMap::new();
    {
        let mut graph = db.graph_mut("g").unwrap();
        let mut edges = input["edges"]
            .as_array()
            .unwrap()
            .iter()
            .collect::<Vec<_>>();
        edges.sort_by_key(|edge| edge["id"].as_u64().unwrap());
        for (index, edge) in edges.into_iter().enumerate() {
            let logical = edge["id"].as_u64().unwrap();
            let key = format!("fixture-edge-{index}");
            let source = &node_keys[&edge["source"].as_u64().unwrap()];
            let target = &node_keys[&edge["target"].as_u64().unwrap()];
            let operation = if let Some(segments) = edge["segments"].as_array() {
                let segments = segments
                    .iter()
                    .map(|segment| TemporalSegment {
                        valid_from: segment["from"].as_i64(),
                        valid_to: segment["to"].as_i64(),
                        properties: Metadata::new(),
                    })
                    .collect();
                GraphOp::Temporal(TemporalOp::CreateEdge {
                    key: Some(key.clone()),
                    source: source.clone(),
                    relationship: edge["relationship"].as_str().unwrap().into(),
                    destination: target.clone(),
                    segments,
                })
            } else {
                GraphOp::CreateEdge {
                    key: Some(key.clone()),
                    source: source.clone(),
                    relationship: edge["relationship"].as_str().unwrap().into(),
                    destination: target.clone(),
                    properties: Metadata::new(),
                }
            };
            graph.write_batch(&[operation]).unwrap();
            edge_ids.insert(logical, index as u64);
            edge_keys.insert(logical, key);
        }
    }

    let mut expiries = Vec::new();
    for node in input["nodes"].as_array().unwrap() {
        if !node["deadline"].is_null() {
            expiries.push(GraphOp::SetNodeExpiry {
                key: node_keys[&node["id"].as_u64().unwrap()].clone(),
                expires_at_ms: node["deadline"]
                    .as_u64()
                    .map(|deadline| clock_offset.saturating_add(deadline)),
                expected: Expected::Any,
            });
        }
    }
    for edge in input["edges"].as_array().unwrap() {
        if !edge["deadline"].is_null() {
            expiries.push(GraphOp::SetEdgeExpiry {
                edge: EdgeRef::Key(edge_keys[&edge["id"].as_u64().unwrap()].clone()),
                expires_at_ms: edge["deadline"]
                    .as_u64()
                    .map(|deadline| clock_offset.saturating_add(deadline)),
                expected: Expected::Any,
            });
        }
    }
    if !expiries.is_empty() {
        db.graph_mut("g").unwrap().write_batch(&expiries).unwrap();
    }
    (db, node_ids, edge_ids, clock_offset)
}

fn remap(values: &[u64], ids: &HashMap<u64, u64>) -> Vec<u64> {
    let reverse: HashMap<_, _> = ids
        .iter()
        .map(|(logical, actual)| (*actual, *logical))
        .collect();
    values.iter().map(|id| reverse[id]).collect()
}

fn assert_oracle_work<T>(
    read: &crate::property_graph::GraphRead<T>,
    expected: &Value,
    node_ids: &HashMap<u64, u64>,
) {
    assert_eq!(
        read.stats.edges_examined,
        expected["edges_examined"].as_u64().unwrap()
    );
    assert_eq!(
        read.stats.nodes_visited,
        expected["nodes_visited"].as_u64().unwrap()
    );
    let truncation = &expected["truncation"];
    let mut visited = false;
    let mut budget = false;
    let mut degree = Vec::new();
    for reason in &read.truncation {
        match reason {
            TruncationReason::VisitedLimit => visited = true,
            TruncationReason::EdgeLimit => budget = true,
            TruncationReason::DegreeLimit { node_ids: nodes } => {
                degree = remap(nodes, node_ids);
            }
            other => panic!("unexpected oracle truncation: {other:?}"),
        }
    }
    assert_eq!(visited, truncation["visited"].as_bool().unwrap());
    assert_eq!(budget, truncation["budget"].as_bool().unwrap());
    assert_eq!(
        degree,
        truncation["degree"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_u64().unwrap())
            .collect::<Vec<_>>()
    );
}

#[test]
fn retained_golden_interval_queries_match_real_temporal_reads() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let (_path, mut db) = temporal_db(dir.path(), "golden.vlite");
    let document: Value = serde_json::from_slice(&fs::read(GOLDEN).unwrap()).unwrap();
    let cases = document["series"].as_array().unwrap();

    for (index, case) in cases.iter().enumerate() {
        if [&case["edge"], &case["source"], &case["target"]]
            .iter()
            .any(|entity| {
                !entity["retained"].as_bool().unwrap() || !entity["expires_at_ms"].is_null()
            })
        {
            continue;
        }

        let key = format!("golden{index}");
        let segments = case["segments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| TemporalSegment {
                valid_from: item["valid_from"].as_i64(),
                valid_to: item["valid_to"].as_i64(),
                properties: json_properties(&item["properties"]),
            })
            .collect();
        db.graph_mut("g")
            .unwrap()
            .write_batch(&[create_edge(&key, "a", "R", "b", segments)])
            .unwrap();

        for query in case["queries"].as_array().unwrap() {
            let as_of = query["T"].as_i64().unwrap();
            let clock = query["C"].as_u64().unwrap();
            let result = db
                .graph("g")
                .unwrap()
                .as_of(as_of)
                .unwrap()
                .with_clock(clock)
                .get_edge_by_key(&key)
                .unwrap();
            match &query["expected"] {
                Value::Null => assert_eq!(result.read.output, None, "{}", case["name"]),
                expected => {
                    let edge = result.read.output.as_ref().unwrap();
                    assert_eq!(
                        edge.validity,
                        Some((
                            expected["valid_from"].as_i64(),
                            expected["valid_to"].as_i64()
                        )),
                        "{} at T={as_of}",
                        case["name"]
                    );
                    assert_eq!(
                        edge.edge.properties,
                        json_properties(&expected["properties"]),
                        "{} at T={as_of}",
                        case["name"]
                    );
                }
            }
            assert_eq!(result.as_of_ms, as_of);
            assert_eq!(result.ttl_clock_ms, clock);
        }
    }
}

#[test]
fn retained_temporal_read_oracle_cases_match_real_rust_reads() {
    let _on = Activation::on();
    let document: Value = serde_json::from_slice(&fs::read(READ_CASES).unwrap()).unwrap();
    for case in document["cases"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let (db, node_ids, edge_ids, clock_offset) = fixture_graph(dir.path(), case);
        for (query_index, query) in case["input"]["queries"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let name = format!("{} query {query_index}", case["name"].as_str().unwrap());
            let ttl_clock = clock_offset.saturating_add(query["C"].as_u64().unwrap());
            let at = db
                .graph("g")
                .unwrap()
                .as_of(query["T"].as_i64().unwrap())
                .unwrap()
                .with_clock(ttl_clock);
            let expected = &case["expected"][query_index];
            match query["op"].as_str().unwrap() {
                "neighbors" => {
                    let request = Neighbors {
                        node: NodeSelector::Id(node_ids[&query["node"].as_u64().unwrap()]),
                        direction: direction(&query["direction"]),
                        ..Neighbors::new(NodeSelector::Id(0))
                    };
                    let read = at.neighbors(&request, &ReadOptions::default()).unwrap();
                    let actual: Vec<_> = read
                        .read
                        .output
                        .edges
                        .iter()
                        .map(|edge| edge.edge.id)
                        .collect();
                    assert_eq!(
                        remap(&actual, &edge_ids),
                        json_ids(&expected["edges"]),
                        "{name}"
                    );
                    assert_eq!(read.as_of_ms, query["T"].as_i64().unwrap(), "{name}");
                    assert_eq!(read.ttl_clock_ms, ttl_clock, "{name}");
                    assert_eq!(
                        read.read.view_sequence,
                        db.graph("g").unwrap().view_sequence(),
                        "{name}"
                    );
                }
                "traverse" => {
                    let request = Traverse {
                        start: query["start"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|id| NodeSelector::Id(node_ids[&id.as_u64().unwrap()]))
                            .collect(),
                        direction: direction(&query["direction"]),
                        include_paths: true,
                        ..Traverse::new(Vec::new())
                    };
                    let read = at.traverse(&request, &options(query)).unwrap();
                    let output = &read.read.output;
                    assert_eq!(
                        output
                            .nodes
                            .iter()
                            .map(|node| (node.depth, node.node.id))
                            .collect::<Vec<_>>(),
                        expected["nodes"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|node| (
                                node["depth"].as_u64().unwrap(),
                                node_ids[&node["id"].as_u64().unwrap()]
                            ))
                            .collect::<Vec<_>>(),
                        "{name}"
                    );
                    let actual_edges: Vec<_> = output.edges.iter().map(|edge| edge.id).collect();
                    assert_eq!(
                        remap(&actual_edges, &edge_ids),
                        json_ids(&expected["edges"]),
                        "{name}"
                    );
                    let paths = output
                        .paths
                        .iter()
                        .map(|path| (remap(&path.nodes, &node_ids), remap(&path.edges, &edge_ids)))
                        .collect::<Vec<_>>();
                    let expected_paths = expected["paths"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|path| (json_ids(&path["nodes"]), json_ids(&path["edges"])))
                        .collect::<Vec<_>>();
                    assert_eq!(paths, expected_paths, "{name}");
                    assert_oracle_work(&read.read, expected, &node_ids);
                    assert_eq!(read.as_of_ms, query["T"].as_i64().unwrap(), "{name}");
                    assert_eq!(read.ttl_clock_ms, ttl_clock, "{name}");
                    assert_eq!(
                        read.read.view_sequence,
                        db.graph("g").unwrap().view_sequence(),
                        "{name}"
                    );
                }
                "shortest" => {
                    let request = ShortestPath {
                        source: NodeSelector::Id(node_ids[&query["source"].as_u64().unwrap()]),
                        destination: NodeSelector::Id(
                            node_ids[&query["destination"].as_u64().unwrap()],
                        ),
                        direction: direction(&query["direction"]),
                        types: None,
                    };
                    let read = at.shortest_path(&request, &options(query)).unwrap();
                    match (&read.read.output, expected["outcome"].as_str().unwrap()) {
                        (PathOutcome::Found { path, .. }, "found") => assert_eq!(
                            (remap(&path.nodes, &node_ids), remap(&path.edges, &edge_ids)),
                            (
                                json_ids(&expected["path"]["nodes"]),
                                json_ids(&expected["path"]["edges"])
                            ),
                            "{name}"
                        ),
                        (PathOutcome::NoPathWithinHops, "no_path") => {}
                        (PathOutcome::Exhausted { .. }, "exhausted") => {}
                        (actual, expected) => {
                            panic!("{name}: expected {expected}, got {actual:?}")
                        }
                    }
                    assert_oracle_work(&read.read, expected, &node_ids);
                    assert_eq!(read.as_of_ms, query["T"].as_i64().unwrap(), "{name}");
                    assert_eq!(read.ttl_clock_ms, ttl_clock, "{name}");
                    assert_eq!(
                        read.read.view_sequence,
                        db.graph("g").unwrap().view_sequence(),
                        "{name}"
                    );
                }
                operation => panic!("unexpected oracle operation: {operation}"),
            }
        }
        drop(db);
    }
}

fn json_ids(value: &Value) -> Vec<u64> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_u64().unwrap())
        .collect()
}

fn direction(value: &Value) -> Direction {
    match value.as_str().unwrap() {
        "out" => Direction::Out,
        "in" => Direction::In,
        "both" => Direction::Both,
        other => panic!("unexpected direction {other}"),
    }
}

fn options(query: &Value) -> ReadOptions<'static> {
    let limits = GraphLimits {
        max_hops: query["max_hops"].as_u64().unwrap(),
        max_visited_nodes: query["max_visited_nodes"].as_u64().unwrap(),
        max_edges_per_node: query["max_edges_per_node"].as_u64().unwrap(),
        max_edges_examined: query["budget"].as_u64().unwrap(),
        ..GraphLimits::DEFAULT
    };
    ReadOptions {
        limits,
        ..ReadOptions::default()
    }
}

#[test]
fn edge_and_node_gets_share_one_snapshot_and_keep_node_metadata_non_temporal() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let (_path, mut db) = temporal_db(dir.path(), "gets.vlite");
    db.graph_mut("g")
        .unwrap()
        .write_batch(&[
            create_edge("ab", "a", "R", "b", vec![segment(Some(10), Some(20), 1)]),
            create_edge("cd", "c", "R", "d", vec![]),
            GraphOp::CreateEdge {
                key: Some("ef".into()),
                source: "e".into(),
                relationship: "R".into(),
                destination: "f".into(),
                properties: properties(9),
            },
        ])
        .unwrap();
    db.graph_mut("g")
        .unwrap()
        .upsert_node("a", &["Changed"], &properties(77), Expected::Any)
        .unwrap();

    let graph = db.graph("g").unwrap();
    let by_key = graph.as_of(15).unwrap().get_edge_by_key("ab").unwrap();
    let edge_id = by_key.read.output.as_ref().unwrap().edge.id;
    let by_id = graph.as_of(15).unwrap().get_edge(edge_id).unwrap();
    assert_eq!(by_id.read.output, by_key.read.output);
    let temporal_edge = by_key.read.output.as_ref().unwrap();
    assert_eq!(temporal_edge.validity, Some((Some(10), Some(20))));
    assert_eq!(temporal_edge.edge.properties, properties(1));
    assert_eq!(
        by_key.read.view_sequence,
        graph
            .as_of(15)
            .unwrap()
            .get_node("a")
            .unwrap()
            .read
            .view_sequence
    );
    assert_eq!(
        graph
            .as_of(15)
            .unwrap()
            .get_node("a")
            .unwrap()
            .read
            .output
            .as_ref()
            .unwrap()
            .properties,
        properties(77)
    );

    let node_id = graph
        .as_of(15)
        .unwrap()
        .get_node("a")
        .unwrap()
        .read
        .output
        .unwrap()
        .id;
    assert_eq!(
        graph
            .as_of(15)
            .unwrap()
            .get_node_by_id(node_id)
            .unwrap()
            .read
            .output
            .unwrap()
            .key,
        "a"
    );
    assert_eq!(
        graph
            .as_of(0)
            .unwrap()
            .get_edge_by_key("ef")
            .unwrap()
            .read
            .output
            .unwrap()
            .validity,
        None
    );
    assert_eq!(
        graph
            .as_of(0)
            .unwrap()
            .get_edge_by_key("cd")
            .unwrap()
            .read
            .output,
        None
    );

    let ordinary = graph.get_edge_by_key("ef").unwrap().output.unwrap();
    assert_eq!(ordinary.properties, properties(9));
}

#[test]
fn default_time_resolution_separates_validity_from_the_ttl_clock() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let (_path, mut db) = temporal_db(dir.path(), "clocks.vlite");
    db.graph_mut("g")
        .unwrap()
        .write_batch(&[create_edge(
            "ab",
            "a",
            "R",
            "b",
            vec![segment(Some(100), Some(200), 3)],
        )])
        .unwrap();

    let graph = db.graph("g").unwrap();
    let default = graph.with_clock(150).temporal_read().unwrap();
    let read = default.get_edge_by_key("ab").unwrap();
    assert_eq!((read.as_of_ms, read.ttl_clock_ms), (150, 150));
    assert_eq!(
        read.read.output.unwrap().validity,
        Some((Some(100), Some(200)))
    );

    let explicit = graph
        .as_of(i64::MAX)
        .unwrap()
        .with_clock(u64::MAX)
        .get_edge_by_key("ab")
        .unwrap();
    assert_eq!(
        (explicit.as_of_ms, explicit.ttl_clock_ms),
        (i64::MAX, u64::MAX)
    );
    assert_eq!(explicit.read.output, None);
    assert!(matches!(
        graph
            .with_clock(u64::MAX)
            .temporal_read()
            .unwrap()
            .get_edge(1),
        Err(Error::InvalidQuery(_))
    ));
}

#[test]
fn timeline_bounds_slice_all_retained_segments_and_reject_invalid_bounds() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let (_path, mut db) = temporal_db(dir.path(), "timeline.vlite");
    let segments = (0..256).map(|i| segment(Some(i), Some(i + 1), i)).collect();
    db.graph_mut("g")
        .unwrap()
        .write_batch(&[
            create_edge("ab", "a", "R", "b", segments),
            create_edge("cd", "c", "R", "d", vec![]),
            create_edge(
                "gh",
                "g",
                "R",
                "h",
                vec![segment(Some(0), Some(1), 1), segment(Some(1), Some(2), 2)],
            ),
            GraphOp::CreateEdge {
                key: Some("ef".into()),
                source: "e".into(),
                relationship: "R".into(),
                destination: "f".into(),
                properties: properties(1),
            },
        ])
        .unwrap();
    let view = db.graph("g").unwrap().as_of(999).unwrap();

    let all = view.timeline(EdgeRef::Key("ab".into()), 0..256).unwrap();
    let timeline = all.read.output.unwrap();
    assert_eq!(timeline.total_segments, 256);
    assert_eq!(timeline.bounds, 0..256);
    assert_eq!(timeline.segments.len(), 256);
    assert_eq!(timeline.segments[0], segment(Some(0), Some(1), 0));
    assert_eq!(timeline.segments[255], segment(Some(255), Some(256), 255));

    let tail = view
        .timeline(EdgeRef::Id(timeline.edge_id), 250..256)
        .unwrap();
    assert_eq!(tail.read.output.unwrap().segments, timeline.segments[250..]);
    assert_eq!(
        view.timeline(EdgeRef::Key("ab".into()), 256..256)
            .unwrap()
            .read
            .output
            .unwrap()
            .segments,
        Vec::<TemporalSegment>::new()
    );
    assert_eq!(
        view.timeline(EdgeRef::Key("cd".into()), 0..256)
            .unwrap()
            .read
            .output
            .unwrap()
            .total_segments,
        0
    );
    let clamped = view
        .timeline(EdgeRef::Key("gh".into()), 1..256)
        .unwrap()
        .read
        .output
        .unwrap();
    assert_eq!(clamped.bounds, 1..256);
    assert_eq!(clamped.total_segments, 2);
    assert_eq!(clamped.segments, [segment(Some(1), Some(2), 2)]);
    assert!(matches!(
        view.timeline(
            EdgeRef::Key("ab".into()),
            std::ops::Range { start: 3, end: 2 },
        ),
        Err(Error::InvalidQuery(_))
    ));
    assert!(matches!(
        view.timeline(EdgeRef::Key("ab".into()), 0..257),
        Err(Error::InvalidQuery(_))
    ));
    assert!(matches!(
        view.timeline(EdgeRef::Key("ef".into()), 0..1),
        Err(Error::InvalidQuery(_))
    ));
    assert_eq!(
        view.timeline(EdgeRef::Key("ab".into()), 0..256)
            .unwrap()
            .read
            .output
            .unwrap()
            .segments
            .len(),
        256
    );
}

#[test]
fn timeline_reads_all_256_segments_and_failed_cap_writes_leave_reads_unchanged() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let (_path, mut db) = temporal_db(dir.path(), "timeline-cap.vlite");
    let segments: Vec<_> = (0..256)
        .map(|i| segment(Some(i * 2), Some(i * 2 + 1), i))
        .collect();
    db.graph_mut("g")
        .unwrap()
        .write_batch(&[create_edge("ab", "a", "R", "b", segments)])
        .unwrap();

    let view = db.graph("g").unwrap().as_of(1_000).unwrap();
    let initial_timeline = view
        .timeline(EdgeRef::Key("ab".into()), 0..256)
        .unwrap()
        .read
        .output
        .unwrap();
    assert_eq!(initial_timeline.total_segments, 256);
    assert_eq!(initial_timeline.segments[0], segment(Some(0), Some(1), 0));
    assert_eq!(
        initial_timeline.segments[255],
        segment(Some(510), Some(511), 255)
    );

    let edge_at = |at| {
        db.graph("g")
            .unwrap()
            .as_of(at)
            .unwrap()
            .with_clock(0)
            .get_edge_by_key("ab")
            .unwrap()
            .read
            .output
    };
    assert_eq!(edge_at(0).unwrap().validity, Some((Some(0), Some(1))));
    assert!(edge_at(1).is_none());
    assert_eq!(edge_at(510).unwrap().validity, Some((Some(510), Some(511))));
    assert!(edge_at(511).is_none());

    assert!(matches!(
        view.timeline(
            EdgeRef::Key("ab".into()),
            std::ops::Range { start: 2, end: 1 },
        ),
        Err(Error::InvalidQuery(_))
    ));
    assert!(matches!(
        view.timeline(EdgeRef::Key("ab".into()), 0..257),
        Err(Error::InvalidQuery(_))
    ));

    let revision = initial_timeline.revision;
    assert!(matches!(
        db.graph_mut("g")
            .unwrap()
            .write_batch(&[GraphOp::Temporal(TemporalOp::AppendSegment {
                edge: EdgeRef::Key("ab".into()),
                segment: segment(Some(512), Some(513), 256),
                expected: Expected::Revision(revision),
            })]),
        Err(Error::InvalidQuery(_))
    ));

    let unchanged = db
        .graph("g")
        .unwrap()
        .as_of(1_000)
        .unwrap()
        .timeline(EdgeRef::Key("ab".into()), 0..256)
        .unwrap()
        .read
        .output
        .unwrap();
    assert_eq!(unchanged, initial_timeline);
}

#[test]
fn temporal_neighbors_filter_and_project_the_segment_selected_by_as_of() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let (_path, mut db) = temporal_db(dir.path(), "neighbors.vlite");
    db.graph_mut("g")
        .unwrap()
        .write_batch(&[
            create_edge(
                "ab",
                "a",
                "R",
                "b",
                vec![
                    TemporalSegment {
                        valid_from: Some(0),
                        valid_to: Some(10),
                        properties: Metadata::from([
                            ("kind".into(), MetadataValue::String("old".into())),
                            ("keep".into(), MetadataValue::Integer(1)),
                            ("omit".into(), MetadataValue::Boolean(true)),
                        ]),
                    },
                    TemporalSegment {
                        valid_from: Some(10),
                        valid_to: None,
                        properties: Metadata::from([
                            ("kind".into(), MetadataValue::String("new".into())),
                            ("keep".into(), MetadataValue::Integer(2)),
                            ("omit".into(), MetadataValue::Boolean(false)),
                        ]),
                    },
                ],
            ),
            create_edge(
                "ac",
                "a",
                "R",
                "c",
                vec![TemporalSegment {
                    valid_from: None,
                    valid_to: None,
                    properties: Metadata::from([
                        ("kind".into(), MetadataValue::String("new".into())),
                        ("keep".into(), MetadataValue::Integer(3)),
                    ]),
                }],
            ),
        ])
        .unwrap();

    let mut request = Neighbors::new(NodeSelector::key("a"));
    request.direction = Direction::Out;
    request.edge_conditions = vec![Condition::Eq {
        field: "kind".into(),
        value: MetadataValue::String("new".into()),
    }];
    let options = ReadOptions {
        projection: PropertyProjection::Only(vec!["keep".into()]),
        ..ReadOptions::default()
    };
    let old = db
        .graph("g")
        .unwrap()
        .as_of(5)
        .unwrap()
        .neighbors(&request, &options)
        .unwrap();
    assert_eq!(
        old.read
            .output
            .edges
            .iter()
            .map(|edge| edge.edge.key.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["ac"]
    );

    let now = db
        .graph("g")
        .unwrap()
        .as_of(10)
        .unwrap()
        .neighbors(&request, &options)
        .unwrap();
    assert_eq!(
        now.read
            .output
            .edges
            .iter()
            .map(|edge| edge.edge.key.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["ab", "ac"]
    );
    assert_eq!(
        now.read
            .output
            .edges
            .iter()
            .map(|edge| edge.edge.properties.clone())
            .collect::<Vec<_>>(),
        [
            Metadata::from([("keep".into(), MetadataValue::Integer(2))]),
            Metadata::from([("keep".into(), MetadataValue::Integer(3))]),
        ]
    );
    assert_eq!(
        now.read
            .output
            .nodes
            .iter()
            .map(|node| node.key.as_str())
            .collect::<Vec<_>>(),
        ["b", "c"]
    );
}

#[test]
fn ttl_visibility_uses_clock_for_edges_and_both_endpoints_without_resurrection() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let (_path, mut db) = temporal_db(dir.path(), "ttl.vlite");
    let make = |key, src, dst| create_edge(key, src, "R", dst, vec![segment(None, None, 1)]);
    let deadline = crate::current_time_ms() + 2_000;
    db.graph_mut("g")
        .unwrap()
        .write_batch(&[
            make("ab", "a", "b"),
            make("cd", "c", "d"),
            make("ef", "e", "f"),
            GraphOp::SetEdgeExpiry {
                edge: EdgeRef::Key("ab".into()),
                expires_at_ms: Some(deadline),
                expected: Expected::Any,
            },
            GraphOp::SetNodeExpiry {
                key: "c".into(),
                expires_at_ms: Some(deadline),
                expected: Expected::Any,
            },
            GraphOp::SetNodeExpiry {
                key: "f".into(),
                expires_at_ms: Some(deadline),
                expected: Expected::Any,
            },
        ])
        .unwrap();

    for key in ["ab", "cd", "ef"] {
        assert!(
            db.graph("g")
                .unwrap()
                .as_of(-100)
                .unwrap()
                .with_clock(deadline - 1)
                .get_edge_by_key(key)
                .unwrap()
                .read
                .output
                .is_some(),
            "{key} before its deadline"
        );
        for clock in [deadline, deadline + 1] {
            assert!(
                db.graph("g")
                    .unwrap()
                    .as_of(i64::MAX)
                    .unwrap()
                    .with_clock(clock)
                    .get_edge_by_key(key)
                    .unwrap()
                    .read
                    .output
                    .is_none(),
                "{key} at C={clock}"
            );
        }
    }
    assert!(
        db.graph("g")
            .unwrap()
            .as_of(0)
            .unwrap()
            .with_clock(deadline - 1)
            .timeline(EdgeRef::Key("ab".into()), 0..1)
            .unwrap()
            .read
            .output
            .is_some()
    );
    assert!(
        db.graph("g")
            .unwrap()
            .as_of(0)
            .unwrap()
            .with_clock(deadline)
            .timeline(EdgeRef::Key("ab".into()), 0..1)
            .unwrap()
            .read
            .output
            .is_none()
    );

    let read_options = ReadOptions::default();
    let found = db
        .graph("g")
        .unwrap()
        .as_of(100)
        .unwrap()
        .with_clock(deadline)
        .find_nodes(&FindNodes::default(), &read_options)
        .unwrap();
    let visible: Vec<_> = found
        .read
        .output
        .iter()
        .map(|node| node.key.as_str())
        .collect();
    assert!(!visible.contains(&"c") && !visible.contains(&"f"));
    let neighbor = db
        .graph("g")
        .unwrap()
        .as_of(100)
        .unwrap()
        .with_clock(deadline)
        .neighbors(&Neighbors::new(NodeSelector::key("a")), &read_options)
        .unwrap();
    assert!(neighbor.read.output.edges.is_empty());

    std::thread::sleep(std::time::Duration::from_millis(
        deadline.saturating_sub(crate::current_time_ms()) + 5,
    ));
    db.graph_mut("g")
        .unwrap()
        .create_node("x", &["N"], &Metadata::new())
        .unwrap();
    for key in ["ab", "cd", "ef"] {
        assert!(
            db.graph("g")
                .unwrap()
                .as_of(0)
                .unwrap()
                .with_clock(0)
                .get_edge_by_key(key)
                .unwrap()
                .read
                .output
                .is_none(),
            "{key} must remain purged at an earlier clock"
        );
    }
    assert!(
        db.graph("g")
            .unwrap()
            .as_of(0)
            .unwrap()
            .with_clock(0)
            .get_node("c")
            .unwrap()
            .read
            .output
            .is_none()
    );
}

#[test]
fn timeline_obeys_source_and_target_expiry_independently_of_validity_time() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let (_path, mut db) = temporal_db(dir.path(), "timeline-endpoint-ttl.vlite");
    let deadline = crate::current_time_ms() + 60_000;
    db.graph_mut("g")
        .unwrap()
        .write_batch(&[
            create_edge("ab", "a", "R", "b", vec![segment(Some(0), Some(1), 1)]),
            create_edge("cd", "c", "R", "d", vec![segment(Some(0), Some(1), 2)]),
            GraphOp::SetNodeExpiry {
                key: "a".into(),
                expires_at_ms: Some(deadline),
                expected: Expected::Any,
            },
            GraphOp::SetNodeExpiry {
                key: "d".into(),
                expires_at_ms: Some(deadline),
                expected: Expected::Any,
            },
        ])
        .unwrap();

    for key in ["ab", "cd"] {
        let before = db
            .graph("g")
            .unwrap()
            .as_of(100)
            .unwrap()
            .with_clock(deadline - 1)
            .timeline(EdgeRef::Key(key.into()), 0..1)
            .unwrap();
        let timeline = before.read.output.unwrap();
        assert_eq!(timeline.total_segments, 1, "{key}");
        assert_eq!(
            timeline.segments[0].valid_to,
            Some(1),
            "T remains outside validity for {key}"
        );

        for clock in [deadline, deadline + 1] {
            assert!(
                db.graph("g")
                    .unwrap()
                    .as_of(100)
                    .unwrap()
                    .with_clock(clock)
                    .timeline(EdgeRef::Key(key.into()), 0..1)
                    .unwrap()
                    .read
                    .output
                    .is_none(),
                "{key} must be hidden at C={clock} despite timeline ignoring T"
            );
        }
    }
}

#[test]
fn temporal_pages_bind_selectors_continue_and_keep_stale_detection() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let (_path, mut db) = temporal_db(dir.path(), "pages.vlite");
    db.graph_mut("g")
        .unwrap()
        .write_batch(&[
            create_edge("ab", "a", "R", "b", vec![segment(None, None, 1)]),
            create_edge("ac", "a", "R", "c", vec![segment(None, None, 1)]),
        ])
        .unwrap();

    let mut neighbors = Neighbors::new(NodeSelector::key("a"));
    neighbors.limit = Some(1);
    let first = db
        .graph("g")
        .unwrap()
        .as_of(0)
        .unwrap()
        .with_clock(100)
        .neighbors(&neighbors, &ReadOptions::default())
        .unwrap();
    assert!(!first.read.complete);
    let cursor = first
        .read
        .cursor
        .clone()
        .expect("temporal continuation cursor");
    assert_eq!((first.as_of_ms, first.ttl_clock_ms), (0, 100));

    neighbors.cursor = Some(cursor.clone());
    let continued = db
        .graph("g")
        .unwrap()
        .temporal_read()
        .unwrap()
        .neighbors(&neighbors, &ReadOptions::default())
        .unwrap();
    assert!(continued.read.complete);
    assert_eq!((continued.as_of_ms, continued.ttl_clock_ms), (0, 100));
    assert_eq!(continued.read.output.edges.len(), 1);
    assert_eq!(continued.read.view_sequence, first.read.view_sequence);

    neighbors.cursor = None;
    let default_first = db
        .graph("g")
        .unwrap()
        .neighbors(&neighbors, &ReadOptions::default())
        .unwrap();
    let mut default_request = neighbors.clone();
    default_request.cursor = default_first.cursor.clone();
    let default_next = db
        .graph("g")
        .unwrap()
        .neighbors(&default_request, &ReadOptions::default())
        .unwrap();
    assert!(default_next.complete);
    assert_eq!(default_next.view_sequence, default_first.view_sequence);

    let mut query_page = neighbors.clone();
    query_page.cursor = None;
    let generic_first = db
        .graph("g")
        .unwrap()
        .as_of(0)
        .unwrap()
        .with_clock(100)
        .query(
            &GraphQuery::Neighbors(query_page.clone()),
            &ReadOptions {
                limits: GraphLimits {
                    max_output_edges: 1,
                    ..GraphLimits::DEFAULT
                },
                ..ReadOptions::default()
            },
        )
        .unwrap();
    query_page.cursor = generic_first.read.cursor.clone();
    assert!(
        db.graph("g")
            .unwrap()
            .as_of(1)
            .unwrap()
            .query(
                &GraphQuery::Neighbors(query_page.clone()),
                &ReadOptions::default()
            )
            .is_err(),
        "a cursor must reject a conflicting explicit T"
    );
    assert!(matches!(
        db.graph("g")
            .unwrap()
            .with_clock(101)
            .temporal_read()
            .unwrap()
            .query(
                &GraphQuery::Neighbors(query_page.clone()),
                &ReadOptions::default()
            ),
        Err(Error::InvalidQuery(
            "temporal cursor conflicts with the TTL clock"
        ))
    ));

    let mut find = FindNodes {
        limit: Some(1),
        ..FindNodes::default()
    };
    let page = db
        .graph("g")
        .unwrap()
        .as_of(0)
        .unwrap()
        .with_clock(100)
        .find_nodes(&find, &ReadOptions::default())
        .unwrap();
    assert!(!page.read.complete);
    let find_cursor = page.read.cursor.clone().expect("find continuation cursor");
    find.cursor = Some(find_cursor);
    let find_next = db
        .graph("g")
        .unwrap()
        .temporal_read()
        .unwrap()
        .find_nodes(&find, &ReadOptions::default())
        .unwrap();
    assert_eq!((find_next.as_of_ms, find_next.ttl_clock_ms), (0, 100));

    drop(page);
    drop(first);
    drop(continued);
    db.graph_mut("g")
        .unwrap()
        .create_node("changed", &[], &Metadata::new())
        .unwrap();
    assert!(matches!(
        db.graph("g")
            .unwrap()
            .temporal_read()
            .unwrap()
            .find_nodes(&find, &ReadOptions::default()),
        Err(Error::GraphCursorStale)
    ));
}

#[test]
fn temporal_query_text_explain_and_retrieve_inherit_the_read_view() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let (_path, mut db) = temporal_db(dir.path(), "c2-operations.vlite");
    db.put(1, &[0.0, 0.0], b"pivot").unwrap();
    db.graph_mut("g")
        .unwrap()
        .write_batch(&[
            create_edge("ab", "a", "R", "b", vec![segment(Some(0), Some(10), 1)]),
            create_edge("bc", "b", "R", "c", vec![segment(Some(10), Some(20), 2)]),
            create_edge("ad", "a", "R", "d", vec![segment(Some(0), Some(20), 3)]),
            GraphOp::SetNodeExpiry {
                key: "d".into(),
                expires_at_ms: Some(50),
                expected: Expected::Any,
            },
            GraphOp::Attach {
                node: "a".into(),
                collection: "default".into(),
                record: 1,
            },
        ])
        .unwrap();

    let view = db.graph("g").unwrap().as_of(5).unwrap().with_clock(50);
    let limits = GraphLimits::DEFAULT;
    let text = "MATCH p = (a KEY \"a\")-[*1..2]->(b) RETURN b, p";
    let typed_explanation = view
        .explain(
            &GraphQuery::Traverse(Traverse::new(vec![NodeSelector::key("a")])),
            &ReadOptions::default(),
        )
        .unwrap();
    assert_eq!(
        (
            typed_explanation.as_of_ms,
            typed_explanation.ttl_clock_ms,
            typed_explanation.view_sequence
        ),
        (5, 50, db.graph("g").unwrap().view_sequence())
    );
    let explained = view.explain_text(text, &Metadata::new(), &limits).unwrap();
    assert_eq!(
        (
            explained.as_of_ms,
            explained.ttl_clock_ms,
            explained.view_sequence
        ),
        (5, 50, db.graph("g").unwrap().view_sequence())
    );
    assert!(matches!(
        explained.explanation.query,
        GraphQuery::Traverse(_)
    ));

    let queried = view.query_text(text, &Metadata::new(), &limits).unwrap();
    assert_eq!((queried.as_of_ms, queried.ttl_clock_ms), (5, 50));
    let GraphOutput::Traversal(traversal) = queried.read.output else {
        panic!("query text did not execute the explained traversal");
    };
    assert_eq!(
        traversal
            .nodes
            .iter()
            .map(|node| node.node.key.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );

    let mut pivot = Pivot::new("default", 1);
    pivot.vector = Some(vec![0.0, 0.0]);
    let retrieved = view
        .retrieve(
            &Retrieve {
                pivot,
                expansion: Expansion::default(),
                evidence: EvidenceSpec::default(),
            },
            &ReadOptions::default(),
        )
        .unwrap();
    assert_eq!((retrieved.as_of_ms, retrieved.ttl_clock_ms), (5, 50));
    assert_eq!(
        retrieved.retrieval.view_sequence,
        db.graph("g").unwrap().view_sequence()
    );
    assert_eq!(
        retrieved
            .retrieval
            .nodes
            .iter()
            .map(|node| node.node.key.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(
        retrieved
            .retrieval
            .edges
            .iter()
            .map(|edge| (edge.key.as_deref(), edge.properties.clone()))
            .collect::<Vec<_>>(),
        [(Some("ab"), properties(1))]
    );

    let shortest = view
        .shortest_path(
            &ShortestPath::new(NodeSelector::key("a"), NodeSelector::key("c")),
            &ReadOptions {
                limits: GraphLimits {
                    max_hops: 2,
                    ..GraphLimits::DEFAULT
                },
                ..ReadOptions::default()
            },
        )
        .unwrap();
    assert_eq!(shortest.read.output, PathOutcome::NoPathWithinHops);
    assert_eq!((shortest.as_of_ms, shortest.ttl_clock_ms), (5, 50));
}

#[test]
fn temporal_reads_are_gated_and_ordinary_graph_reads_remain_unchanged() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    let (path, mut db) = temporal_db(dir.path(), "gated.vlite");
    db.graph_mut("g")
        .unwrap()
        .write_batch(&[create_edge(
            "ab",
            "a",
            "R",
            "b",
            vec![segment(None, None, 1)],
        )])
        .unwrap();
    {
        let _off = Activation::set(false);
        assert!(matches!(
            db.graph("g").unwrap().as_of(0),
            Err(Error::UnsupportedFeature(_))
        ));
        assert!(matches!(
            db.graph("g").unwrap().temporal_read(),
            Err(Error::UnsupportedFeature(_))
        ));
    }

    let plain = dir.path().join("plain.vlite");
    let mut db = Database::create(&plain, 2).unwrap();
    db.create_graph("g").unwrap();
    {
        let mut graph = db.graph_mut("g").unwrap();
        graph.create_node("a", &[], &Metadata::new()).unwrap();
        graph.create_node("b", &[], &Metadata::new()).unwrap();
        graph
            .create_edge(Some("ab"), "a", "R", "b", &properties(2))
            .unwrap();
    }
    {
        let _off = Activation::set(false);
        assert!(matches!(
            db.graph("g").unwrap().as_of(0),
            Err(Error::UnsupportedFeature(_))
        ));
        assert_eq!(
            db.graph("g")
                .unwrap()
                .get_edge_by_key("ab")
                .unwrap()
                .output
                .unwrap()
                .properties,
            properties(2)
        );
    }
    drop(db);
    assert!(path.exists());
}

#[test]
fn temporal_and_legacy_edges_and_plain_or_encrypted_reopens_are_distinct() {
    let _on = Activation::on();
    let dir = tempfile::tempdir().unwrap();
    for (name, key) in [
        ("plain.vlite", None),
        ("encrypted.vlite", Some(EncryptionKey::from_bytes([7; 32]))),
    ] {
        let source = dir.path().join(format!("{name}.source"));
        let options = CreateOptions {
            encryption: key.clone(),
            ..CreateOptions::default()
        };
        let mut db = Database::create_with_options(&source, 2, options).unwrap();
        db.create_graph("g").unwrap();
        {
            let mut graph = db.graph_mut("g").unwrap();
            for node in ["a", "b", "c", "d"] {
                graph.create_node(node, &["N"], &Metadata::new()).unwrap();
            }
        }
        db.close().unwrap();

        let path = dir.path().join(name);
        enable_temporal(&source, &path, key.as_ref()).unwrap();
        let mut db = if let Some(key) = &key {
            Database::open_encrypted(&path, key).unwrap()
        } else {
            Database::open(&path).unwrap()
        };
        db.graph_mut("g")
            .unwrap()
            .write_batch(&[
                create_edge(
                    "ab",
                    "a",
                    "R",
                    "b",
                    vec![segment(None, Some(10), 1), segment(Some(10), None, 2)],
                ),
                GraphOp::CreateEdge {
                    key: Some("cd".into()),
                    source: "c".into(),
                    relationship: "R".into(),
                    destination: "d".into(),
                    properties: properties(3),
                },
            ])
            .unwrap();
        let before = db
            .graph("g")
            .unwrap()
            .as_of(10)
            .unwrap()
            .with_clock(0)
            .get_edge_by_key("ab")
            .unwrap();
        let before_traverse = db
            .graph("g")
            .unwrap()
            .as_of(10)
            .unwrap()
            .with_clock(0)
            .traverse(
                &Traverse::new(vec![NodeSelector::key("a")]),
                &ReadOptions::default(),
            )
            .unwrap();
        let before_shortest = db
            .graph("g")
            .unwrap()
            .as_of(10)
            .unwrap()
            .with_clock(0)
            .shortest_path(
                &ShortestPath::new(NodeSelector::key("a"), NodeSelector::key("b")),
                &ReadOptions::default(),
            )
            .unwrap();
        let expected_timeline = db
            .graph("g")
            .unwrap()
            .as_of(10)
            .unwrap()
            .with_clock(0)
            .timeline(EdgeRef::Key("ab".into()), 0..2)
            .unwrap()
            .read
            .output
            .unwrap();
        drop(db);

        let reopened = if let Some(key) = &key {
            Database::open_encrypted(&path, key).unwrap()
        } else {
            Database::open(&path).unwrap()
        };
        let after = reopened
            .graph("g")
            .unwrap()
            .as_of(10)
            .unwrap()
            .with_clock(0)
            .get_edge_by_key("ab")
            .unwrap();
        let after_traverse = reopened
            .graph("g")
            .unwrap()
            .as_of(10)
            .unwrap()
            .with_clock(0)
            .traverse(
                &Traverse::new(vec![NodeSelector::key("a")]),
                &ReadOptions::default(),
            )
            .unwrap();
        let after_shortest = reopened
            .graph("g")
            .unwrap()
            .as_of(10)
            .unwrap()
            .with_clock(0)
            .shortest_path(
                &ShortestPath::new(NodeSelector::key("a"), NodeSelector::key("b")),
                &ReadOptions::default(),
            )
            .unwrap();
        assert_eq!(after.read.output, before.read.output, "{name}");
        assert_eq!(after.as_of_ms, before.as_of_ms);
        assert_eq!(after.ttl_clock_ms, before.ttl_clock_ms);
        assert_eq!(
            after_traverse.read.output, before_traverse.read.output,
            "{name}"
        );
        assert_eq!(
            after_traverse.read.view_sequence, before_traverse.read.view_sequence,
            "{name}"
        );
        assert_eq!(
            (after_traverse.as_of_ms, after_traverse.ttl_clock_ms),
            (before_traverse.as_of_ms, before_traverse.ttl_clock_ms),
            "{name}"
        );
        assert_eq!(
            after_shortest.read.output, before_shortest.read.output,
            "{name}"
        );
        assert_eq!(
            after_shortest.read.view_sequence, before_shortest.read.view_sequence,
            "{name}"
        );
        assert_eq!(
            reopened
                .graph("g")
                .unwrap()
                .as_of(10)
                .unwrap()
                .with_clock(0)
                .timeline(EdgeRef::Key("ab".into()), 0..2)
                .unwrap()
                .read
                .output
                .unwrap(),
            expected_timeline,
            "{name}"
        );
        assert_eq!(
            reopened
                .graph("g")
                .unwrap()
                .as_of(10)
                .unwrap()
                .get_edge_by_key("cd")
                .unwrap()
                .read
                .output
                .unwrap()
                .validity,
            None
        );
    }
}
