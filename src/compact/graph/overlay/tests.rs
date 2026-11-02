use super::*;
use serde_json::{Value, json};

fn number(value: &Value, field: &str) -> u64 {
    value[field].as_u64().unwrap()
}

fn deadline(value: &Value) -> Deadline {
    match value.get("deadline") {
        None => Deadline::Preserve,
        Some(value) => Deadline::Set(value.as_u64()),
    }
}

fn node(id: u64, row: &Value, revision: u64) -> SourceNode {
    SourceNode {
        id,
        key: None,
        labels: row["labels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect(),
        revision,
        deadline: row["deadline"].as_u64(),
        properties: Properties::new(),
        attachments: BTreeMap::new(),
    }
}

fn edge(id: u64, row: &Value, revision: u64) -> SourceEdge {
    SourceEdge {
        id,
        source: number(row, "source"),
        target: number(row, "target"),
        relationship: row["relationship"].as_str().unwrap().to_owned(),
        key: None,
        revision,
        deadline: row["deadline"].as_u64(),
        properties: Properties::new(),
        segments: None,
    }
}

fn empty_source() -> GraphSource {
    GraphSource {
        graph_id: 0,
        name: "overlay".into(),
        sequence: 0,
        next_node_id: 0,
        next_edge_id: 0,
        temporal_enabled: false,
        schema: GraphSchema::default(),
        nodes: Vec::new(),
        edges: Vec::new(),
    }
}

fn put_node(id: u64) -> Change<SourceNode> {
    Change {
        id,
        row: Some(node(id, &json!({"labels": []}), 0)),
        deadline: Deadline::Preserve,
    }
}

fn put_edge(id: u64, source: u64, target: u64) -> Change<SourceEdge> {
    Change {
        id,
        row: Some(edge(
            id,
            &json!({"source": source, "target": target, "relationship": "r"}),
            0,
        )),
        deadline: Deadline::Preserve,
    }
}

fn delete<T>(id: u64) -> Change<T> {
    Change {
        id,
        row: None,
        deadline: Deadline::Preserve,
    }
}

fn commit(sequence: u64) -> Commit {
    Commit {
        sequence,
        nodes: Vec::new(),
        edges: Vec::new(),
        high_water: None,
    }
}

#[test]
fn retained_overlay_goldens() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/compact/graph-overlay-cases.json"
    ))
    .unwrap();
    assert_eq!(fixture["cases"].as_array().unwrap().len(), 5);
    for case in fixture["cases"].as_array().unwrap() {
        let sequence = number(case, "base_sequence");
        let mut source = empty_source();
        source.sequence = sequence;
        source.next_node_id = number(&case["base"], "next_node_id");
        source.next_edge_id = number(&case["base"], "next_edge_id");
        source.nodes = case["base"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| node(number(row, "id"), row, sequence))
            .collect();
        source.edges = case["base"]["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| edge(number(row, "id"), row, sequence))
            .collect();
        let base = decode(&encode(&source).unwrap()).unwrap();
        let mut overlay = Overlay::new(&base, 128);
        let mut commits: Vec<_> = case["overlays"].as_array().unwrap().iter().collect();
        commits.sort_by_key(|row| number(row, "sequence"));
        for layer in commits {
            let sequence = number(layer, "sequence");
            let nodes = layer["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| {
                    let id = number(entry, "id");
                    Change {
                        id,
                        row: (!entry["row"].is_null()).then(|| node(id, &entry["row"], sequence)),
                        deadline: deadline(&entry["row"]),
                    }
                })
                .collect();
            let edges = layer["edges"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| {
                    let id = number(entry, "id");
                    Change {
                        id,
                        row: (!entry["row"].is_null()).then(|| edge(id, &entry["row"], sequence)),
                        deadline: deadline(&entry["row"]),
                    }
                })
                .collect();
            overlay
                .apply(Commit {
                    sequence,
                    nodes,
                    edges,
                    high_water: None,
                })
                .unwrap();
        }
        let clock = number(case, "clock");
        let nodes: Vec<_> = overlay
            .node_ids()
            .into_iter()
            .filter_map(|id| overlay.node(id, clock))
            .map(|node| {
                let sequence = overlay
                    .nodes
                    .get(&node.id)
                    .map_or(sequence, |version| version.sequence);
                json!({"id": node.id, "sequence": sequence, "labels": node.labels,
                       "deadline": node.deadline})
            })
            .collect();
        let edges: Vec<_> = overlay
            .edge_ids()
            .into_iter()
            .filter_map(|id| overlay.edge(id, clock, None))
            .map(|read| {
                let edge = read.edge;
                let sequence = overlay
                    .edges
                    .get(&edge.id)
                    .map_or(sequence, |version| version.sequence);
                json!({"id": edge.id, "sequence": sequence, "source": edge.source,
                       "target": edge.target, "relationship": edge.relationship,
                       "deadline": edge.deadline})
            })
            .collect();
        let adjacency: Vec<Value> = case["queries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|query| {
                let direction = match query["direction"].as_str().unwrap() {
                    "in" => Direction::In,
                    "out" => Direction::Out,
                    "both" => Direction::Both,
                    value => panic!("unknown direction {value}"),
                };
                let types = query["relationship"].as_array().map(|types| {
                    types
                        .iter()
                        .map(|value| value.as_str().unwrap().to_owned())
                        .collect()
                });
                let after = query["after"].as_object().map(|_| {
                    (
                        query["after"]["relationship"].as_str().unwrap(),
                        number(&query["after"], "edge"),
                    )
                });
                let limit = number(query, "limit");
                let rows: Vec<_> = overlay
                    .neighbors(
                        number(query, "node"),
                        direction,
                        types.as_ref(),
                        clock,
                        None,
                    )
                    .into_iter()
                    .filter(|read| {
                        after.is_none_or(|after| {
                            (read.edge.relationship.as_str(), read.edge.id) > after
                        })
                    })
                    .take(usize::try_from(limit).unwrap_or(usize::MAX))
                    .map(|read| json!([read.edge.relationship, read.edge.id]))
                    .collect();
                json!(rows)
            })
            .collect();
        assert_eq!(
            json!({"nodes": nodes, "edges": edges, "adjacency": adjacency}),
            case["expected"],
            "{}",
            case["name"]
        );
        assert_eq!(overlay.counts(clock), (nodes.len(), edges.len()));
        let folded = overlay.fold().unwrap();
        let folded_overlay = Overlay::new(&folded, 0);
        assert_eq!(folded_overlay.counts(clock), overlay.counts(clock));
        assert_eq!(
            encode(&folded.source).unwrap(),
            encode(&overlay.source()).unwrap()
        );
    }
}

