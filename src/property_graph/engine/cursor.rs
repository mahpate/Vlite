//! REQ-027: opaque, versioned page cursors for `find_nodes` and `neighbors`.
//!
//! Layout (integers little-endian): the shared header contains `version u8`
//! (1 ordinary, 2 temporal), `kind u8` (1 `find_nodes`, 2 `neighbors`),
//! `graph_id u16`, `graph_sequence u64` at issue time, `fingerprint u64` of
//! the normalized query, and `position u8` (0 start, 1 node, 2 edge). V2
//! stores resolved `as_of_ms i64` and `ttl_clock_ms u64` after that header.
//! Position payload follows: node ID, or edge ID, type length and type. Both
//! versions end with an FNV-1a 64 checksum of all preceding bytes.
//!
//! Decoding never panics and allocates at most one relationship type. It
//! checks, in order: version and structure, checksum, operation, graph and
//! query fingerprint (all `InvalidCursor`), then the graph's current
//! `graph_sequence` (`CursorStale`). The checksum detects corruption; it is
//! not a MAC. A forged cursor can only choose a resume position within the
//! same read.

use super::error::GraphError;
use super::model::{
    Direction, EdgeId, GraphId, NodeId, PropertyCondition, PropertyValue,
    validate_relationship_type,
};

const VERSION: u8 = 1;
const TEMPORAL_VERSION: u8 = 2;
const HEADER_BYTES: usize = 21;
const CHECKSUM_BYTES: usize = 8;

/// The read a cursor belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CursorKind {
    FindNodes = 1,
    Neighbors = 2,
}

/// Where the next page starts: after this position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Position {
    Start,
    Node(NodeId),
    Edge(String, EdgeId),
}

/// FNV-1a 64 over length-prefixed fields; stable across builds and
/// platforms, unlike `DefaultHasher`.
pub(super) struct Fingerprint(u64);

impl Fingerprint {
    pub(super) fn new(kind: CursorKind) -> Self {
        let mut hash = Self(0xcbf2_9ce4_8422_2325);
        hash.bytes(&[kind as u8]);
        hash
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    pub(super) fn u64(&mut self, value: u64) {
        self.bytes(&value.to_le_bytes());
    }

    pub(super) fn str(&mut self, value: &str) {
        self.u64(value.len() as u64);
        self.bytes(value.as_bytes());
    }

    pub(super) fn direction(&mut self, direction: Direction) {
        self.u64(match direction {
            Direction::Out => 0,
            Direction::In => 1,
            Direction::Both => 2,
        });
    }

    fn value(&mut self, value: &PropertyValue) {
        match value {
            PropertyValue::String(value) => {
                self.u64(1);
                self.str(value);
            }
            PropertyValue::Integer(value) => {
                self.u64(2);
                self.bytes(&value.to_le_bytes());
            }
            PropertyValue::Float(value) => {
                self.u64(3);
                self.u64(value.to_bits());
            }
            PropertyValue::Boolean(value) => {
                self.u64(4);
                self.u64(u64::from(*value));
            }
            PropertyValue::GeoPoint {
                latitude,
                longitude,
            } => {
                self.u64(5);
                self.u64(latitude.to_bits());
                self.u64(longitude.to_bits());
            }
        }
    }

    fn optional_value(&mut self, value: Option<&PropertyValue>) {
        match value {
            None => self.u64(0),
            Some(value) => {
                self.u64(1);
                self.value(value);
            }
        }
    }

    pub(super) fn conditions(&mut self, conditions: &[PropertyCondition]) {
        self.u64(conditions.len() as u64);
        for condition in conditions {
            match condition {
                PropertyCondition::Eq { field, value } => {
                    self.u64(1);
                    self.str(field);
                    self.value(value);
                }
                PropertyCondition::Range { field, min, max } => {
                    self.u64(2);
                    self.str(field);
                    self.optional_value(min.as_ref());
                    self.optional_value(max.as_ref());
                }
                PropertyCondition::GeoRadius {
                    field,
                    latitude,
                    longitude,
                    radius_meters,
                } => {
                    self.u64(3);
                    self.str(field);
                    self.u64(latitude.to_bits());
                    self.u64(longitude.to_bits());
                    self.u64(radius_meters.to_bits());
                }
            }
        }
    }

    pub(super) fn finish(&self) -> u64 {
        self.0
    }
}

fn checksum(bytes: &[u8]) -> u64 {
    let mut hash = Fingerprint(0xcbf2_9ce4_8422_2325);
    hash.bytes(bytes);
    hash.finish()
}

/// Identity of the read a cursor is issued for or checked against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CursorScope {
    pub(super) kind: CursorKind,
    pub(super) graph: GraphId,
    pub(super) graph_sequence: u64,
    pub(super) fingerprint: u64,
    pub(super) temporal: Option<(i64, u64)>,
}

