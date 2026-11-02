use super::super::algorithm::{Index, take_visits};
use super::*;

fn algorithm_parity(view: &CompactGraphView, graph: &Graph) {
    let keys: Vec<_> = graph
        .nodes()
        .map(|node| node.key.as_str())
        .chain(["missing", "n2", "n30", "changed", "removed"])
        .collect();
    for key in keys {
        let expected = graph.node_by_key(key);
        assert_eq!(
            view.algorithm_node_by_key(key)
                .map(|node| (node.id, node.properties, node.revision)),
            expected.map(|node| (node.id, &node.properties, node.revision))
        );
    }
    let mut cursors: Vec<_> = graph
        .edges()
        .map(|edge| (edge.relationship.clone(), edge.id))
        .collect();
    cursors.extend([
        ("".into(), EdgeId(0)),
        ("R".into(), EdgeId(0)),
        ("unknown".into(), EdgeId(1)),
        ("zzz".into(), EdgeId(u64::MAX)),
    ]);
    for node in graph.nodes().map(|node| node.id).chain([NodeId(u64::MAX)]) {
        for direction in [Direction::Out, Direction::In, Direction::Both] {
            let expected: Vec<_> = graph.adjacency(node, direction, None, None).collect();
            assert_eq!(
                view.algorithm_adjacency(node, direction, None)
                    .collect::<Vec<_>>(),
                expected
            );
            for (kind, id) in &cursors {
                let expected: Vec<_> = graph
                    .adjacency(node, direction, None, Some((kind, *id)))
                    .collect();
                // Cursor owner is discarded before iteration.
                let iterator = {
                    let owned = kind.clone();
                    view.algorithm_adjacency(node, direction, Some((&owned, *id)))
                };
                assert_eq!(iterator.collect::<Vec<_>>(), expected);
            }
        }
    }
}

fn topology() -> GraphState {
    let mut state = GraphState::new();
    let mut entries = vec![GraphEntry::CreateGraph {
        name: "algorithm".into(),
    }];
    for id in 0..3 {
        entries.push(GraphEntry::PutNode {
            node_id: NodeId(id),
            key: format!("k{id}"),
            labels: BTreeSet::new(),
            properties: Properties::new(),
        });
    }
    for (id, from, to, kind) in [
        (0, 0, 0, "R"),
        (1, 0, 1, "R0"),
        (2, 1, 0, "R"),
        (3, 0, 1, "A"),
        (4, 0, 1, "R"),
        (5, 2, 0, "Z"),
        (6, 0, 2, "a"),
        (7, 0, 2, "Ra"),
    ] {
        entries.push(GraphEntry::PutEdge {
            edge_id: EdgeId(id),
            source: NodeId(from),
            destination: NodeId(to),
            relationship: kind.into(),
            key: Some(format!("edge{id}")),
            properties: Properties::new(),
        });
    }
    entries.extend([
        GraphEntry::NodeExpiry {
            node_id: NodeId(0),
            expires_at_ms: Some(0),
        },
        GraphEntry::EdgeExpiry {
            edge_id: EdgeId(3),
            expires_at_ms: Some(0),
        },
    ]);
    apply(&mut state, 1, entries);
    state
}

#[test]
fn physical_order_cursors_zero_loops_and_expiry_match_engine() {
    let state = topology();
    let view = base_view(&state);
    assert_eq!(
        view.algorithm_adjacency(NodeId(0), Direction::Both, None)
            .collect::<Vec<_>>(),
        [
            ("A", EdgeId(3)),
            ("R", EdgeId(0)),
            ("R", EdgeId(2)),
            ("R", EdgeId(4)),
            ("R0", EdgeId(1)),
            ("Ra", EdgeId(7)),
            ("Z", EdgeId(5)),
            ("a", EdgeId(6))
        ]
    );
    algorithm_parity(&view, state.graph(G).unwrap());
    assert!(view.read(0, None).node_by_key("k0").is_none());
    assert_eq!(view.algorithm_node_by_key("k0").unwrap().id, NodeId(0));
    let mut empty = GraphState::new();
    apply(
        &mut empty,
        1,
        vec![GraphEntry::CreateGraph {
            name: "empty".into(),
        }],
    );
    algorithm_parity(&base_view(&empty), empty.graph(G).unwrap());
}

