//! Adapter tests for the EPIC-005 read surface (ITEM-019 Increment B):
//! property conditions behave exactly like `metadata::matches` record
//! filters (directly and through `find_nodes`/`neighbors` on a real file),
//! invalid conditions fail like record filters, and `view_sequence` follows
//! the handle's view.

use std::path::Path;

use super::super::GraphOp;
use super::super::convert::{properties_from_metadata, property_conditions};
use super::super::engine::conditions_match;
use super::*;
use crate::metadata::{self, MetadataValue};
use crate::{Database, Error};

/// SplitMix64: a small deterministic generator for seeded cases.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

const FIELDS: [&str; 4] = ["a", "b", "c", "geo"];

fn value(rng: &mut Rng) -> MetadataValue {
    match rng.below(6) {
        0 => MetadataValue::Integer(rng.below(7) as i64 - 3),
        1 => MetadataValue::Float(*rng.pick(&[-1.5, -0.0, 0.0, 0.5, 1.0, 2.25])),
        2 => MetadataValue::String(rng.pick(&["x", "y", ""]).to_string()),
        3 => MetadataValue::Boolean(rng.below(2) == 0),
        _ => MetadataValue::GeoPoint {
            latitude: rng.below(5) as f64 * 0.01,
            longitude: rng.below(5) as f64 * 0.01,
        },
    }
}

fn record_metadata(rng: &mut Rng) -> Metadata {
    let mut metadata = Metadata::new();
    for field in FIELDS {
        if rng.below(4) != 0 {
            metadata.insert(field.to_string(), value(rng));
        }
    }
    metadata
}

fn numeric(rng: &mut Rng, float: bool) -> MetadataValue {
    if float {
        MetadataValue::Float(*rng.pick(&[-2.0, -0.0, 0.5, 1.0, 3.0]))
    } else {
        MetadataValue::Integer(rng.below(7) as i64 - 3)
    }
}

fn bound(rng: &mut Rng, float: bool) -> Option<MetadataValue> {
    match rng.below(12) {
        0 => None,
        1 => Some(numeric(rng, !float)),
        2 => Some(MetadataValue::Boolean(true)),
        _ => Some(numeric(rng, float)),
    }
}

/// Mostly valid conditions, with occasional invalid ones (mixed-type or
/// non-numeric range bounds, unbounded ranges, negative radii).
fn conditions(rng: &mut Rng) -> Vec<Condition> {
    (0..rng.below(4))
        .map(|_| {
            let field = rng.pick(&FIELDS).to_string();
            match rng.below(4) {
                0 | 1 => Condition::Eq {
                    field,
                    value: value(rng),
                },
                2 => {
                    let float = rng.below(2) == 0;
                    Condition::Range {
                        field,
                        min: bound(rng, float),
                        max: bound(rng, float),
                    }
                }
                _ => Condition::GeoRadius {
                    field,
                    latitude: 0.0,
                    longitude: 0.0,
                    radius_meters: *rng.pick(&[-1.0, 0.0, 1_000.0, 3_000.0, 10_000.0]),
                },
            }
        })
        .collect()
}

#[test]
fn property_conditions_match_exactly_like_record_filters() {
    let mut rng = Rng(0x5eed);
    let (mut valid, mut invalid, mut matched) = (0, 0, 0);
    for _ in 0..20_000 {
        let metadata = record_metadata(&mut rng);
        let conditions = conditions(&mut rng);
        let properties = properties_from_metadata(&metadata).unwrap();
        match metadata::validate_filter(&conditions) {
            Ok(()) => {
                valid += 1;
                let converted = property_conditions(&conditions).unwrap();
                let expected = metadata::matches(&metadata, &conditions);
                assert_eq!(
                    conditions_match(&properties, &converted),
                    expected,
                    "{metadata:?} {conditions:?}"
                );
                matched += usize::from(expected);
            }
            Err(error) => {
                invalid += 1;
                let converted = property_conditions(&conditions).unwrap_err();
                assert_eq!(
                    std::mem::discriminant(&converted),
                    std::mem::discriminant(&error)
                );
            }
        }
    }
    // The generator exercises both outcomes and both validation results.
    assert!(valid > 10_000 && invalid > 500, "{valid} {invalid}");
    assert!(matched > 1_000 && matched < valid, "{matched} of {valid}");
}

fn graph_db(path: &Path) -> Database {
    let mut db = Database::create(path, 2).unwrap();
    db.create_graph("g").unwrap();
    db
}

