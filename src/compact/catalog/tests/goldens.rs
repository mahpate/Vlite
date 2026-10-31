//! The independent reference payloads
//! (`tests/fixtures/compact/precision-catalog-cases.json`, checked by
//! `tests/precision_catalog_oracle.py`) through the real reader: each op-17
//! body is framed and appended after an initial precision image, and each
//! kind-3/4/5 section replaces its block in the image of a store that holds
//! the case's collections, dropped bindings or retained graph entities.
//! The fixtures cover raw payloads only; framing, encryption and recovery
//! are the other tests'.
//!
//! A fixture `schema_version` is the block's version, which the capability
//! table declares: an unknown one is `UnsupportedFeature(FLAG_COMPACT)`.
//! Unknown catalog opt-in or precision values are
//! `UnsupportedFeature(FLAG_PRECISION)`; anything malformed is corrupt.

use super::*;
use crate::property_graph::EdgeRef;
use serde_json::Value;

const CASES: &str = include_str!("../../../../tests/fixtures/compact/precision-catalog-cases.json");

fn hex(value: &Value) -> Vec<u8> {
    let text: String = value
        .as_str()
        .unwrap()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
        .collect()
}

fn number(value: &Value) -> u64 {
    value.as_u64().unwrap()
}

fn ids(value: &Value) -> Vec<u64> {
    value.as_array().unwrap().iter().map(number).collect()
}

/// One revision block: partition, frame count, node and edge sections.
struct Partition {
    partition: u64,
    count: u32,
    nodes: Vec<u8>,
    edges: Vec<u8>,
}

/// A case's input bytes after overrides and edits.
struct Input {
    version: u16,
    count: u32,
    /// The op-17 body or the catalog/registry section.
    bytes: Vec<u8>,
    partitions: Vec<Partition>,
}

impl Input {
    fn parse(value: &Value) -> Self {
        let bytes = value
            .get("body_hex")
            .or_else(|| value.get("section_hex"))
            .map(hex)
            .unwrap_or_default();
        let partitions = value.get("partitions").map_or_else(Vec::new, |list| {
            list.as_array()
                .unwrap()
                .iter()
                .map(|partition| Partition {
                    partition: number(&partition["partition"]),
                    count: number(&partition["frame_count"]) as u32,
                    nodes: hex(&partition["nodes_hex"]),
                    edges: hex(&partition["edges_hex"]),
                })
                .collect()
        });
        Self {
            version: value.get("schema_version").map_or(1, number) as u16,
            count: value.get("frame_count").map_or(0, number) as u32,
            bytes,
            partitions,
        }
    }

    /// Replaces bytes at an offset (extending past the end) or truncates.
    fn edit(&mut self, edit: &Value) {
        let target = edit["target"].as_str().unwrap();
        let bytes = match target {
            "body" | "section" => &mut self.bytes,
            _ => {
                let (index, class) = target.split_once('.').unwrap();
                let partition = &mut self.partitions[index.parse::<usize>().unwrap()];
                match class {
                    "nodes" => &mut partition.nodes,
                    "edges" => &mut partition.edges,
                    other => panic!("edit target {other}"),
                }
            }
        };
        if let Some(length) = edit.get("length") {
            bytes.truncate(number(length) as usize);
            return;
        }
        let offset = number(&edit["offset"]) as usize;
        let patch = hex(&edit["hex"]);
        if bytes.len() < offset + patch.len() {
            bytes.resize(offset + patch.len(), 0);
        }
        bytes[offset..offset + patch.len()].copy_from_slice(&patch);
    }
}