pub(super) fn encode(scope: &CursorScope, position: &Position) -> Vec<u8> {
    let temporal_bytes = if scope.temporal.is_some() { 16 } else { 0 };
    let mut bytes = Vec::with_capacity(HEADER_BYTES + temporal_bytes + 9 + 64 + CHECKSUM_BYTES);
    bytes.push(if scope.temporal.is_some() {
        TEMPORAL_VERSION
    } else {
        VERSION
    });
    bytes.push(scope.kind as u8);
    bytes.extend_from_slice(&scope.graph.0.to_le_bytes());
    bytes.extend_from_slice(&scope.graph_sequence.to_le_bytes());
    bytes.extend_from_slice(&scope.fingerprint.to_le_bytes());
    let position_bytes = match position {
        Position::Start => {
            bytes.push(0);
            Vec::new()
        }
        Position::Node(node) => {
            bytes.push(1);
            node.0.to_le_bytes().to_vec()
        }
        Position::Edge(kind, edge) => {
            bytes.push(2);
            // Relationship types are at most 63 bytes.
            let mut position = edge.0.to_le_bytes().to_vec();
            position.push(kind.len() as u8);
            position.extend_from_slice(kind.as_bytes());
            position
        }
    };
    if let Some((as_of_ms, ttl_clock_ms)) = scope.temporal {
        bytes.extend_from_slice(&as_of_ms.to_le_bytes());
        bytes.extend_from_slice(&ttl_clock_ms.to_le_bytes());
    }
    bytes.extend_from_slice(&position_bytes);
    let sum = checksum(&bytes);
    bytes.extend_from_slice(&sum.to_le_bytes());
    bytes
}

fn invalid(reason: &'static str) -> GraphError {
    GraphError::InvalidCursor(reason)
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    let slice = bytes.get(at..at.checked_add(8)?)?;
    let mut value = [0; 8];
    value.copy_from_slice(slice);
    Some(u64::from_le_bytes(value))
}

fn i64_at(bytes: &[u8], at: usize) -> Option<i64> {
    let slice = bytes.get(at..at.checked_add(8)?)?;
    let mut value = [0; 8];
    value.copy_from_slice(slice);
    Some(i64::from_le_bytes(value))
}

struct Components {
    kind: u8,
    graph: GraphId,
    graph_sequence: u64,
    fingerprint: u64,
    temporal: Option<(i64, u64)>,
    position: Position,
}

fn components(bytes: &[u8]) -> Result<Components, GraphError> {
    const MALFORMED: &str = "malformed cursor";
    let version = match bytes.first() {
        None => return Err(invalid("empty cursor")),
        Some(&VERSION) => VERSION,
        Some(&TEMPORAL_VERSION) => TEMPORAL_VERSION,
        Some(_) => return Err(invalid("unsupported cursor version")),
    };
    let (Some(&kind), Some(graph), Some(sequence), Some(fingerprint), Some(&tag)) = (
        bytes.get(1),
        bytes.get(2..4),
        u64_at(bytes, 4),
        u64_at(bytes, 12),
        bytes.get(20),
    ) else {
        return Err(invalid(MALFORMED));
    };
    let temporal = if version == TEMPORAL_VERSION {
        Some((
            i64_at(bytes, HEADER_BYTES).ok_or(invalid(MALFORMED))?,
            u64_at(bytes, HEADER_BYTES + 8).ok_or(invalid(MALFORMED))?,
        ))
    } else {
        None
    };
    let position_start = HEADER_BYTES + if temporal.is_some() { 16 } else { 0 };
    let (position, end) = match tag {
        0 => (Position::Start, position_start),
        1 => (
            Position::Node(NodeId(
                u64_at(bytes, position_start).ok_or(invalid(MALFORMED))?,
            )),
            position_start + 8,
        ),
        2 => {
            let edge = u64_at(bytes, position_start).ok_or(invalid(MALFORMED))?;
            let length = usize::from(*bytes.get(position_start + 8).ok_or(invalid(MALFORMED))?);
            let start = position_start + 9;
            let name = bytes
                .get(start..start + length)
                .and_then(|name| std::str::from_utf8(name).ok())
                .filter(|name| validate_relationship_type(name).is_ok())
                .ok_or(invalid(MALFORMED))?;
            (
                Position::Edge(name.to_owned(), EdgeId(edge)),
                start + length,
            )
        }
        _ => return Err(invalid(MALFORMED)),
    };
    if bytes.len() != end + CHECKSUM_BYTES {
        return Err(invalid(MALFORMED));
    }
    if u64_at(bytes, end) != Some(checksum(&bytes[..end])) {
        return Err(invalid("corrupt cursor"));
    }
    let graph = u16::from_le_bytes([graph[0], graph[1]]);
    Ok(Components {
        kind,
        graph: GraphId(graph),
        graph_sequence: sequence,
        fingerprint,
        temporal,
        position,
    })
}

