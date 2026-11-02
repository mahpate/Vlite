use super::*;
use serde_json::json;

fn pool(workers: usize) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()
        .unwrap()
}

fn options(partitions: usize, layout: Layout, run_size: usize) -> Options {
    Options {
        partitions,
        layout,
        run_size,
    }
}

fn golden_cases() -> Vec<Value> {
    let document: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/compact_graph_import/golden.json"
    ))
    .unwrap();
    assert_eq!(document["schema"], "vlite.compact-graph-import-logical");
    assert_eq!(document["version"], 1);
    let cases = document["cases"].as_array().unwrap().clone();
    assert_eq!(cases.len(), 19);
    cases
}

fn logical(graph: &CompactGraph) -> Value {
    let adjacency = |csr: &Csr| {
        csr.offsets
            .windows(2)
            .map(|pair| {
                csr.edges[pair[0] as usize..pair[1] as usize]
                    .iter()
                    .map(|ordinal| {
                        let edge = &graph.source.edges[*ordinal as usize];
                        json!([edge.relationship, edge.id])
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    };
    json!({"image": {
        "nodes": graph.source.nodes.iter().map(|node| json!({
            "id": node.id, "key": node.key, "labels": node.labels,
            "deadline": node.deadline,
            "attachments": node.attachments.iter()
                .map(|(collection, record)| json!([collection, record])).collect::<Vec<_>>()
        })).collect::<Vec<_>>(),
        "edges": graph.source.edges.iter().map(|edge| json!({
            "id": edge.id, "source": edge.source, "target": edge.target,
            "relationship": edge.relationship, "deadline": edge.deadline
        })).collect::<Vec<_>>(),
        "adjacency": {"out": adjacency(&graph.out), "in": adjacency(&graph.incoming)},
        "degree_prefix_sums": {"out": graph.out.offsets, "in": graph.incoming.offsets},
        "dictionaries": {"labels": graph.labels, "relationships": graph.relationships},
        "next_node_id": graph.source.next_node_id,
        "next_edge_id": graph.source.next_edge_id
    }})
}

fn result_value(result: &ImportResult<Image>) -> Value {
    match result {
        Ok(image) => logical(&image.graph),
        Err(error) => json!({"error": {"class": error.kind, "ordinal": error.ordinal}}),
    }
}

// Independent ordered admission baseline; no partition or run machinery.
// The existing codec builds and sorts its own CSR from this sequential source.
fn sequential(nodes: &[Value], edges: &[Value]) -> ImportResult<CompactGraph> {
    let mut admitted = Vec::<SourceNode>::new();
    let mut owners = BTreeSet::new();
    for (ordinal, row) in nodes.iter().enumerate() {
        let mut node = decode_node(row, ordinal)?;
        if node.source.key.is_some() && admitted.iter().any(|other| other.key == node.source.key) {
            return Err(failure("NodeKeyExists", ordinal));
        }
        for (collection, record) in node.attachments {
            if node.source.attachments.contains_key(&collection) {
                return Err(failure("NodeAlreadyAttached", ordinal));
            }
            if !owners.insert((collection, record)) {
                return Err(failure("RecordAlreadyAttached", ordinal));
            }
            node.source.attachments.insert(collection, record);
        }
        admitted.push(node.source);
    }
    let mut admitted_edges = Vec::new();
    for (id, row) in edges.iter().enumerate() {
        let ordinal = nodes.len() + id;
        let edge = decode_edge(row, ordinal)?;
        let resolve = |endpoint: &Endpoint| match endpoint {
            Endpoint::Key(key) => admitted
                .iter()
                .find(|node| node.key.as_ref() == Some(key))
                .map(|node| node.id)
                .ok_or_else(|| failure("UnknownNodeKey", ordinal)),
            Endpoint::Ordinal(id) => admitted
                .get(usize::try_from(*id).unwrap_or(usize::MAX))
                .map(|node| node.id)
                .ok_or_else(|| failure("NodeNotFound", ordinal)),
        };
        admitted_edges.push(SourceEdge {
            id: id as u64,
            source: resolve(&edge.source)?,
            target: resolve(&edge.target)?,
            relationship: edge.relationship,
            key: None,
            revision: 1,
            deadline: edge.deadline,
            properties: Properties::new(),
            segments: None,
        });
    }
    build(GraphSource {
        graph_id: 0,
        name: "import".into(),
        sequence: 1,
        next_node_id: nodes.len() as u64,
        next_edge_id: edges.len() as u64,
        temporal_enabled: false,
        schema: GraphSchema::default(),
        nodes: admitted,
        edges: admitted_edges,
    })
    .map_err(|_| failure("Capacity", 0))
}

fn parity(nodes: &[Value], edges: &[Value], options: Options, pool: &rayon::ThreadPool) {
    let baseline = sequential(nodes, edges);
    let actual = stage(nodes, edges, options, pool);
    match (actual, baseline) {
        (Ok(actual), Ok(expected)) => {
            assert_eq!(actual.graph, expected);
            assert_eq!(actual.bytes, encode(&expected.source).unwrap());
            assert_eq!(decode(&actual.bytes).unwrap(), expected);
        }
        (Err(actual), Err(expected)) => assert_eq!(actual, expected),
        (actual, expected) => panic!(
            "admission mismatch: actual={:?}, expected={:?}",
            actual.err(),
            expected.err()
        ),
    }
}

#[test]
fn all_import_goldens_worker_layout_run_parity() {
    let cases = golden_cases();
    for workers in [1, 2, 3, 4, 7, 16] {
        let pool = pool(workers);
        for layout in [Layout::Balanced, Layout::FrontHeavy, Layout::BackHeavy] {
            for run_size in [1, 2, 3, usize::MAX] {
                // Partition count is deliberately not tied to thread count.
                for partitions in [workers, workers + 1] {
                    for case in &cases {
                        let nodes = case["nodes"].as_array().unwrap();
                        let edges = case["edges"].as_array().unwrap();
                        let options = options(partitions, layout, run_size);
                        let actual = stage(nodes, edges, options, &pool);
                        assert_eq!(
                            result_value(&actual),
                            case["expected"],
                            "{} W={workers} P={partitions} {layout:?} R={run_size}",
                            case["name"]
                        );
                        parity(nodes, edges, options, &pool);
                    }
                }
            }
        }
    }
}

struct Prng(u64);

impl Prng {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 as usize % bound
    }
}

fn seeded(seed: u64) -> (Vec<Value>, Vec<Value>) {
    let mut rng = Prng(seed);
    let nodes = (0..23)
        .map(|id| {
            let mut row = json!({
                "labels": [format!("L{}", rng.below(5))],
                "deadline": ([None, Some(0), Some(7), Some(u64::MAX)][rng.below(4)]),
                "attachments": [{"collection": id % 4, "record": id * 101}]
            });
            if id % 3 != 1 {
                row["key"] = json!(format!("sparse-{}", id * 1009));
            }
            row
        })
        .collect();
    let edges = (0..129)
        .map(|id| {
            let source = if id % 5 == 0 { rng.below(23) } else { 0 };
            let target = if id % 7 == 0 { source } else { rng.below(23) };
            let endpoint = |node: usize| {
                if node % 3 != 1 && id % 2 == 0 {
                    json!({"key": format!("sparse-{}", node * 1009)})
                } else {
                    json!({"ordinal": node})
                }
            };
            json!({
                "source": endpoint(source), "target": endpoint(target),
                "relationship": (["z", "a", "m"][rng.below(3)]),
                "deadline": ([None, Some(0), Some(u64::MAX)][rng.below(3)])
            })
        })
        .collect();
    (nodes, edges)
}

#[test]
fn seeded_skew_and_conflict_first_error_parity() {
    for workers in [1, 2, 3, 4, 7, 16] {
        let pool = pool(workers);
        for seed in [1, 0x1234_5678, 0x9e37_79b9, 0xd192_ed03, 0x28db_7537] {
            let (nodes, edges) = seeded(seed);
            for layout in [Layout::Balanced, Layout::FrontHeavy, Layout::BackHeavy] {
                for run in [1, 2, 3, 17, usize::MAX] {
                    let options = options(workers + 2, layout, run);
                    parity(&nodes, &edges, options, &pool);
                    for variant in 0..6 {
                        let mut nodes = nodes.clone();
                        let mut edges = edges.clone();
                        match variant {
                            0 => nodes[17]["key"] = nodes[0]["key"].clone(),
                            1 => nodes[9]["attachments"] = nodes[2]["attachments"].clone(),
                            2 => {
                                nodes[4]["attachments"] = json!([
                                    {"collection": 2, "record": 900},
                                    {"collection": 2, "record": 900}
                                ])
                            }
                            3 => {
                                edges[3]["source"] = json!({"key": "missing"});
                                edges[1]["target"] = json!({"ordinal": u64::MAX});
                            }
                            4 => {
                                nodes[13]["key"] = nodes[0]["key"].clone();
                                nodes[1]["attachments"] = nodes[0]["attachments"].clone();
                                edges[0]["source"] = json!({"key": "missing"});
                            }
                            _ => {
                                nodes[1]["key"] = nodes[0]["key"].clone();
                                nodes[2] = json!({"labels": 42});
                            }
                        }
                        parity(&nodes, &edges, options, &pool);
                    }
                }
            }
        }
    }
}

#[test]
fn import_matches_engine_for_representable_sources_and_explicit_key_remap() {
    let pool = pool(4);
    for seed in [1, 0x1234_5678, 0x9e37_79b9] {
        let (mut nodes, edges) = seeded(seed);
        let original = stage(&nodes, &edges, options(7, Layout::BackHeavy, 2), &pool).unwrap();
        assert!(
            original
                .graph
                .source
                .nodes
                .iter()
                .any(|node| node.key.is_none())
        );
        assert!(matches!(
            engine_state(&original.graph.source),
            Err(CodecError::UnsupportedSource(_))
        ));
        // Anonymous logical nodes are not engine nodes. Explicitly name them
        // only for this parity comparison; never claim unchanged-source parity.
        for (ordinal, row) in nodes.iter_mut().enumerate() {
            if row.get("key").is_none() {
                row["key"] = json!(format!("remapped-{ordinal}"));
            }
        }
        let image = stage(&nodes, &edges, options(7, Layout::FrontHeavy, 3), &pool).unwrap();
        let mut entries = vec![GraphEntry::CreateGraph {
            name: "import".into(),
        }];
        for node in &image.graph.source.nodes {
            entries.push(GraphEntry::PutNode {
                node_id: NodeId(node.id),
                key: node.key.clone().unwrap(),
                labels: node.labels.iter().cloned().collect(),
                properties: Properties::new(),
            });
            if let Some(deadline) = node.deadline {
                entries.push(GraphEntry::NodeExpiry {
                    node_id: NodeId(node.id),
                    expires_at_ms: Some(deadline),
                });
            }
            for (collection, record) in &node.attachments {
                entries.push(GraphEntry::Attach {
                    node_id: NodeId(node.id),
                    collection: *collection,
                    record_id: *record,
                });
            }
        }
        for edge in &image.graph.source.edges {
            entries.push(GraphEntry::PutEdge {
                edge_id: EdgeId(edge.id),
                source: NodeId(edge.source),
                destination: NodeId(edge.target),
                relationship: edge.relationship.clone(),
                key: None,
                properties: Properties::new(),
            });
            if let Some(deadline) = edge.deadline {
                entries.push(GraphEntry::EdgeExpiry {
                    edge_id: EdgeId(edge.id),
                    expires_at_ms: Some(deadline),
                });
            }
        }
        let mut state = GraphState::new();
        state
            .apply(
                1,
                &GraphBody {
                    graph_id: GraphId(0),
                    entries,
                },
            )
            .unwrap();
        let source = GraphSource::from_graph(state.graph(GraphId(0)).unwrap()).unwrap();
        assert_eq!(image.graph.source, source);
        assert_eq!(image.graph, build(source.clone()).unwrap());
        assert_eq!(image.bytes, encode(&source).unwrap());
    }
}

#[test]
fn rejects_unsupported_admission_and_invalid_configuration() {
    let pool = pool(2);
    for field in ["properties", "schema", "segments", "extra"] {
        let nodes = [json!({"labels": [], field: {}})];
        assert_eq!(
            stage(&nodes, &[], options(3, Layout::Balanced, 1), &pool).err(),
            Some(failure("UnsupportedInput", 0))
        );
    }
    for options in [
        options(0, Layout::Balanced, 1),
        options(257, Layout::Balanced, 1),
        options(1, Layout::Balanced, 0),
    ] {
        assert_eq!(
            stage(&[], &[], options, &pool).err(),
            Some(failure("InvalidOptions", 0))
        );
    }
    for row in [
        json!({"labels": ["a", "a"]}),
        json!({"labels": [], "key": ""}),
        json!({"labels": [], "deadline": -1}),
        json!({"labels": [], "attachments": [{"collection": 65536, "record": 0}]}),
    ] {
        assert_eq!(
            stage(&[row], &[], options(3, Layout::BackHeavy, 1), &pool).err(),
            Some(failure("InvalidInput", 0))
        );
    }
}

#[test]
fn partition_layouts_cover_every_ordinal_once() {
    for length in 0..100 {
        for workers in [1, 2, 3, 7, 16, 256] {
            for layout in [Layout::Balanced, Layout::FrontHeavy, Layout::BackHeavy] {
                assert_eq!(
                    partitions(length, options(workers, layout, 1))
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>(),
                    (0..length).collect::<Vec<_>>()
                );
                for run_size in [1, 3, 17, usize::MAX] {
                    let ranges = encoding_ranges(length, options(workers, layout, run_size));
                    assert!(ranges.iter().all(|range| range.len() <= run_size));
                    assert_eq!(
                        ranges.into_iter().flatten().collect::<Vec<_>>(),
                        (0..length).collect::<Vec<_>>()
                    );
                }
            }
        }
    }
}
