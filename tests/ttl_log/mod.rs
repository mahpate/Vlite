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
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"VLITEDB\0");
        bytes.extend_from_slice(&5u32.to_le_bytes());
        bytes.extend_from_slice(&dimensions.to_le_bytes());
        bytes.extend_from_slice(&LINEAGE.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&FLAG_GRAPH.to_le_bytes());
        let crc = crc32(&bytes);
        bytes.extend_from_slice(&crc.to_le_bytes());
        Self { bytes, sequence: 0 }
    }

    fn frame(&mut self, operation: u8, id: u64, count: u32, payload_len: u32, body: &[u8]) {
        self.sequence += 1;
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
        let mut body = vec![1, 0];
        body.extend_from_slice(&graph.to_le_bytes());
        for entry in entries {
            let payload = entry.payload();
            body.extend_from_slice(&[entry.tag(), 0, 0, 0]);
            body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            body.extend_from_slice(&payload);
        }
        let reclaims = entries
            .iter()
            .filter(|entry| matches!(entry, Entry::Reclaim(_)))
            .count() as u64;
        let len = body.len() as u32;
        self.frame(12, reclaims, entries.len() as u32, len, &body);
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