/// Decodes `bytes` issued for `expected` and returns the resume position.
pub(super) fn decode(bytes: &[u8], expected: &CursorScope) -> Result<Position, GraphError> {
    let decoded = components(bytes)?;
    let position_kind = match &decoded.position {
        Position::Start => None,
        Position::Node(_) => Some(CursorKind::FindNodes),
        Position::Edge(..) => Some(CursorKind::Neighbors),
    };
    if decoded.kind != expected.kind as u8
        || position_kind.is_some_and(|kind| kind != expected.kind)
    {
        return Err(invalid("cursor belongs to another operation"));
    }
    if decoded.graph != expected.graph {
        return Err(invalid("cursor belongs to another graph"));
    }
    if decoded.temporal != expected.temporal {
        return Err(invalid("cursor belongs to another temporal view"));
    }
    if decoded.fingerprint != expected.fingerprint {
        return Err(invalid("cursor belongs to another query"));
    }
    if decoded.graph_sequence != expected.graph_sequence {
        return Err(GraphError::CursorStale);
    }
    Ok(decoded.position)
}

/// Validates and returns the fixed selectors embedded in a temporal cursor.
#[allow(dead_code)] // The standalone graph harness has no temporal adapter.
pub(super) fn temporal_selectors(bytes: &[u8]) -> Result<(i64, u64), GraphError> {
    components(bytes)?
        .temporal
        .ok_or(invalid("cursor is not temporal"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(kind: CursorKind) -> CursorScope {
        CursorScope {
            kind,
            graph: GraphId(3),
            graph_sequence: 41,
            fingerprint: 0x0123_4567_89ab_cdef,
            temporal: None,
        }
    }

    fn positions() -> Vec<(CursorKind, Position)> {
        vec![
            (CursorKind::FindNodes, Position::Start),
            (CursorKind::FindNodes, Position::Node(NodeId(0))),
            (CursorKind::FindNodes, Position::Node(NodeId(u64::MAX))),
            (CursorKind::Neighbors, Position::Start),
            (CursorKind::Neighbors, Position::Edge("R".into(), EdgeId(7))),
            (
                CursorKind::Neighbors,
                Position::Edge("T".repeat(63), EdgeId(u64::MAX)),
            ),
        ]
    }

    #[test]
    fn cursors_round_trip_with_the_documented_layout() {
        let bytes = encode(
            &scope(CursorKind::Neighbors),
            &Position::Edge("R".into(), EdgeId(7)),
        );
        let mut expected = vec![1, 2, 3, 0, 41, 0, 0, 0, 0, 0, 0, 0];
        expected.extend_from_slice(&0x0123_4567_89ab_cdefu64.to_le_bytes());
        expected.extend_from_slice(&[2, 7, 0, 0, 0, 0, 0, 0, 0, 1, b'R']);
        expected.extend_from_slice(&checksum(&expected).to_le_bytes());
        assert_eq!(bytes, expected);
        for (kind, position) in positions() {
            let bytes = encode(&scope(kind), &position);
            assert!(bytes.len() <= 101);
            assert_eq!(decode(&bytes, &scope(kind)), Ok(position));
        }
        // FNV-1a 64 reference values.
        assert_eq!(checksum(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(checksum(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(checksum(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn mismatches_are_invalid_and_checked_before_staleness() {
        for (kind, position) in positions() {
            let bytes = encode(&scope(kind), &position);
            let other_kind = match kind {
                CursorKind::FindNodes => CursorKind::Neighbors,
                CursorKind::Neighbors => CursorKind::FindNodes,
            };
            let mut stale = scope(kind);
            stale.graph_sequence += 1;
            assert_eq!(decode(&bytes, &stale), Err(GraphError::CursorStale));
            for (mut expected, reason) in [
                (scope(other_kind), "cursor belongs to another operation"),
                (scope(kind), "cursor belongs to another graph"),
                (scope(kind), "cursor belongs to another query"),
            ] {
                match reason {
                    "cursor belongs to another graph" => expected.graph = GraphId(4),
                    "cursor belongs to another query" => expected.fingerprint ^= 1,
                    _ => {}
                }
                expected.graph_sequence += 1;
                assert_eq!(decode(&bytes, &expected), Err(invalid(reason)));
            }
        }
    }

    #[test]
    fn corrupt_truncated_and_garbage_cursors_never_panic() {
        let expected = scope(CursorKind::Neighbors);
        let bytes = encode(&expected, &Position::Edge("REL".into(), EdgeId(9)));
        for length in 0..bytes.len() {
            assert!(matches!(
                decode(&bytes[..length], &expected),
                Err(GraphError::InvalidCursor(_))
            ));
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(decode(&longer, &expected), Err(invalid("malformed cursor")));
        // Every single-bit flip is rejected as invalid, never as stale.
        for index in 0..bytes.len() {
            for bit in 0..8 {
                let mut flipped = bytes.clone();
                flipped[index] ^= 1 << bit;
                assert!(
                    matches!(
                        decode(&flipped, &expected),
                        Err(GraphError::InvalidCursor(_))
                    ),
                    "byte {index} bit {bit}"
                );
            }
        }
        // A type that is not a valid relationship type, valid checksum.
        let mut bad_type = bytes[..HEADER_BYTES + 9].to_vec();
        bad_type[HEADER_BYTES + 8] = 3;
        bad_type.extend_from_slice(b"a-b");
        let sum = checksum(&bad_type);
        bad_type.extend_from_slice(&sum.to_le_bytes());
        assert_eq!(
            decode(&bad_type, &expected),
            Err(invalid("malformed cursor"))
        );
        // Seeded garbage, including a valid version byte.
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        for round in 0..2_000 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let length = (rng % 120) as usize;
            let mut garbage: Vec<u8> = (0..length)
                .map(|index| (rng.rotate_left(index as u32 % 64) >> 3) as u8)
                .collect();
            if round % 2 == 0
                && let Some(first) = garbage.first_mut()
            {
                *first = VERSION;
            }
            assert!(matches!(
                decode(&garbage, &expected),
                Err(GraphError::InvalidCursor(_))
            ));
        }
        assert_eq!(
            decode(&[3], &expected),
            Err(invalid("unsupported cursor version"))
        );
        assert_eq!(decode(&[], &expected), Err(invalid("empty cursor")));
    }

    #[test]
    fn fingerprints_separate_fields() {
        let digest = |build: &dyn Fn(&mut Fingerprint)| {
            let mut hash = Fingerprint::new(CursorKind::FindNodes);
            build(&mut hash);
            hash.finish()
        };
        let a = digest(&|hash| {
            hash.str("ab");
            hash.str("c");
        });
        let b = digest(&|hash| {
            hash.str("a");
            hash.str("bc");
        });
        assert_ne!(a, b);
        let int = digest(&|hash| {
            hash.conditions(&[PropertyCondition::Eq {
                field: "f".into(),
                value: PropertyValue::Integer(1),
            }])
        });
        let float = digest(&|hash| {
            hash.conditions(&[PropertyCondition::Eq {
                field: "f".into(),
                value: PropertyValue::Float(1.0),
            }])
        });
        assert_ne!(int, float);
        assert_ne!(
            Fingerprint::new(CursorKind::FindNodes).finish(),
            Fingerprint::new(CursorKind::Neighbors).finish()
        );
    }

    #[test]
    fn temporal_cursors_bind_and_recover_both_selectors() {
        let mut temporal_scope = scope(CursorKind::Neighbors);
        temporal_scope.temporal = Some((-17, u64::MAX));
        let cursor = encode(&temporal_scope, &Position::Edge("R".into(), EdgeId(9)));
        assert_eq!(temporal_selectors(&cursor), Ok((-17, u64::MAX)));
        assert_eq!(
            decode(&cursor, &temporal_scope),
            Ok(Position::Edge("R".into(), EdgeId(9)))
        );
        for length in 0..cursor.len() {
            assert!(matches!(
                decode(&cursor[..length], &temporal_scope),
                Err(GraphError::InvalidCursor(_))
            ));
        }
        for index in 0..cursor.len() {
            let mut corrupted = cursor.clone();
            corrupted[index] ^= 1;
            assert!(matches!(
                decode(&corrupted, &temporal_scope),
                Err(GraphError::InvalidCursor(_))
            ));
        }

        let mut different_as_of = temporal_scope;
        different_as_of.temporal = Some((-16, u64::MAX));
        assert_eq!(
            decode(&cursor, &different_as_of),
            Err(invalid("cursor belongs to another temporal view"))
        );
        let ordinary = scope(CursorKind::Neighbors);
        assert!(matches!(
            decode(&cursor, &ordinary),
            Err(GraphError::InvalidCursor(_))
        ));
    }
}
