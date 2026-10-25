//! Shared EPIC-006 test helpers (`graph` feature only): a graph workload that
//! touches every graph entry kind, and a canonical snapshot of everything
//! observable through the public API.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use vlite::maintenance;
use vlite::property_graph::{
    EdgeRef, EdgeView, FindNodes, GraphCounts, GraphMut, GraphOp, GraphRef, Neighbors, NodePatch,
    NodeSelector, NodeView, ReadOptions,
};
use vlite::{
    CollectionInfo, CollectionSettings, CollectionSpec, CreateOptions, Database, EncryptionKey,
    Error, Expected, FormatVersion, Metadata, MetadataValue, Mutation, Record,
};

pub const FLAG_ENCRYPTED: u16 = 1;
pub const FLAG_TIMESTAMPS: u16 = 2;
pub const FLAG_GRAPH: u16 = 4;

pub fn key() -> EncryptionKey {
    EncryptionKey::from_bytes([7; 32])
}

pub fn other_key() -> EncryptionKey {
    EncryptionKey::from_bytes([9; 32])
}

/// The three file kinds TEST-004 and TEST-008 cover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Plain,
    Encrypted,
    Timestamped,
}

pub const KINDS: [Kind; 3] = [Kind::Plain, Kind::Encrypted, Kind::Timestamped];

impl Kind {
    pub fn key(self) -> Option<EncryptionKey> {
        (self == Kind::Encrypted).then(key)
    }

    pub fn options(self) -> CreateOptions {
        CreateOptions {
            format: Some(FormatVersion::V5),
            encryption: self.key(),
            commit_timestamps: self == Kind::Timestamped,
        }
    }

    /// Header flags of a graph-enabled file of this kind.
    pub fn graph_flags(self) -> u16 {
        FLAG_GRAPH
            | match self {
                Kind::Plain => 0,
                Kind::Encrypted => FLAG_ENCRYPTED,
                Kind::Timestamped => FLAG_TIMESTAMPS,
            }
    }
}

pub fn open(path: &Path, key: Option<&EncryptionKey>) -> Database {
    match key {
        Some(key) => Database::open_encrypted(path, key),
        None => Database::open(path),
    }
    .unwrap()
}

/// Committed sequence of a closed file (existing `inspect*`).
pub fn commits(path: &Path, key: Option<&EncryptionKey>) -> u64 {
    match key {
        Some(key) => maintenance::inspect_with_key(path, key),
        None => maintenance::inspect(path),
    }
    .unwrap()
    .commits
}

pub fn header_flags(path: &Path) -> u16 {
    let bytes = fs::read(path).unwrap();
    u16::from_le_bytes([bytes[26], bytes[27]])
}

/// Deterministic commit clock: commit `n` is recorded at `1000 + 100 * n`
/// milliseconds, given that `sequence` commits already exist.
pub fn clock(db: &mut Database, sequence: u64) {
    let mut now = 1_000 + 100 * sequence;
    db.set_commit_clock(Box::new(move || {
        now += 100;
        now
    }));
}

/// The complete committed byte image of an open file-backed database
/// (header and every frame, as a bootstrap log segment carries them).
pub fn live_bytes(db: &Database) -> Vec<u8> {
    let segment = db.export_log(0, usize::MAX).unwrap();
    let mut bytes = segment.header;
    bytes.extend_from_slice(&segment.frames);
    bytes
}

/// Node and edge IDs the next writes to graph `alpha` must receive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HighWater {
    pub node: u64,
    pub edge: u64,
}

/// `alpha`'s high-water marks after `populate`: its highest node (4) and
/// edge (5) were deleted again, so copies must not assign them twice.
pub const ALPHA_HIGH_WATER: HighWater = HighWater { node: 5, edge: 6 };

/// `populate` counts as `[graphs, nodes, edges, attachments]`.
pub const POPULATED: [usize; 4] = [3, 6, 6, 4];

/// Graph-enabled file of `kind` holding the `populate` workload.
pub fn graph_file(dir: &Path, name: &str, kind: Kind) -> PathBuf {
    let path = dir.join(format!("{name}-{kind:?}.vlite"));
    let mut db = Database::create_with_graph(&path, 2, kind.options()).unwrap();
    clock(&mut db, 0);
    populate(&mut db);
    db.close().unwrap();
    path
}

fn text(value: &str) -> MetadataValue {
    MetadataValue::String(value.into())
}