#[test]
fn non_ascii_relationship_bytes_follow_independent_golden() {
    // Engine writes restrict relationship identifiers to ASCII. The private
    // codec's logical string dictionary also permits these byte-order goldens.
    let mut source = source(&topology());
    source.edges[5].relationship = "\u{00e9}".into();
    source.edges[6].relationship = "\u{8282}".into();
    let view = CompactGraphView::new(Arc::new(build(source).unwrap()));
    assert_eq!(
        view.algorithm_adjacency(NodeId(0), Direction::Both, None)
            .collect::<Vec<_>>(),
        [
            ("A", EdgeId(3)),
            ("R", EdgeId(0)),
            ("R", EdgeId(2)),
            ("R", EdgeId(4)),
            ("R0", EdgeId(1)),
            ("Ra", EdgeId(7)),
            ("\u{00e9}", EdgeId(5)),
            ("\u{8282}", EdgeId(6))
        ]
    );
    assert_eq!(
        view.algorithm_adjacency(NodeId(0), Direction::Both, Some(("\u{00e9}", EdgeId(5))))
            .collect::<Vec<_>>(),
        [("\u{8282}", EdgeId(6))]
    );
}

#[test]
fn tombstones_keys_reuse_and_pinned_snapshot_match_engine() {
    let mut state = topology();
    let initial = source(&state);
    let base = Arc::new(decode(&encode(&initial).unwrap()).unwrap());
    let original = CompactGraphView::new(Arc::clone(&base));
    let mut overlay = Overlay::new(&base, 32);
    apply(
        &mut state,
        2,
        vec![
            GraphEntry::DeleteEdge { edge_id: EdgeId(5) },
            GraphEntry::DeleteEdge { edge_id: EdgeId(6) },
            GraphEntry::DeleteEdge { edge_id: EdgeId(7) },
            GraphEntry::DeleteNode { node_id: NodeId(2) },
            GraphEntry::PutNode {
                node_id: NodeId(3),
                key: "k2".into(),
                labels: BTreeSet::new(),
                properties: Properties::new(),
            },
            GraphEntry::PutEdge {
                edge_id: EdgeId(8),
                source: NodeId(3),
                destination: NodeId(0),
                relationship: "A".into(),
                key: None,
                properties: Properties::new(),
            },
            GraphEntry::PatchEdge {
                edge_id: EdgeId(4),
                set: Properties::from([("p".into(), PropertyValue::Integer(9))]),
                unset: BTreeSet::new(),
            },
        ],
    );
    overlay
        .apply(projection(&initial, &source(&state)))
        .unwrap();
    let first = CompactGraphView::with_overlay(overlay.snapshot(Arc::clone(&base)).unwrap());
    let earlier = state.clone();
    let before = source(&state);
    apply(
        &mut state,
        3,
        vec![
            GraphEntry::DeleteEdge { edge_id: EdgeId(8) },
            GraphEntry::DeleteNode { node_id: NodeId(3) },
            GraphEntry::PutNode {
                node_id: NodeId(4),
                key: "k2".into(),
                labels: BTreeSet::new(),
                properties: Properties::new(),
            },
        ],
    );
    overlay.apply(projection(&before, &source(&state))).unwrap();
    let latest = CompactGraphView::with_overlay(overlay.snapshot(Arc::clone(&base)).unwrap());
    algorithm_parity(&original, topology().graph(G).unwrap());
    algorithm_parity(&first, earlier.graph(G).unwrap());
    algorithm_parity(&latest, state.graph(G).unwrap());
    assert_eq!(original.algorithm_node_by_key("k2").unwrap().id, NodeId(2));
    assert_eq!(first.algorithm_node_by_key("k2").unwrap().id, NodeId(3));
    assert_eq!(latest.algorithm_node_by_key("k2").unwrap().id, NodeId(4));
}