/// Live collections (`live_collections`, `default` at ID 0) and dropped
/// bindings of a case context; IDs up to the highest are created in
/// order (gaps as `c<ID>`) and every one that is not live is dropped.
fn collections(context: &Value) -> Loaded {
    let list = |key: &str| {
        context
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let (live, dropped) = (list("live_collections"), list("dropped_bindings"));
    let dimensions = |entry: &Value| entry.get("dimensions").map_or(1, number) as usize;
    let mut loaded = initial(None);
    if let Some(default) = live.iter().find(|entry| number(&entry["id"]) == 0) {
        loaded.dimensions = default.get("dimensions").map_or(2, number) as usize;
        loaded.collections.get_mut(&0).unwrap().dimensions = loaded.dimensions;
    }
    let high = live
        .iter()
        .chain(&dropped)
        .map(|entry| number(&entry["id"]))
        .max()
        .unwrap_or(0);
    let mut log = image_of(&loaded);
    let settings = CollectionSettings::default();
    let mut sequence = 1;
    let mut gone = Vec::new();
    for id in 1..=high as u16 {
        let found = |entries: &[Value]| {
            entries
                .iter()
                .find(|entry| number(&entry["id"]) == u64::from(id))
                .cloned()
        };
        let (entry, live) = match (found(&live), found(&dropped)) {
            (Some(entry), _) => (Some(entry), true),
            (None, entry) => (entry, false),
        };
        let name = entry.as_ref().map_or(format!("c{id}"), |entry| {
            entry["name"].as_str().unwrap().to_owned()
        });
        let size = entry.as_ref().map_or(1, dimensions);
        sequence += 1;
        log.extend(format::precise_create_frame(
            sequence, id, &name, size, &settings, F32,
        ));
        if !live {
            gone.push(id);
        }
    }
    for id in gone {
        sequence += 1;
        log.extend(format::collection_drop_frame(sequence, id));
    }
    load(&log, None).unwrap()
}

/// The op-17 body after an initial image, framed for `collection_id`.
fn op17(input: &Input, context: &Value) -> Vec<u8> {
    let id = number(&context["collection_id"]) as u16;
    let settings = CollectionSettings::default();
    let create = format::precise_create_frame(2, id, "x", 1, &settings, F32);
    let mut log = image_of(&initial(None));
    log.extend(refit(&create, |bytes| {
        bytes.truncate(PREFIX_SIZE);
        bytes.extend_from_slice(&input.bytes);
    }));
    log
}

fn section_case(kind: u16, input: &Input, context: &Value) -> Vec<u8> {
    let loaded = collections(context);
    let block = block_frame(loaded.sequence, kind, 0, 0, input.count, &[&input.bytes]);
    replaced(&loaded, kind, input.version, vec![block])
}

/// A store whose graph `g` retains exactly the context's node and edge
/// IDs (every edge a loop on the first retained node), imaged at the
/// context floor, with the case's revision blocks. An empty graph keeps
/// its own head, as creating the graph is a commit.
fn graph_case(input: &Input, context: &Value) -> Vec<u8> {
    let (nodes, edges) = (
        ids(&context["retained_nodes"]),
        ids(&context["retained_edges"]),
    );
    let mut db = Database::open_from_bytes(image_of(&initial(None)), None).unwrap();
    db.create_graph("g").unwrap();
    let node_count = nodes.iter().max().map_or(0, |high| high + 1);
    let edge_count = edges.iter().max().map_or(0, |high| high + 1);
    let mut create: Vec<GraphOp> = (0..node_count)
        .map(|id| GraphOp::CreateNode {
            key: format!("n{id}"),
            labels: Vec::new(),
            properties: Metadata::new(),
        })
        .collect();
    let anchor = format!("n{}", nodes.first().copied().unwrap_or(0));
    create.extend((0..edge_count).map(|id| GraphOp::CreateEdge {
        key: Some(format!("e{id}")),
        source: anchor.clone(),
        relationship: "r".into(),
        destination: anchor.clone(),
        properties: Metadata::new(),
    }));
    let mut delete: Vec<GraphOp> = (0..edge_count)
        .filter(|id| !edges.contains(id))
        .map(|id| GraphOp::DeleteEdge {
            edge: EdgeRef::Id(id),
            expected: Expected::Any,
        })
        .collect();
    delete.extend(
        (0..node_count)
            .filter(|id| !nodes.contains(id))
            .map(|id| GraphOp::DeleteNode {
                key: format!("n{id}"),
                expected: Expected::Any,
            }),
    );
    let mut graph = db.graph_mut("g").unwrap();
    for ops in [create, delete] {
        if !ops.is_empty() {
            graph.write_batch(&ops).unwrap();
        }
    }
    let view = db.graph("g").unwrap();
    for &id in &nodes {
        assert!(
            view.get_node_by_id(id).unwrap().output.is_some(),
            "node {id}"
        );
    }
    for &id in &edges {
        assert!(view.get_edge(id).unwrap().output.is_some(), "edge {id}");
    }
    let mut loaded = load(&db.to_bytes().unwrap(), None).unwrap();
    let floor = number(&context["floor"]);
    if !nodes.is_empty() || !edges.is_empty() {
        assert!(loaded.sequence <= floor);
    }
    loaded.sequence = loaded.sequence.max(floor);
    let (_, frames) = split(&image_of(&loaded));
    let owner = frames
        .iter()
        .find(|frame| is_block(frame, KIND_REVISIONS))
        .map(|frame| u16_at(frame, 5))
        .unwrap();
    let blocks = input
        .partitions
        .iter()
        .map(|part| {
            block_frame(
                loaded.sequence,
                KIND_REVISIONS,
                owner,
                part.partition,
                part.count,
                &[&part.nodes, &part.edges],
            )
        })
        .collect();
    replaced(&loaded, KIND_REVISIONS, input.version, blocks)
}

fn run(kind: &str, input: &Input, context: &Value) -> Vec<u8> {
    match kind {
        "op17" => op17(input, context),
        "catalog" => section_case(KIND_CATALOG, input, context),
        "imports" => section_case(KIND_IMPORTS, input, context),
        "graph" => graph_case(input, context),
        other => panic!("case kind {other}"),
    }
}

fn precision(code: &Value) -> Precision {
    Precision::from_code(number(code) as u8).unwrap()
}

/// Checks the restored state of an accepted case against its expectation,
/// and that the writer restates the same payload.
fn check_accepted(kind: &str, input: &Input, context: &Value, expected: &Value, bytes: Vec<u8>) {
    let loaded = load(&bytes, None).unwrap();
    let (_, frames) = split(&image_of(&loaded));
    let restated = |kind: u16| {
        let frame = frames.iter().find(|frame| is_block(frame, kind)).unwrap();
        sections(frame).remove(0)
    };
    match kind {
        "op17" => {
            let collection = &loaded.collections[&(number(&context["collection_id"]) as u16)];
            assert_eq!(collection.name, expected["name"].as_str().unwrap());
            assert_eq!(
                collection.dimensions as u64,
                number(&expected["dimensions"])
            );
            assert_eq!(
                collection.precision,
                Some(precision(&expected["precision"]))
            );
            let settings = &expected["settings"];
            let actual = &collection.settings;
            assert_eq!(
                [
                    actual.hnsw.m as u64,
                    actual.hnsw.ef_construction as u64,
                    actual.hnsw.seed,
                    u64::from(actual.rerank_multiplier),
                    u64::from(actual.planner.exact_max_candidates),
                    u64::from(actual.planner.exact_max_selectivity_ppm),
                ],
                [
                    "m",
                    "ef_construction",
                    "seed",
                    "rerank_multiplier",
                    "exact_max_candidates",
                    "exact_max_selectivity_ppm",
                ]
                .map(|field| number(&settings[field]))
            );
            assert_eq!(*actual, CollectionSettings::default());
        }
        "catalog" => {
            let entries = expected["entries"].as_array().unwrap();
            for entry in entries {
                let collection = &loaded.collections[&(number(&entry["id"]) as u16)];
                assert_eq!(collection.precision, Some(precision(&entry["precision"])));
                assert_eq!(collection.dimensions as u64, number(&entry["dimensions"]));
            }
            assert_eq!(loaded.collections.len(), entries.len());
            assert_eq!(restated(KIND_CATALOG), input.bytes);
        }
        "imports" => {
            let db = Database::open_from_bytes(bytes, None).unwrap();
            for job in expected["jobs"].as_array().unwrap() {
                let status = db.import_status(number(&job["job"])).unwrap();
                let state = match number(&job["state"]) {
                    0 => ImportState::Open,
                    1 => ImportState::Finished,
                    _ => ImportState::Aborted,
                };
                assert_eq!(
                    (
                        status.collection.as_str(),
                        u64::from(status.collection_dropped),
                        status.state,
                        status.last_chunk,
                        status.rows_committed,
                        status.rows_skipped,
                        status.last_digest.to_vec(),
                    ),
                    (
                        job["name"].as_str().unwrap(),
                        number(&job["dropped"]),
                        state,
                        number(&job["last_chunk"]),
                        number(&job["committed"]),
                        number(&job["skipped"]),
                        hex(&job["digest_hex"]),
                    )
                );
            }
            assert_eq!(
                db.next_import_job_id().unwrap(),
                number(&expected["max_job"]) + 1
            );
            assert_eq!(restated(KIND_IMPORTS), input.bytes);
        }
        "graph" => {
            let db = Database::open_from_bytes(bytes, None).unwrap();
            let graph = db.graph("g").unwrap();
            let (mut nodes, mut edges) = (Vec::new(), Vec::new());
            for partition in expected["partitions"].as_array().unwrap() {
                let pairs = |class: &str| {
                    partition[class]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|pair| (number(&pair["id"]), number(&pair["revision"])))
                        .collect::<Vec<_>>()
                };
                nodes.extend(pairs("nodes"));
                edges.extend(pairs("edges"));
            }
            for &(id, revision) in &nodes {
                let node = graph.get_node_by_id(id).unwrap().output.unwrap();
                assert_eq!(node.revision, revision, "node {id}");
            }
            for &(id, revision) in &edges {
                let edge = graph.get_edge(id).unwrap().output.unwrap();
                assert_eq!(edge.revision, revision, "edge {id}");
            }
            // The writer packs the same entries into full blocks.
            let (mut written_nodes, mut written_edges) = (Vec::new(), Vec::new());
            for frame in frames
                .iter()
                .filter(|frame| is_block(frame, KIND_REVISIONS))
            {
                let parts = sections(frame);
                let count = u32_at(frame, 24);
                let (more_nodes, more_edges) =
                    decode_revisions(&[&parts[0], &parts[1]], count, loaded.sequence, 0).unwrap();
                written_nodes.extend(more_nodes);
                written_edges.extend(more_edges);
            }
            assert_eq!((written_nodes, written_edges), (nodes, edges));
        }
        other => panic!("case kind {other}"),
    }
}