/// Node `nXX` and edge `hub -> nXX` carry the same random metadata.
fn seeded_graph(db: &mut Database, rng: &mut Rng) -> Vec<(String, Metadata)> {
    let records: Vec<(String, Metadata)> = (0..80)
        .map(|index| (format!("n{index:02}"), record_metadata(rng)))
        .collect();
    let mut ops = vec![GraphOp::CreateNode {
        key: "hub".into(),
        labels: Vec::new(),
        properties: Metadata::new(),
    }];
    for (key, metadata) in &records {
        ops.push(GraphOp::CreateNode {
            key: key.clone(),
            labels: vec!["Item".into()],
            properties: metadata.clone(),
        });
        ops.push(GraphOp::CreateEdge {
            key: Some(format!("e-{key}")),
            source: "hub".into(),
            relationship: "HAS".into(),
            destination: key.clone(),
            properties: metadata.clone(),
        });
    }
    db.graph_mut("g").unwrap().write_batch(&ops).unwrap();
    records
}

#[test]
fn find_nodes_and_neighbors_filter_like_record_filters_on_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph_db(&dir.path().join("parity.vlite"));
    let mut rng = Rng(42);
    let records = seeded_graph(&mut db, &mut rng);
    let graph = db.graph("g").unwrap();
    let options = ReadOptions::default();
    let mut checked = 0;
    while checked < 300 {
        let conditions = conditions(&mut rng);
        if metadata::validate_filter(&conditions).is_err() {
            continue;
        }
        checked += 1;
        let expected: Vec<&str> = records
            .iter()
            .filter(|(_, metadata)| metadata::matches(metadata, &conditions))
            .map(|(key, _)| key.as_str())
            .collect();

        let found = graph
            .find_nodes(
                &FindNodes {
                    label: Some("Item".into()),
                    conditions: conditions.clone(),
                    limit: Some(1_000),
                    cursor: None,
                },
                &options,
            )
            .unwrap();
        assert!(found.complete && found.cursor.is_none());
        let keys: Vec<&str> = found.output.iter().map(|node| node.key.as_str()).collect();
        assert_eq!(keys, expected, "{conditions:?}");
        for node in &found.output {
            let (_, metadata) = records.iter().find(|(key, _)| *key == node.key).unwrap();
            assert_eq!(&node.properties, metadata);
        }

        let neighbors = graph
            .neighbors(
                &Neighbors {
                    edge_conditions: conditions.clone(),
                    limit: Some(1_000),
                    ..Neighbors::new(NodeSelector::key("hub"))
                },
                &options,
            )
            .unwrap();
        assert!(neighbors.complete);
        let targets: Vec<&str> = neighbors
            .output
            .edges
            .iter()
            .map(|edge| edge.target_key.as_str())
            .collect();
        assert_eq!(targets, expected, "{conditions:?}");
    }
}

#[test]
fn invalid_conditions_fail_like_record_filters() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph_db(&dir.path().join("invalid.vlite"));
    seeded_graph(&mut db, &mut Rng(1));
    let graph = db.graph("g").unwrap();
    let options = ReadOptions::default();
    let cases = vec![
        vec![Condition::Eq {
            field: "a".into(),
            value: MetadataValue::Float(f64::NAN),
        }],
        vec![Condition::Range {
            field: "a".into(),
            min: None,
            max: None,
        }],
        vec![Condition::Range {
            field: "a".into(),
            min: Some(MetadataValue::Integer(1)),
            max: Some(MetadataValue::Float(2.0)),
        }],
        vec![Condition::GeoRadius {
            field: "geo".into(),
            latitude: 0.0,
            longitude: 0.0,
            radius_meters: -1.0,
        }],
        vec![Condition::Eq {
            field: String::new(),
            value: MetadataValue::Boolean(true),
        }],
    ];
    for conditions in cases {
        let expected = metadata::validate_filter(&conditions).unwrap_err();
        let find = graph
            .find_nodes(
                &FindNodes {
                    conditions: conditions.clone(),
                    ..FindNodes::default()
                },
                &options,
            )
            .unwrap_err();
        let neighbors = graph
            .neighbors(
                &Neighbors {
                    edge_conditions: conditions.clone(),
                    ..Neighbors::new(NodeSelector::key("hub"))
                },
                &options,
            )
            .unwrap_err();
        let mut traverse = Traverse::new(vec![NodeSelector::key("hub")]);
        traverse.return_filter.conditions = conditions.clone();
        let traverse = graph.traverse(&traverse, &options).unwrap_err();
        for error in [find, neighbors, traverse] {
            assert_eq!(
                std::mem::discriminant(&error),
                std::mem::discriminant(&expected),
                "{error:?} vs {expected:?}"
            );
        }
    }
}