#[test]
fn key_changes_and_removals_use_effective_index() {
    // Key changes/removal are representable source projections; the accepted
    // ordered engine planner currently keeps node keys immutable.
    let state = topology();
    let base = Arc::new(decode(&encode(&source(&state)).unwrap()).unwrap());
    let mut snapshot = Overlay::new(&base, 0).snapshot(Arc::clone(&base)).unwrap();
    let mut changed = base.source.nodes[0].clone();
    changed.key = Some("changed".into());
    snapshot.nodes.insert(changed.id, Some(changed));
    let mut removed = base.source.nodes[1].clone();
    removed.key = None;
    snapshot.nodes.insert(removed.id, Some(removed));
    let view = CompactGraphView::with_overlay(snapshot);
    for old in ["k0", "k1"] {
        assert!(view.algorithm_node_by_key(old).is_none());
    }
    assert_eq!(view.algorithm_node_by_key("changed").unwrap().id, NodeId(0));
    assert!(view.algorithm_node_by_key("missing").is_none());
    assert_eq!(view.algorithm_node_by_key("k2").unwrap().id, NodeId(2));
    let mut snapshot = Overlay::new(&base, 0).snapshot(Arc::clone(&base)).unwrap();
    let mut changed = base.source.nodes[0].clone();
    changed.key = Some("changed".into());
    snapshot.nodes.insert(changed.id, Some(changed));
    let mut reused = base.source.nodes[1].clone();
    reused.key = Some("k0".into());
    snapshot.nodes.insert(reused.id, Some(reused));
    let view = CompactGraphView::with_overlay(snapshot);
    assert_eq!(view.algorithm_node_by_key("k0").unwrap().id, NodeId(1));
}

#[test]
fn tombstone_heavy_iteration_has_bounded_visits() {
    let mut state = GraphState::new();
    apply(
        &mut state,
        1,
        vec![
            GraphEntry::CreateGraph {
                name: "heavy".into(),
            },
            GraphEntry::PutNode {
                node_id: NodeId(0),
                key: "hub".into(),
                labels: BTreeSet::new(),
                properties: Properties::new(),
            },
            GraphEntry::PutNode {
                node_id: NodeId(1),
                key: "other".into(),
                labels: BTreeSet::new(),
                properties: Properties::new(),
            },
        ],
    );
    for start in (0..10_000).step_by(1000) {
        apply(
            &mut state,
            2 + start / 1000,
            (start..start + 1000)
                .map(|id| GraphEntry::PutEdge {
                    edge_id: EdgeId(id),
                    source: NodeId(0),
                    destination: NodeId(1),
                    relationship: "R".into(),
                    key: None,
                    properties: Properties::new(),
                })
                .collect(),
        );
    }
    let initial = source(&state);
    let base = Arc::new(decode(&encode(&initial).unwrap()).unwrap());
    let mut overlay = Overlay::new(&base, 10_000);
    for (step, start) in (0..9990).step_by(1000).enumerate() {
        let before = source(&state);
        apply(
            &mut state,
            20 + step as u64,
            (start..(start + 1000).min(9990))
                .map(|id| GraphEntry::DeleteEdge {
                    edge_id: EdgeId(id),
                })
                .collect(),
        );
        overlay.apply(projection(&before, &source(&state))).unwrap();
    }
    let view = CompactGraphView::with_overlay(overlay.snapshot(Arc::clone(&base)).unwrap());
    take_visits();
    let mut iterator = view.algorithm_adjacency(NodeId(0), Direction::Both, Some(("R", EdgeId(0))));
    assert!(take_visits() <= 16, "constructor must binary-position only");
    let expected: Vec<_> = state
        .graph(G)
        .unwrap()
        .adjacency(NodeId(0), Direction::Both, None, Some(("R", EdgeId(0))))
        .collect();
    let mut actual = Vec::new();
    loop {
        let entry = iterator.next();
        assert!(take_visits() <= 2, "next must not walk tombstones");
        match entry {
            Some(entry) => actual.push(entry),
            None => break,
        }
    }
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 10);
    take_visits();
    assert_eq!(view.algorithm_node_by_key("hub").unwrap().id, NodeId(0));
    assert!(take_visits() <= 3);
    take_visits();
    assert!(view.algorithm_node_by_key("missing").is_none());
    assert!(take_visits() <= 3);
    assert_eq!(view.algorithm.bytes(), Index::projected_bytes(2, 2, 10));
}

