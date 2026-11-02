use super::*;

// Seeds, xorshift and committed-history shape reused from overlay/seeded.rs.
const SEEDS: [u64; 4] = [
    0x4d59_5df4_d0f3_3173,
    0x1234_5678_9abc_def0,
    0x9e37_79b9_7f4a_7c15,
    0xd1b5_4a32_d192_ed03,
];

fn payload(random: &mut u64) -> Properties {
    *random ^= *random << 13;
    *random ^= *random >> 7;
    *random ^= *random << 17;
    Properties::from([
        ("number".into(), PropertyValue::Integer(*random as i64)),
        (
            "text".into(),
            PropertyValue::String(format!("\u{8282}\0{random:016x}")),
        ),
        (
            "flag".into(),
            PropertyValue::Boolean(random.is_multiple_of(2)),
        ),
        ("float".into(), PropertyValue::Float(-0.125)),
        (
            "point".into(),
            PropertyValue::GeoPoint {
                latitude: 12.5,
                longitude: -179.75,
            },
        ),
    ])
}

fn put_node(id: u64, key: &str, random: &mut u64) -> GraphEntry {
    GraphEntry::PutNode {
        node_id: NodeId(id),
        key: key.into(),
        labels: BTreeSet::from(["Entity".into()]),
        properties: payload(random),
    }
}

fn put_edge(id: u64, from: u64, to: u64, kind: &str, key: &str, random: &mut u64) -> GraphEntry {
    GraphEntry::PutEdge {
        edge_id: EdgeId(id),
        source: NodeId(from),
        destination: NodeId(to),
        relationship: kind.into(),
        key: Some(key.into()),
        properties: payload(random),
    }
}

fn history(seed: u64) -> Vec<GraphState> {
    let mut random = seed;
    let mut state = GraphState::new();
    state.set_temporal(true).unwrap();
    let schema = GraphSchema::default()
        .label(
            "Entity",
            LabelDefinition::new("seeded").property(
                "number",
                PropertyDefinition::new(PropertyType::Integer)
                    .required()
                    .index(PropertyIndexKind::Range),
            ),
        )
        .relationship(
            "R",
            RelationshipDefinition::new("").endpoint("Entity", "Entity"),
        );
    let mut entries = vec![
        GraphEntry::CreateGraph {
            name: format!("seeded_{seed:016x}"),
        },
        GraphEntry::PutSchema { schema },
    ];
    for i in 0..6 {
        entries.push(put_node((i + 1) * 10, &format!("n{i}"), &mut random));
    }
    entries.extend([
        GraphEntry::NodeExpiry {
            node_id: NodeId(20),
            expires_at_ms: Some(7),
        },
        GraphEntry::NodeExpiry {
            node_id: NodeId(60),
            expires_at_ms: Some(5),
        },
        GraphEntry::Attach {
            node_id: NodeId(60),
            collection: 2,
            record_id: seed,
        },
        put_edge(10, 10, 20, "R", "parallel_a", &mut random),
        put_edge(20, 10, 20, "R", "parallel_b", &mut random),
        put_edge(30, 10, 10, "R", "loop", &mut random),
        put_edge(40, 20, 30, "A", "remove_a", &mut random),
        put_edge(50, 60, 10, "R", "reclaim", &mut random),
        put_edge(60, 30, 40, "Z", "deleted_edge", &mut random),
        GraphEntry::Temporal(TemporalEntry::PutEdge {
            edge_id: EdgeId(70),
            revision: 0,
            source: NodeId(40),
            destination: NodeId(40),
            relationship: "T".into(),
            key: Some("temporal".into()),
            segments: vec![
                Segment {
                    valid_from: None,
                    valid_to: Some(-2),
                    properties: payload(&mut random),
                },
                Segment {
                    valid_from: Some(0),
                    valid_to: Some(5),
                    properties: payload(&mut random),
                },
                Segment {
                    valid_from: Some(5),
                    valid_to: None,
                    properties: payload(&mut random),
                },
            ],
        }),
        GraphEntry::EdgeExpiry {
            edge_id: EdgeId(20),
            expires_at_ms: Some(9),
        },
        GraphEntry::EdgeExpiry {
            edge_id: EdgeId(30),
            expires_at_ms: Some(7),
        },
    ]);
    apply(&mut state, 3, entries);
    let mut states = vec![state.clone()];
    let batches = vec![
        (
            11,
            vec![
                GraphEntry::PatchNode {
                    node_id: NodeId(20),
                    add_labels: BTreeSet::from(["Extra".into()]),
                    remove_labels: BTreeSet::new(),
                    set: payload(&mut random),
                    unset: BTreeSet::new(),
                },
                GraphEntry::PatchEdge {
                    edge_id: EdgeId(20),
                    set: {
                        let mut properties = payload(&mut random);
                        properties.remove("flag");
                        properties
                    },
                    unset: BTreeSet::from(["flag".into()]),
                },
            ],
        ),
        (
            23,
            vec![
                GraphEntry::NodeExpiry {
                    node_id: NodeId(20),
                    expires_at_ms: None,
                },
                GraphEntry::EdgeExpiry {
                    edge_id: EdgeId(30),
                    expires_at_ms: Some(5),
                },
            ],
        ),
        (
            31,
            vec![
                GraphEntry::DeleteEdge {
                    edge_id: EdgeId(40),
                },
                GraphEntry::DeleteEdge {
                    edge_id: EdgeId(60),
                },
                GraphEntry::DeleteNode {
                    node_id: NodeId(30),
                },
                GraphEntry::Detach {
                    node_id: NodeId(60),
                    collection: 2,
                },
            ],
        ),
        (
            44,
            vec![
                put_node(80, "n2", &mut random),
                put_edge(90, 80, 10, "R", "deleted_edge", &mut random),
                put_edge(100, 80, 80, "R", "new_loop", &mut random),
                put_edge(110, 10, 20, "R", "new_parallel", &mut random),
            ],
        ),
        (
            51,
            vec![
                GraphEntry::NodeExpiry {
                    node_id: NodeId(80),
                    expires_at_ms: Some(7),
                },
                GraphEntry::EdgeExpiry {
                    edge_id: EdgeId(90),
                    expires_at_ms: Some(5),
                },
                GraphEntry::Attach {
                    node_id: NodeId(80),
                    collection: 2,
                    record_id: seed,
                },
                GraphEntry::Temporal(TemporalEntry::ReplaceSegment {
                    edge_id: EdgeId(70),
                    valid_from: Some(0),
                    valid_to: Some(5),
                    segment: Segment {
                        valid_from: Some(1),
                        valid_to: Some(4),
                        properties: payload(&mut random),
                    },
                }),
            ],
        ),
    ];
    for (sequence, entries) in batches {
        apply(&mut state, sequence, entries);
        states.push(state.clone());
    }
    states
}

pub(super) fn histories_every_split() {
    for seed in SEEDS {
        let states = history(seed);
        for cut in 0..states.len() {
            let base = Arc::new(decode(&encode(&source(&states[cut])).unwrap()).unwrap());
            let mut overlay = Overlay::new(&base, 128);
            all(&CompactGraphView::new(Arc::clone(&base)), &states[cut]);
            for step in cut + 1..states.len() {
                overlay
                    .apply(projection(
                        &source(&states[step - 1]),
                        &source(&states[step]),
                    ))
                    .unwrap();
                let view =
                    CompactGraphView::with_overlay(overlay.snapshot(Arc::clone(&base)).unwrap());
                all(&view, &states[step]);
            }
        }
    }
}