/// Vector-only V5 file of `kind`: two collections, metadata, an expiring
/// record and a delete.
pub fn vector_file(dir: &Path, name: &str, kind: Kind) -> PathBuf {
    let path = dir.join(format!("{name}-{kind:?}.vlite"));
    let mut db = Database::create_with_options(&path, 2, kind.options()).unwrap();
    clock(&mut db, 0);
    populate_vectors(&mut db);
    db.close().unwrap();
    path
}

/// The vector records of `populate` (V4 or V5; the expiring record needs V5).
pub fn populate_vectors(db: &mut Database) {
    db.create_collection(
        "docs",
        CollectionSpec {
            dimensions: 3,
            settings: CollectionSettings::default(),
        },
    )
    .unwrap();
    db.put(1, &[1.0, 2.0], b"one").unwrap();
    db.put(2, &[2.0, 1.0], b"two").unwrap();
    db.put(3, &[0.5, 0.5], b"three").unwrap();
    if db.format_version() >= 5 {
        db.put_with_expiry(
            8,
            &[8.0, 8.0],
            b"expiring",
            &Metadata::new(),
            Some(u64::MAX / 2),
        )
        .unwrap();
    }
    let mut docs = db.collection_mut("docs").unwrap();
    docs.put_with_metadata(
        10,
        &[1.0, 0.0, 0.0],
        b"d10",
        &Metadata::from([("lang".into(), text("en"))]),
    )
    .unwrap();
    docs.put(11, &[0.0, 1.0, 0.0], b"d11").unwrap();
    db.delete(3).unwrap();
}

fn graph<'a>(db: &'a mut Database, name: &str) -> GraphMut<'a> {
    db.graph_mut(name).unwrap()
}

pub fn populate(db: &mut Database) {
    populate_with(db, &mut |_| {});
}

/// Vector records in two collections and three live graphs with
/// non-contiguous IDs (a dropped graph) including an empty one; labels,
/// every property type, keyed, parallel and self-loop edges, node patches,
/// attach and detach, a graph batch with a vector mutation, and deleted
/// highest node and edge IDs. `step` runs after every commit.
pub fn populate_with(db: &mut Database, step: &mut dyn FnMut(&Database)) {
    db.create_collection(
        "docs",
        CollectionSpec {
            dimensions: 3,
            settings: CollectionSettings::default(),
        },
    )
    .unwrap();
    step(db);
    for (id, vector, payload) in [
        (1, [1.0, 2.0], b"one".as_slice()),
        (2, [2.0, 1.0], b"two"),
        (3, [0.5, 0.5], b"three"),
    ] {
        db.put(id, &vector, payload).unwrap();
        step(db);
    }
    db.put_with_expiry(
        8,
        &[8.0, 8.0],
        b"expiring",
        &Metadata::new(),
        Some(u64::MAX / 2),
    )
    .unwrap();
    step(db);
    db.collection_mut("docs")
        .unwrap()
        .put_with_metadata(
            10,
            &[1.0, 0.0, 0.0],
            b"d10",
            &Metadata::from([("lang".into(), text("en"))]),
        )
        .unwrap();
    step(db);
    db.collection_mut("docs")
        .unwrap()
        .put(11, &[0.0, 1.0, 0.0], b"d11")
        .unwrap();
    step(db);
    for name in ["alpha", "tmp", "beta", "empty"] {
        db.create_graph(name).unwrap();
        step(db);
    }
    db.drop_graph("tmp").unwrap();
    step(db);

    let ada = Metadata::from([
        ("name".into(), text("Ada")),
        ("age".into(), MetadataValue::Integer(36)),
        ("score".into(), MetadataValue::Float(0.5)),
        ("active".into(), MetadataValue::Boolean(true)),
        (
            "home".into(),
            MetadataValue::GeoPoint {
                latitude: 51.5,
                longitude: -0.1,
            },
        ),
    ]);
    graph(db, "alpha")
        .create_node("ada", &["Person", "Engineer"], &ada)
        .unwrap();
    step(db);
    graph(db, "alpha")
        .create_node("bob", &["Person"], &Metadata::new())
        .unwrap();
    step(db);
    graph(db, "alpha")
        .create_node(
            "doc",
            &["Doc"],
            &Metadata::from([("title".into(), text("notes"))]),
        )
        .unwrap();
    step(db);
    graph(db, "alpha")
        .create_edge(
            Some("ada-knows-bob"),
            "ada",
            "KNOWS",
            "bob",
            &Metadata::from([("since".into(), MetadataValue::Integer(2020))]),
        )
        .unwrap();
    step(db);
    for (source, relationship, destination) in [
        ("ada", "KNOWS", "bob"),
        ("bob", "LIKES", "bob"),
        ("ada", "WROTE", "doc"),
    ] {
        graph(db, "alpha")
            .create_edge(None, source, relationship, destination, &Metadata::new())
            .unwrap();
        step(db);
    }
    graph(db, "alpha")
        .patch_node(
            "bob",
            &NodePatch {
                add_labels: vec!["Reviewer".into()],
                set: Metadata::from([("level".into(), MetadataValue::Integer(3))]),
                ..NodePatch::default()
            },
            Expected::Any,
        )
        .unwrap();
    step(db);
    for (node, collection, record) in [
        ("ada", "default", 1),
        ("doc", "docs", 10),
        ("bob", "default", 2),
    ] {
        graph(db, "alpha").attach(node, collection, record).unwrap();
        step(db);
    }
    graph(db, "alpha").detach("bob", "default").unwrap();
    step(db);
    graph(db, "alpha")
        .write_batch(&[
            GraphOp::CreateNode {
                key: "carol".into(),
                labels: vec!["Person".into()],
                properties: Metadata::new(),
            },
            GraphOp::Vector {
                collection: "default".into(),
                mutation: Mutation::Put {
                    id: 4,
                    vector: vec![4.0, 4.0],
                    payload: b"four".to_vec(),
                },
            },
            GraphOp::Attach {
                node: "carol".into(),
                collection: "default".into(),
                record: 4,
            },
            GraphOp::CreateEdge {
                key: None,
                source: "carol".into(),
                relationship: "KNOWS".into(),
                destination: "ada".into(),
                properties: Metadata::new(),
            },
        ])
        .unwrap();
    step(db);
    let gone = graph(db, "alpha")
        .create_node("gone", &[], &Metadata::new())
        .unwrap()
        .id;
    step(db);
    let gone_edge = graph(db, "alpha")
        .create_edge(None, "gone", "TEMP", "ada", &Metadata::new())
        .unwrap()
        .id;
    step(db);
    assert_eq!(
        HighWater {
            node: gone + 1,
            edge: gone_edge + 1
        },
        ALPHA_HIGH_WATER
    );
    graph(db, "alpha")
        .delete_edge(EdgeRef::Id(gone_edge), Expected::Any)
        .unwrap();
    step(db);
    graph(db, "alpha")
        .delete_node("gone", Expected::Any)
        .unwrap();
    step(db);

    for node in ["x", "y"] {
        graph(db, "beta")
            .create_node(node, &["Thing"], &Metadata::new())
            .unwrap();
        step(db);
    }
    graph(db, "beta")
        .create_edge(Some("x-y"), "x", "NEXT", "y", &Metadata::new())
        .unwrap();
    step(db);
    graph(db, "beta").attach("x", "docs", 11).unwrap();
    step(db);

    db.put(5, &[5.0, 5.0], b"five").unwrap();
    step(db);
    db.delete(3).unwrap();
    step(db);
}