#[test]
fn synthetic_index_bytes_stay_inside_incremental_envelope() {
    let nodes = 100_000usize;
    let edges = 1_000_000usize;
    let header = Index::empty().bytes();
    let bytes = Index::projected_bytes(nodes, nodes, edges);
    assert_eq!(header, 88);
    assert_eq!(bytes, 18_400_088);
    assert!(bytes <= 20 * edges);
    let source_rows = edges * std::mem::size_of::<SourceEdge>();
    let edge_rows = edges * std::mem::size_of::<EdgeRow>();
    let csr = (2 * edges + 2 * (nodes + 1)) * std::mem::size_of::<u32>();
    let node_rows = nodes * std::mem::size_of::<SourceNode>();
    let strings = edges + 8 * nodes + 1 + 9; // Edge relationships, node keys, dictionary, graph name.
    let dictionaries = std::mem::size_of::<String>(); // One relationship; no labels or property names.
    let ordinal_payload = nodes * (std::mem::size_of::<u64>() + std::mem::size_of::<u32>());
    let baseline = source_rows
        + edge_rows
        + csr
        + node_rows
        + strings
        + dictionaries
        + ordinal_payload
        + std::mem::size_of::<CompactGraph>()
        + 2 * std::mem::size_of::<usize>();
    assert_eq!(std::mem::size_of::<SourceEdge>(), 144);
    println!(
        "index_bytes={bytes}; added_overlay_side_bytes=0; tests_only_decoded_counted_payload_bytes={baseline}; E={edges}; N={nodes}; allocator_metadata_and_BTree_slack_excluded"
    );
}

#[test]
fn indexed_key_lookup_visits_are_logarithmic() {
    let mut state = GraphState::new();
    apply(
        &mut state,
        1,
        vec![GraphEntry::CreateGraph {
            name: "indexed_keys".into(),
        }],
    );
    for start in (0..4096).step_by(1024) {
        apply(
            &mut state,
            2 + start / 1024,
            (start..start + 1024)
                .map(|id| GraphEntry::PutNode {
                    node_id: NodeId(id),
                    key: format!("n{id:07}"),
                    labels: BTreeSet::new(),
                    properties: Properties::new(),
                })
                .collect(),
        );
    }
    let view = base_view(&state);
    for key in ["n0000000", "n0002048", "n0004095", "missing", "n9999999"] {
        take_visits();
        assert_eq!(
            view.algorithm_node_by_key(key).map(|node| node.id),
            state.graph(G).unwrap().node_by_key(key).map(|node| node.id)
        );
        assert!(
            take_visits() <= 14,
            "key lookup must binary-search, never scan"
        );
    }
}

#[test]
fn seeded_algorithm_histories_at_every_split() {
    for seed in seeded::SEEDS {
        let states = seeded::history(seed);
        for cut in 0..states.len() {
            let base = Arc::new(decode(&encode(&source(&states[cut])).unwrap()).unwrap());
            let mut overlay = Overlay::new(&base, 128);
            algorithm_parity(
                &CompactGraphView::new(Arc::clone(&base)),
                states[cut].graph(G).unwrap(),
            );
            for step in cut + 1..states.len() {
                overlay
                    .apply(projection(
                        &source(&states[step - 1]),
                        &source(&states[step]),
                    ))
                    .unwrap();
                algorithm_parity(
                    &CompactGraphView::with_overlay(overlay.snapshot(Arc::clone(&base)).unwrap()),
                    states[step].graph(G).unwrap(),
                );
            }
        }
    }
}