#[test]
fn reference_payloads_decode_exactly_through_the_reader() {
    let _gate = Activation::on();
    let fixture: Value = serde_json::from_str(CASES).unwrap();
    assert_eq!(fixture["schema"], "vlite.precision-catalog-golden");
    assert_eq!(fixture["version"], 1);
    let cases = fixture["cases"].as_array().unwrap();
    let mut failures = Vec::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let kind = case["kind"].as_str().unwrap();
        let input = Input::parse(&case["input"]);
        let bytes = run(kind, &input, &case["context"]);
        match outcome(load(&bytes, None)) {
            ok if ok == "ok" => {
                check_accepted(kind, &input, &case["context"], &case["expected"], bytes);
            }
            actual => failures.push(format!("{name}: {actual}, expected ok")),
        }
    }

    let invalid = fixture["invalid_cases"].as_array().unwrap();
    for case in invalid {
        let name = case["name"].as_str().unwrap();
        let base = cases
            .iter()
            .find(|base| base["name"] == case["base"])
            .unwrap();
        let mut input = base["input"].clone();
        for (key, value) in case["input_override"].as_object().unwrap() {
            input[key] = value.clone();
        }
        let mut input = Input::parse(&input);
        for edit in case["edits"].as_array().unwrap() {
            input.edit(edit);
        }
        let context = match &case["context_override"] {
            Value::Null => &base["context"],
            context => context,
        };
        let expected = match case["error"].as_str().unwrap() {
            "malformed" => "corrupt",
            _ if case["input_override"].get("schema_version").is_some() => "unsupported 16",
            _ => "unsupported 32",
        };
        let kind = base["kind"].as_str().unwrap();
        let actual = outcome(load(&run(kind, &input, context), None));
        if actual != expected {
            failures.push(format!("{name}: {actual}, expected {expected}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    assert_eq!((cases.len(), invalid.len()), (10, 79));
}