/// One more multi-entry graph commit after `populate` (TEST-004's last frame).
pub fn extend(db: &mut Database) {
    graph(db, "beta")
        .write_batch(&[
            GraphOp::CreateNode {
                key: "z".into(),
                labels: vec!["Thing".into(), "Last".into()],
                properties: Metadata::from([("note".into(), text("final batch"))]),
            },
            GraphOp::Vector {
                collection: "docs".into(),
                mutation: Mutation::Put {
                    id: 12,
                    vector: vec![0.0, 0.0, 1.0],
                    payload: b"d12".to_vec(),
                },
            },
            GraphOp::Attach {
                node: "z".into(),
                collection: "docs".into(),
                record: 12,
            },
            GraphOp::CreateEdge {
                key: Some("y-z".into()),
                source: "y".into(),
                relationship: "NEXT".into(),
                destination: "z".into(),
                properties: Metadata::from([("weight".into(), MetadataValue::Float(1.5))]),
            },
        ])
        .unwrap();
}

#[track_caller]
pub fn assert_counts(counts: &GraphCounts, expected: [usize; 4]) {
    assert_eq!(
        [
            counts.graphs,
            counts.nodes,
            counts.edges,
            counts.attachments
        ],
        expected
    );
}

/// `(collection, id, record, expiry, revision)` of one record.
pub type RecordRow = (String, u64, Record, Option<u64>, Option<u64>);

/// Everything observable through the public API, in canonical order.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub graph_enabled: bool,
    pub collections: Vec<CollectionInfo>,
    /// Records with IDs 0..=20.
    pub records: Vec<RecordRow>,
    pub graphs: Vec<GraphSnapshot>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GraphSnapshot {
    pub name: String,
    pub nodes: Vec<NodeView>,
    pub edges: Vec<EdgeView>,
    /// `(node key, collection, record)`.
    pub attachments: Vec<(String, String, u64)>,
}

