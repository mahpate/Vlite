use super::*;

fn base() -> Arc<CompactGraph> {
    let source = GraphSource {
        graph_id: 0,
        name: "snapshot".into(),
        sequence: 1,
        next_node_id: 0,
        next_edge_id: 0,
        temporal_enabled: false,
        schema: GraphSchema::default(),
        nodes: Vec::new(),
        edges: Vec::new(),
    };
    Arc::new(decode(&encode(&source).unwrap()).unwrap())
}

#[test]
fn snapshot_pins_matching_base_and_captures_commit() {
    let base = base();
    let mut overlay = Overlay::new(&base, 8);
    overlay
        .apply(Commit {
            sequence: 2,
            nodes: vec![Change {
                id: 7,
                row: Some(SourceNode {
                    id: 7,
                    key: Some("seven".into()),
                    labels: Vec::new(),
                    revision: 2,
                    deadline: None,
                    properties: Properties::new(),
                    attachments: BTreeMap::new(),
                }),
                deadline: Deadline::Set(Some(9)),
            }],
            edges: vec![Change {
                id: 8,
                row: Some(SourceEdge {
                    id: 8,
                    source: 7,
                    target: 7,
                    relationship: "R".into(),
                    key: Some("loop".into()),
                    revision: 2,
                    deadline: None,
                    properties: Properties::new(),
                    segments: None,
                }),
                deadline: Deadline::Preserve,
            }],
            high_water: Some((10, 11)),
        })
        .unwrap();
    let snapshot = overlay.snapshot(Arc::clone(&base)).unwrap();
    assert!(Arc::ptr_eq(&snapshot.base, &base));
    assert_eq!(snapshot.sequence, 2);
    assert_eq!((snapshot.next_node_id, snapshot.next_edge_id), (10, 11));
    assert_eq!(snapshot.nodes[&7].as_ref().unwrap().deadline, Some(9));
    assert_eq!(
        snapshot.edges[&8].as_ref().unwrap().key.as_deref(),
        Some("loop")
    );
    assert_eq!(snapshot.adjacency[&7], BTreeSet::from([8]));
    overlay
        .apply(Commit {
            sequence: 3,
            nodes: vec![Change {
                id: 7,
                row: None,
                deadline: Deadline::Preserve,
            }],
            edges: vec![Change {
                id: 8,
                row: None,
                deadline: Deadline::Preserve,
            }],
            high_water: None,
        })
        .unwrap();
    assert!(snapshot.nodes[&7].is_some());
}

#[test]
fn snapshot_rejects_equal_but_different_base_allocation() {
    let base = base();
    let other = Arc::new((*base).clone());
    assert_eq!(*base, *other);
    let overlay = Overlay::new(&base, 0);
    assert!(matches!(
        overlay.snapshot(other),
        Err(OverlayError::Identity)
    ));
}