#[test]
fn invalid_limits_precede_invalid_query_conversion_on_every_read_entry_point() {
    let dir = tempfile::tempdir().unwrap();
    let db = graph_db(&dir.path().join("precedence.vlite"));
    let graph = db.graph("g").unwrap();
    let invalid = vec![Condition::Range {
        field: "a".into(),
        min: None,
        max: None,
    }];
    let find = FindNodes {
        conditions: invalid.clone(),
        ..FindNodes::default()
    };
    let neighbors = Neighbors {
        edge_conditions: invalid.clone(),
        ..Neighbors::new(NodeSelector::key(""))
    };
    let traverse = Traverse {
        edge_filter: invalid,
        ..Traverse::new(vec![NodeSelector::key("")])
    };
    let shortest = ShortestPath::new(NodeSelector::key(""), NodeSelector::key(""));
    let queries = [
        GraphQuery::GetNode(NodeSelector::key("")),
        GraphQuery::GetEdge(EdgeRef::Key(String::new())),
        GraphQuery::FindNodes(find.clone()),
        GraphQuery::Neighbors(neighbors.clone()),
        GraphQuery::Traverse(traverse.clone()),
        GraphQuery::ShortestPath(shortest.clone()),
    ];
    for above_ceiling in [true, false] {
        let mut options = ReadOptions::default();
        if above_ceiling {
            options.limits.max_hops = GraphLimits::CEILING.max_hops + 1;
        } else {
            options.limits.max_visited_nodes = 0;
        }
        let mut errors = vec![
            graph.find_nodes(&find, &options).unwrap_err(),
            graph.neighbors(&neighbors, &options).unwrap_err(),
            graph.traverse(&traverse, &options).unwrap_err(),
            graph.shortest_path(&shortest, &options).unwrap_err(),
        ];
        for query in &queries {
            errors.push(graph.query(query, &options).unwrap_err());
            errors.push(graph.explain(query, &options).unwrap_err());
        }
        for error in errors {
            if above_ceiling {
                assert!(
                    matches!(
                        error,
                        Error::GraphLimitExceedsCeiling {
                            limit: "max_hops",
                            ..
                        }
                    ),
                    "{error:?}"
                );
            } else {
                assert!(
                    matches!(
                        error,
                        Error::InvalidQuery("max_visited_nodes must be at least 1")
                    ),
                    "{error:?}"
                );
            }
        }
    }
}

#[test]
fn view_sequence_is_the_handle_view_and_read_only_handles_keep_the_open_time_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("view.vlite");
    let mut db = graph_db(&path);
    let created = db.sequence;
    let graph = db.graph("g").unwrap();
    assert_eq!(graph.view_sequence(), created);
    let read = graph.get_node("missing").unwrap();
    assert_eq!(read.output, None);
    assert!(read.complete && read.truncation.is_empty());
    assert_eq!(read.view_sequence, created);

    let write = db
        .graph_mut("g")
        .unwrap()
        .create_node("a", &["Doc"], &Metadata::new())
        .unwrap();
    // Unrelated vector writes still advance the database view.
    db.put(1, &[1.0, 0.0], b"").unwrap();
    let head = db.sequence;
    assert!(head > write.revision);
    let graph = db.graph("g").unwrap();
    assert_eq!(graph.view_sequence(), head);
    let node = graph.get_node("a").unwrap();
    assert_eq!(node.view_sequence, head);
    assert_eq!(node.output.unwrap().revision, write.revision);
    let by_id = graph.get_node_by_id(write.id).unwrap();
    assert_eq!(by_id.output.unwrap().key, "a");
    let explained = graph
        .explain(
            &GraphQuery::GetNode(NodeSelector::key("a")),
            &ReadOptions::default(),
        )
        .unwrap();
    assert_eq!(explained.indexes, vec![IndexChoice::NodeKeyIndex]);
    drop(db);

    let reader = Database::open_read_only(&path).unwrap();
    let graph = reader.graph("g").unwrap();
    assert_eq!(graph.view_sequence(), head);
    let read = graph
        .traverse(
            &Traverse::new(vec![NodeSelector::key("a")]),
            &ReadOptions::default(),
        )
        .unwrap();
    assert_eq!(read.view_sequence, head);
    assert_eq!(read.output.nodes.len(), 1);
    assert!(matches!(reader.graph("missing"), Err(Error::GraphNotFound)));
}