impl Snapshot {
    /// The snapshot with record, node and edge revisions cleared (compaction
    /// restarts commit sequences).
    pub fn without_revisions(mut self) -> Self {
        for record in &mut self.records {
            record.4 = None;
        }
        for graph in &mut self.graphs {
            graph.nodes.iter_mut().for_each(|node| node.revision = 0);
            graph.edges.iter_mut().for_each(|edge| edge.revision = 0);
        }
        self
    }

    /// The vector part only, as a file without `FLAG_GRAPH` shows it.
    pub fn vector_only(mut self) -> Self {
        self.graph_enabled = false;
        self.graphs.clear();
        self
    }

    pub fn counts(&self) -> [usize; 4] {
        [
            self.graphs.len(),
            self.graphs.iter().map(|graph| graph.nodes.len()).sum(),
            self.graphs.iter().map(|graph| graph.edges.len()).sum(),
            self.graphs
                .iter()
                .map(|graph| graph.attachments.len())
                .sum(),
        ]
    }
}

pub fn snapshot(path: &Path, key: Option<&EncryptionKey>) -> Snapshot {
    snapshot_bytes(fs::read(path).unwrap(), key)
}

/// Snapshot of a complete file image. Attachments have no read API, so they
/// are found by detaching every node from every collection in a second,
/// in-memory copy.
pub fn snapshot_bytes(bytes: Vec<u8>, key: Option<&EncryptionKey>) -> Snapshot {
    let mut snapshot = state(&Database::open_from_bytes(bytes.clone(), key).unwrap());
    if !snapshot.graph_enabled {
        return snapshot;
    }
    let mut scratch = Database::open_from_bytes(bytes, key).unwrap();
    let collections: Vec<String> = snapshot
        .collections
        .iter()
        .map(|info| info.name.clone())
        .collect();
    for graph in &mut snapshot.graphs {
        let mut handle = scratch.graph_mut(&graph.name).unwrap();
        for node in &graph.nodes {
            for collection in &collections {
                match handle.detach(&node.key, collection) {
                    Ok(record) => {
                        graph
                            .attachments
                            .push((node.key.clone(), collection.clone(), record))
                    }
                    Err(Error::GraphAttachmentNotFound) => {}
                    Err(other) => panic!("detach {}/{collection}: {other:?}", node.key),
                }
            }
        }
    }
    snapshot
}

/// Collections, records and graph nodes and edges of an open database
/// (attachments are left empty).
pub fn state(db: &Database) -> Snapshot {
    let collections = db.collections().unwrap();
    let mut records = Vec::new();
    for info in &collections {
        let collection = db.collection(&info.name).unwrap();
        for id in 0..=20 {
            if let Some(record) = collection.get(id).unwrap() {
                records.push((
                    info.name.clone(),
                    id,
                    record.clone(),
                    collection.expires_at(id).unwrap(),
                    collection.revision(id).unwrap(),
                ));
            }
        }
    }
    let mut graphs = Vec::new();
    if db.is_graph_enabled() {
        for info in db.list_graphs().unwrap() {
            let graph = db.graph(&info.name).unwrap();
            let nodes = nodes(&graph);
            let edges = edges(&graph, &nodes);
            graphs.push(GraphSnapshot {
                name: info.name,
                nodes,
                edges,
                attachments: Vec::new(),
            });
        }
    }
    Snapshot {
        graph_enabled: db.is_graph_enabled(),
        collections,
        records,
        graphs,
    }
}

fn nodes(graph: &GraphRef<'_>) -> Vec<NodeView> {
    let mut request = FindNodes::default();
    let mut nodes = Vec::new();
    loop {
        let page = graph.find_nodes(&request, &ReadOptions::default()).unwrap();
        nodes.extend(page.output);
        match page.cursor {
            Some(cursor) => request.cursor = Some(cursor),
            None => return nodes,
        }
    }
}

fn edges(graph: &GraphRef<'_>, nodes: &[NodeView]) -> Vec<EdgeView> {
    let mut edges = Vec::new();
    for node in nodes {
        let mut request = Neighbors::new(NodeSelector::Id(node.id));
        loop {
            let page = graph.neighbors(&request, &ReadOptions::default()).unwrap();
            edges.extend(page.output.edges);
            match page.cursor {
                Some(cursor) => request.cursor = Some(cursor),
                None => break,
            }
        }
    }
    edges.sort_by_key(|edge| edge.id);
    edges
}