#[test]
fn bounded_atomic_admission_and_retained_tombstones() {
    let base = build(empty_source()).unwrap();
    let mut overlay = Overlay::new(&base, 3);
    let mut first = commit(1);
    first.nodes = vec![put_node(1), put_node(2)];
    overlay.apply(first).unwrap(); // cap - 1
    assert_eq!(overlay.nodes.len(), 2);
    let mut second = commit(2);
    second.edges = vec![put_edge(10, 1, 2)];
    overlay.apply(second).unwrap(); // cap
    let before = encode(&overlay.source()).unwrap();
    let mut refused = commit(3);
    refused.nodes = vec![put_node(3)];
    refused.edges = vec![delete(10)];
    assert_eq!(
        overlay.apply(refused),
        Err(OverlayError::Capacity { cap: 3, needed: 4 })
    );
    assert_eq!(encode(&overlay.source()).unwrap(), before);
    let mut deletion = commit(3);
    deletion.edges = vec![delete(10)];
    deletion.nodes = vec![delete(1)];
    overlay.apply(deletion).unwrap();
    assert!(overlay.node(1, 0).is_none());
    assert!(overlay.edge(10, 0, None).is_none());
    assert_eq!(overlay.nodes.len() + overlay.edges.len(), 3);
    let mut reuse = commit(4);
    reuse.nodes = vec![put_node(1)];
    assert_eq!(overlay.apply(reuse), Err(OverlayError::IdReuse));
    let mut reuse = commit(4);
    reuse.edges = vec![put_edge(10, 2, 2)];
    assert_eq!(overlay.apply(reuse), Err(OverlayError::IdReuse));
    assert_eq!((overlay.next_node_id, overlay.next_edge_id), (3, 11));
    let folded = overlay.fold().unwrap();
    let mut reopened = Overlay::new(&folded, 4);
    let mut reuse = commit(4);
    reuse.nodes = vec![put_node(1)];
    assert_eq!(reopened.apply(reuse), Err(OverlayError::IdReuse));
    let mut fresh = commit(4);
    fresh.nodes = vec![put_node(3)];
    reopened.apply(fresh).unwrap();
    assert!(reopened.node(3, 0).is_some());
}

