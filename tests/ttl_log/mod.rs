//! Gap 3 feed fixtures: an independent byte builder for plain V5 graph logs.
//!
//! Nothing here calls the crate's writers. The layouts are the documented
//! ones: the 32-byte V5 header, the 36-byte frame prefix and 16-byte commit
//! footer, and the DO-003 op-12 body with its 8-byte entry envelopes. Logs
//! use a fixed lineage and logged reclaim clocks, so feed output and cursor
//! tokens are literal constants.
#![allow(dead_code)]

/// The fixed lineage of every fixture log.
pub const LINEAGE: u64 = 0x0123_4567_89ab_cdef;
pub const FLAG_GRAPH: u16 = 4;
pub const FLAG_HISTORY: u16 = 8;

/// IEEE CRC-32, bit by bit.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

/// One op-12 body entry.
#[derive(Clone, Debug)]
pub enum Entry {
    CreateGraph(&'static str),
    DropGraph,
    HighWater(u64, u64),
    /// `(id, key, labels)`, no properties.
    PutNode(u64, &'static str, &'static [&'static str]),
    /// `(id, add_labels)`.
    PatchNode(u64, &'static [&'static str]),
    DeleteNode(u64),
    /// `(id, source, destination, relationship, key)`.
    PutEdge(u64, u64, u64, &'static str, Option<&'static str>),
    DeleteEdge(u64),
    /// `(node, collection, record)`.
    Attach(u64, u16, u64),
    Detach(u64, u16),
    NodeExpiry(u64, Option<u64>),
    EdgeExpiry(u64, Option<u64>),
    Reclaim(u64),
}

fn labels(bytes: &mut Vec<u8>, labels: &[&str]) {
    bytes.push(labels.len() as u8);
    for label in labels {
        bytes.push(label.len() as u8);
        bytes.extend_from_slice(label.as_bytes());
    }
}

/// Empty properties: `len u32 = 4`, `count u32 = 0`.
fn no_properties(bytes: &mut Vec<u8>) {
    bytes.extend_from_slice(&4u32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
}

impl Entry {
    fn tag(&self) -> u8 {
        match self {
            Entry::CreateGraph(_) => 1,
            Entry::DropGraph => 2,
            Entry::HighWater(..) => 3,
            Entry::PutNode(..) => 4,
            Entry::PatchNode(..) => 5,
            Entry::DeleteNode(_) => 6,
            Entry::PutEdge(..) => 7,
            Entry::DeleteEdge(_) => 9,
            Entry::Attach(..) => 10,
            Entry::Detach(..) => 11,
            Entry::NodeExpiry(..) => 13,
            Entry::EdgeExpiry(..) => 14,
            Entry::Reclaim(_) => 15,
        }
    }

    fn payload(&self) -> Vec<u8> {
        let mut b = Vec::new();
        match self {
            Entry::CreateGraph(name) => {
                b.push(name.len() as u8);
                b.extend_from_slice(name.as_bytes());
            }
            Entry::DropGraph => {}
            Entry::HighWater(node, edge) => {
                b.extend_from_slice(&node.to_le_bytes());
                b.extend_from_slice(&edge.to_le_bytes());
            }
            Entry::PutNode(id, key, set) => {
                b.extend_from_slice(&id.to_le_bytes());
                b.extend_from_slice(&(key.len() as u16).to_le_bytes());
                b.extend_from_slice(key.as_bytes());
                labels(&mut b, set);
                no_properties(&mut b);
            }
            Entry::PatchNode(id, add) => {
                b.extend_from_slice(&id.to_le_bytes());
                labels(&mut b, add);
                labels(&mut b, &[]);
                no_properties(&mut b);
                b.push(0);
            }
            Entry::DeleteNode(id) | Entry::DeleteEdge(id) => b.extend_from_slice(&id.to_le_bytes()),
            Entry::PutEdge(id, source, destination, relationship, key) => {
                b.extend_from_slice(&id.to_le_bytes());
                b.extend_from_slice(&source.to_le_bytes());
                b.extend_from_slice(&destination.to_le_bytes());
                b.push(relationship.len() as u8);
                b.extend_from_slice(relationship.as_bytes());
                let key = key.unwrap_or("");
                b.extend_from_slice(&(key.len() as u16).to_le_bytes());
                b.extend_from_slice(key.as_bytes());
                no_properties(&mut b);
            }
            Entry::Attach(node, collection, record) => {
                b.extend_from_slice(&node.to_le_bytes());
                b.extend_from_slice(&collection.to_le_bytes());
                b.extend_from_slice(&[0, 0]);
                b.extend_from_slice(&record.to_le_bytes());
            }
            Entry::Detach(node, collection) => {
                b.extend_from_slice(&node.to_le_bytes());
                b.extend_from_slice(&collection.to_le_bytes());
                b.extend_from_slice(&[0, 0]);
            }
            Entry::NodeExpiry(id, deadline) | Entry::EdgeExpiry(id, deadline) => {
                b.extend_from_slice(&id.to_le_bytes());
                if let Some(deadline) = deadline {
                    b.extend_from_slice(&deadline.to_le_bytes());
                }
            }
            Entry::Reclaim(at_ms) => b.extend_from_slice(&at_ms.to_le_bytes()),
        }
        b
    }
}

/// A DO-003 body: version 1, reserved byte, graph ID, then each entry's
/// tag, three reserved bytes, payload length and payload.
fn graph_body(graph: u16, entries: &[Entry]) -> Vec<u8> {
    let mut body = vec![1, 0];
    body.extend_from_slice(&graph.to_le_bytes());
    for entry in entries {
        let payload = entry.payload();
        body.extend_from_slice(&[entry.tag(), 0, 0, 0]);
        body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        body.extend_from_slice(&payload);
    }
    body
}

/// A plain V5 graph-enabled log under construction; frames get sequences
/// 1, 2, ... in order.
pub struct Log {
    pub bytes: Vec<u8>,
    sequence: u64,
}

impl Log {
    /// Header: magic, version 5, dimensions, lineage, collection floor 0,
    /// flags, CRC of the first 28 bytes.
    pub fn new(dimensions: u32) -> Self {
        Self::with_flags(dimensions, FLAG_GRAPH)
    }

    /// [`Log::new`] with other header flags.
    pub fn with_flags(dimensions: u32, flags: u16) -> Self {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"VLITEDB\0");
        bytes.extend_from_slice(&5u32.to_le_bytes());
        bytes.extend_from_slice(&dimensions.to_le_bytes());
        bytes.extend_from_slice(&LINEAGE.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&flags.to_le_bytes());
        let crc = crc32(&bytes);
        bytes.extend_from_slice(&crc.to_le_bytes());
        Self { bytes, sequence: 0 }
    }

    fn frame(&mut self, operation: u8, id: u64, count: u32, payload_len: u32, body: &[u8]) {
        self.sequence += 1;
        self.frame_at(operation, id, count, payload_len, body);
    }

    /// A frame at the current sequence (restated checkpoint frames).
    fn frame_at(&mut self, operation: u8, id: u64, count: u32, payload_len: u32, body: &[u8]) {
        let mut frame = Vec::new();
        frame.extend_from_slice(b"VREC");
        frame.push(operation);
        frame.extend_from_slice(&0u16.to_le_bytes());
        frame.push(0);
        frame.extend_from_slice(&self.sequence.to_le_bytes());
        frame.extend_from_slice(&id.to_le_bytes());
        frame.extend_from_slice(&count.to_le_bytes());
        frame.extend_from_slice(&payload_len.to_le_bytes());
        let crc = crc32(&frame);
        frame.extend_from_slice(&crc.to_le_bytes());
        frame.extend_from_slice(body);
        let crc = crc32(&frame);
        frame.extend_from_slice(b"VCMT");
        frame.extend_from_slice(&crc.to_le_bytes());
        frame.extend_from_slice(&self.sequence.to_le_bytes());
        self.bytes.extend_from_slice(&frame);
    }

    /// Op-1 put of a default-collection record without metadata.
    pub fn put(&mut self, id: u64, vector: &[f32], payload: &[u8]) -> &mut Self {
        let mut body = Vec::new();
        for value in vector {
            body.extend_from_slice(&value.to_le_bytes());
        }
        body.extend_from_slice(payload);
        self.frame(1, id, vector.len() as u32, payload.len() as u32, &body);
        self
    }

    /// Op-2 delete of a default-collection record.
    pub fn delete(&mut self, id: u64) -> &mut Self {
        self.frame(2, id, 0, 0, &[]);
        self
    }

    /// One op-12 frame for `graph`; the prefix ID counts the reclaims.
    pub fn graph(&mut self, graph: u16, entries: &[Entry]) -> &mut Self {
        let body = graph_body(graph, entries);
        let reclaims = entries
            .iter()
            .filter(|entry| matches!(entry, Entry::Reclaim(_)))
            .count() as u64;
        let len = body.len() as u32;
        self.frame(12, reclaims, entries.len() as u32, len, &body);
        self
    }

    /// The op-13 advance frame of a `FLAG_HISTORY` image restating history
    /// through `floor`; it must be the first frame. Later restated frames
    /// share its sequence.
    pub fn advance(&mut self, floor: u64) -> &mut Self {
        assert_eq!(self.sequence, 0, "the advance frame comes first");
        self.sequence = floor;
        self.frame_at(13, 0, 0, 0, &[]);
        self
    }

    /// An op-14 record block of default-collection records `(id, revision,
    /// vector, payload)`, without expiry or metadata, at the floor.
    pub fn records(&mut self, records: &[(u64, u64, &[f32], &[u8])]) -> &mut Self {
        let mut body = Vec::new();
        for (id, revision, vector, payload) in records {
            let mut extended = Vec::new();
            for value in *vector {
                extended.extend_from_slice(&value.to_le_bytes());
            }
            extended.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            extended.extend_from_slice(payload);
            extended.extend_from_slice(&0u32.to_le_bytes());
            body.extend_from_slice(&id.to_le_bytes());
            body.extend_from_slice(&revision.to_le_bytes());
            body.push(0);
            body.extend_from_slice(&0u64.to_le_bytes());
            body.extend_from_slice(&(extended.len() as u32).to_le_bytes());
            body.extend_from_slice(&extended);
        }
        let len = body.len() as u32;
        self.frame_at(14, 0, records.len() as u32, len, &body);
        self
    }

    /// One op-15 snapshot frame for `graph` at the floor: laid out as an
    /// op-12 frame, with a zero prefix ID.
    pub fn snapshot(&mut self, graph: u16, entries: &[Entry]) -> &mut Self {
        let body = graph_body(graph, entries);
        let len = body.len() as u32;
        self.frame_at(15, 0, entries.len() as u32, len, &body);
        self
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }
}

/// The deadline/reclaim fixture: 14 commits of graph `g` (graph ID 0) and
/// the default collection, with expiry-only commits, two logged reclaims,
/// key reuse after a reclaim, a reattached released record, a preserved
/// deadline on upsert, a reclaim before a drop and a recreated graph.
pub fn reclaim_fixture() -> Log {
    use Entry::*;
    let mut log = Log::new(2);
    log.put(1, &[1.0, 0.0], b"r1") // 1
        .put(2, &[0.0, 1.0], b"r2") // 2
        .graph(0, &[CreateGraph("g")]) // 3
        .graph(
            0,
            &[
                PutNode(0, "a", &[]),
                PutNode(1, "b", &[]),
                PutNode(2, "c", &[]),
                PutEdge(0, 0, 1, "KNOWS", Some("ab")),
                PutEdge(1, 1, 1, "SELF", None),
                PutEdge(2, 0, 2, "KNOWS", None),
                Attach(0, 0, 1),
            ],
        ) // 4
        .graph(0, &[NodeExpiry(0, Some(100))]) // 5
        .graph(0, &[EdgeExpiry(2, Some(50)), PatchNode(1, &["X"])]) // 6
        .graph(0, &[Reclaim(100), PutNode(3, "a", &[]), Attach(3, 0, 1)]) // 7
        .graph(0, &[NodeExpiry(1, Some(150)), Reclaim(150)]) // 8
        .graph(
            0,
            &[
                PutEdge(3, 2, 3, "KNOWS", Some("ca")),
                EdgeExpiry(3, Some(300)),
                EdgeExpiry(3, None),
            ],
        ) // 9
        .graph(0, &[NodeExpiry(2, Some(400)), PutNode(2, "c", &["Kept"])]) // 10
        .graph(0, &[DeleteEdge(3), Detach(3, 0), DeleteNode(3)]) // 11
        .delete(1) // 12
        .graph(0, &[Reclaim(400), DropGraph]) // 13
        .graph(0, &[CreateGraph("g"), PutNode(0, "a", &[])]); // 14
    log
}

/// The TTL stream of `reclaim_fixture`, derived by hand from its 14 commits.
pub const RECLAIM_TTL: [&str; 18] = [
    "1/0 put 1",
    "2/0 put 2",
    "3/0 create g",
    "4/0 g: node+0:a ttl=null, node+1:b ttl=null, node+2:c ttl=null, \
     edge+0:ab(0>1) ttl=null, edge+1:_(1>1) ttl=null, edge+2:_(0>2) ttl=null, \
     attach 0:default/1",
    "5/0 g: node~0:a ttl=100!",
    "6/0 g: edge~2:_ ttl=50!, node~1:b ttl=null",
    "7/0 g: edge-0:ab node_expired=100@100, edge-2:_ expired=50@100, \
     detach 0:default/1, node-0:a expired=100@100",
    "7/1 g: node+3:a ttl=null, attach 3:default/1",
    "8/0 g: node~1:b ttl=150!",
    "8/1 g: edge-1:_ node_expired=150@150, node-1:b expired=150@150",
    "9/0 g: edge+3:ca(2>3) ttl=null, edge~3:ca ttl=300!, edge~3:ca ttl=null!",
    "10/0 g: node~2:c ttl=400!, node=2:c ttl=400",
    "11/0 g: edge-3:ca explicit, detach 3:default/1, node-3:a explicit",
    "12/0 delete 1",
    "13/0 g: node-2:c expired=400@400",
    "13/1 drop g",
    "14/0 create g",
    "14/1 g: node+0:a ttl=null",
];

/// One feed event as text: `sequence/ordinal`, then the event; a graph
/// batch lists `operation ttl` pairs. The notation is this file's own, so
/// expected streams are written out by hand.
pub fn describe(event: &vlite::changes::GraphFeedChange) -> String {
    use vlite::changes::{ChangeKind, GraphChange, GraphRemovalReason, GraphTtl};
    let change = &event.change;
    let head = format!("{}/{}", change.sequence, change.ordinal);
    let at = change
        .committed_at_ms
        .map(|ms| format!(" t={ms}"))
        .unwrap_or_default();
    let body = match &change.kind {
        ChangeKind::Put { id, .. } => format!("put {id}"),
        ChangeKind::Delete { id } => format!("delete {id}"),
        ChangeKind::GraphCreated { graph } => format!("create {graph}"),
        ChangeKind::GraphDropped { graph } => format!("drop {graph}"),
        ChangeKind::GraphBatch {
            graph,
            revision,
            operations,
        } => {
            assert_eq!(*revision, change.sequence, "graph batch revision");
            assert_eq!(event.ttl.len(), operations.len(), "one annotation each");
            let key = |key: &Option<String>| key.clone().unwrap_or_else(|| "_".into());
            let parts: Vec<String> = operations
                .iter()
                .zip(&event.ttl)
                .map(|(operation, ttl)| {
                    let operation = match operation {
                        GraphChange::NodeUpserted {
                            id, key, created, ..
                        } => format!("node{}{id}:{key}", if *created { "+" } else { "=" }),
                        GraphChange::NodePatched { id, key, .. } => format!("node~{id}:{key}"),
                        GraphChange::NodeDeleted { id, key } => format!("node-{id}:{key}"),
                        GraphChange::EdgeUpserted {
                            id,
                            key: edge,
                            source,
                            destination,
                            created,
                            ..
                        } => format!(
                            "edge{}{id}:{}({source}>{destination})",
                            if *created { "+" } else { "=" },
                            key(edge)
                        ),
                        GraphChange::EdgePatched { id, key: edge, .. } => {
                            format!("edge~{id}:{}", key(edge))
                        }
                        GraphChange::EdgeDeleted { id, key: edge } => {
                            format!("edge-{id}:{}", key(edge))
                        }
                        GraphChange::Attached {
                            node,
                            collection,
                            record,
                            ..
                        } => format!("attach {node}:{collection}/{record}"),
                        GraphChange::Detached {
                            node,
                            collection,
                            record,
                            ..
                        } => format!("detach {node}:{collection}/{record}"),
                        other => format!("{other:?}"),
                    };
                    let ttl = match ttl {
                        None => String::new(),
                        Some(GraphTtl::Updated {
                            expires_at_ms,
                            expiry_changed,
                        }) => format!(
                            " ttl={}{}",
                            expires_at_ms.map_or("null".into(), |ms| ms.to_string()),
                            if *expiry_changed { "!" } else { "" }
                        ),
                        Some(GraphTtl::ExplicitRemoval) => " explicit".into(),
                        Some(GraphTtl::ExpiryRemoval {
                            removal_reason,
                            expires_at_ms,
                            removed_at_ms,
                        }) => format!(
                            " {}={expires_at_ms}@{removed_at_ms}",
                            match removal_reason {
                                GraphRemovalReason::Expired => "expired",
                                GraphRemovalReason::NodeExpired => "node_expired",
                                other => panic!("{other:?}"),
                            }
                        ),
                        Some(other) => format!(" {other:?}"),
                    };
                    format!("{operation}{ttl}")
                })
                .collect();
            format!("{graph}: {}", parts.join(", "))
        }
        other => format!("{other:?}"),
    };
    format!("{head}{at} {body}")
}