#[test]
fn projection_refusals_are_atomic() {
    let mut source = empty_source();
    source.nodes = vec![
        node(1, &json!({"labels":[]}), 0),
        node(2, &json!({"labels":[]}), 0),
    ];
    source.edges = vec![edge(
        1,
        &json!({"source":1,"target":2,"relationship":"r"}),
        0,
    )];
    source.next_node_id = 3;
    source.next_edge_id = 2;
    let base = decode(&encode(&source).unwrap()).unwrap();
    let mut overlay = Overlay::new(&base, 10);
    let before = encode(&overlay.source()).unwrap();
    let mut bad = commit(1);
    bad.nodes = vec![delete(1)];
    assert_eq!(overlay.apply(bad), Err(OverlayError::IncidentEdges));
    let mut bad = commit(1);
    bad.edges = vec![put_edge(1, 2, 1)];
    assert_eq!(overlay.apply(bad), Err(OverlayError::Identity));
    let mut bad = commit(1);
    bad.edges = vec![put_edge(2, 1, 99)];
    assert_eq!(
        overlay.apply(bad),
        Err(OverlayError::Invalid(CodecError::Corrupt))
    );
    let mut bad = commit(1);
    bad.nodes = vec![put_node(3), put_node(3)];
    assert_eq!(overlay.apply(bad), Err(OverlayError::DuplicateId));
    let mut bad = commit(1);
    bad.nodes = vec![delete(3)];
    assert_eq!(overlay.apply(bad), Err(OverlayError::MissingRow));
    let mut bad = commit(1);
    bad.high_water = Some((2, 2));
    assert_eq!(overlay.apply(bad), Err(OverlayError::HighWater));
    let mut bad = commit(1);
    bad.nodes = vec![put_node(u64::MAX)];
    assert_eq!(overlay.apply(bad), Err(OverlayError::IdExhausted));
    assert_eq!(overlay.apply(commit(0)), Err(OverlayError::Sequence));
    assert_eq!(encode(&overlay.source()).unwrap(), before);
    let mut removal = commit(1);
    removal.edges = vec![delete(1)];
    removal.nodes = vec![delete(1)];
    overlay.apply(removal).unwrap();
    assert!(overlay.node(1, 0).is_none());
    assert!(overlay.edge(1, 0, None).is_none());
    assert!(
        overlay
            .neighbors(2, Direction::Both, None, 0, None)
            .is_empty()
    );
}

#[test]
fn zero_cap_accepts_only_empty_commits() {
    let base = build(empty_source()).unwrap();
    let mut overlay = Overlay::new(&base, 0);
    overlay.apply(commit(1)).unwrap();
    let mut full = commit(2);
    full.nodes = vec![put_node(0)];
    assert_eq!(
        overlay.apply(full),
        Err(OverlayError::Capacity { cap: 0, needed: 1 })
    );
    assert_eq!(overlay.sequence, 1);
    assert_eq!(overlay.counts(0), (0, 0));
}

#[test]
fn committed_high_water_survives_empty_projection_and_fold() {
    let base = build(empty_source()).unwrap();
    let mut overlay = Overlay::new(&base, 0);
    let mut metadata = commit(1);
    metadata.high_water = Some((900, u64::MAX));
    overlay.apply(metadata).unwrap();
    assert_eq!(overlay.nodes.len() + overlay.edges.len(), 0);
    let folded = overlay.fold().unwrap();
    let mut reopened = Overlay::new(&folded, 2);
    let mut reuse = commit(2);
    reuse.nodes = vec![put_node(899)];
    assert_eq!(reopened.apply(reuse), Err(OverlayError::IdReuse));
    let mut exhausted = commit(2);
    exhausted.edges = vec![put_edge(u64::MAX, 900, 900)];
    assert_eq!(reopened.apply(exhausted), Err(OverlayError::IdExhausted));
    let mut fresh = commit(2);
    fresh.nodes = vec![put_node(900)];
    reopened.apply(fresh).unwrap();
    assert_eq!(reopened.next_node_id, 901);
    assert_eq!(reopened.next_edge_id, u64::MAX);
}

#[test]
fn base_keys_expiry_overrides_and_opaque_payloads() {
    let mut source = empty_source();
    let mut first = node(0, &json!({"labels":["L"],"deadline":5}), 0);
    first.key = Some("first".into());
    first
        .properties
        .insert("rank".into(), PropertyValue::Integer(42));
    first.attachments.insert(3, 17);
    let mut second = node(1, &json!({"labels":[]}), 0);
    second.key = Some("second".into());
    source.nodes = vec![first, second];
    let mut connection = edge(
        0,
        &json!({"source":0,"target":1,"relationship":"r","deadline":5}),
        0,
    );
    connection.key = Some("connection".into());
    connection.properties.insert(
        "text".into(),
        PropertyValue::String("opaque property".into()),
    );
    source.edges = vec![connection];
    source.next_node_id = 2;
    source.next_edge_id = 1;
    let base = decode(&encode(&source).unwrap()).unwrap();
    let mut overlay = Overlay::new(&base, 8);
    assert!(overlay.node_by_key("first", 5).is_none());
    assert!(overlay.edge_by_key("connection", 5, None).is_none());
    let mut clear = commit(1);
    clear.nodes = vec![Change {
        id: 0,
        row: Some(base.source.nodes[0].clone()),
        deadline: Deadline::Set(None),
    }];
    clear.edges = vec![Change {
        id: 0,
        row: Some(base.source.edges[0].clone()),
        deadline: Deadline::Set(None),
    }];
    overlay.apply(clear).unwrap();
    assert_eq!(
        overlay.node_by_key("first", 5).unwrap().properties,
        source.nodes[0].properties
    );
    assert_eq!(
        overlay.node_by_key("first", 5).unwrap().attachments,
        source.nodes[0].attachments
    );
    assert_eq!(
        overlay
            .edge_by_key("connection", 5, None)
            .unwrap()
            .properties,
        &source.edges[0].properties
    );
    assert_eq!(overlay.neighbors(0, Direction::Out, None, 5, None).len(), 1);
    let before = encode(&overlay.source()).unwrap();
    let mut bad = commit(2);
    let mut changed_key = overlay.raw_node(0).unwrap().clone();
    changed_key.key = Some("changed".into());
    bad.nodes = vec![Change {
        id: 0,
        row: Some(changed_key),
        deadline: Deadline::Preserve,
    }];
    assert_eq!(overlay.apply(bad), Err(OverlayError::Identity));
    let mut bad = commit(2);
    let mut duplicate_owner = overlay.raw_node(1).unwrap().clone();
    duplicate_owner.attachments.insert(3, 17);
    bad.nodes = vec![Change {
        id: 1,
        row: Some(duplicate_owner),
        deadline: Deadline::Preserve,
    }];
    assert_eq!(
        overlay.apply(bad),
        Err(OverlayError::Invalid(CodecError::Corrupt))
    );
    let mut bad = commit(2);
    let mut temporal = overlay.raw_edge(0).unwrap().clone();
    temporal.segments = Some(Vec::new());
    bad.edges = vec![Change {
        id: 0,
        row: Some(temporal),
        deadline: Deadline::Preserve,
    }];
    assert_eq!(
        overlay.apply(bad),
        Err(OverlayError::Invalid(CodecError::Corrupt))
    );
    assert_eq!(encode(&overlay.source()).unwrap(), before);
    let mut expire = commit(2);
    expire.nodes = vec![Change {
        id: 0,
        row: Some(overlay.raw_node(0).unwrap().clone()),
        deadline: Deadline::Set(Some(5)),
    }];
    overlay.apply(expire).unwrap();
    assert!(overlay.node_by_key("first", 5).is_none());
    assert!(overlay.edge_by_key("connection", 5, None).is_none());
    assert_eq!(overlay.counts(4), (2, 1));
    assert_eq!(overlay.counts(5), (1, 0));
    assert_eq!(
        overlay.fold().unwrap().source.nodes[0].attachments,
        source.nodes[0].attachments
    );
    let mut remove = commit(3);
    remove.nodes = vec![delete(0)];
    assert_eq!(overlay.apply(remove), Err(OverlayError::IncidentEdges));
    let mut remove = commit(3);
    remove.edges = vec![delete(0)];
    remove.nodes = vec![delete(0)];
    overlay.apply(remove).unwrap();
    assert!(overlay.node_by_key("first", 0).is_none());
    assert!(overlay.edge_by_key("connection", 0, None).is_none());
}
