//! Plain domain types and their validation rules.

use super::error::GraphError;
use super::limits::{
    MAX_GRAPH_NAME_BYTES, MAX_KEY_BYTES, MAX_LABEL_BYTES, MAX_LABELS, MAX_RELATIONSHIP_TYPE_BYTES,
};
use std::collections::BTreeSet;

/// Internal named-graph ID (`0..MAX_GRAPHS`), unique among live graphs; the ID of
/// a dropped (empty) graph may be assigned to a later graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphId(pub u16);

/// Internal node ID, unique within one graph, allocated monotonically and never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u64);

/// Internal edge ID, unique within one graph, allocated monotonically and never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeId(pub u64);

/// A property value. Variants mirror `vlite::MetadataValue` one-to-one.
#[derive(Clone, Debug, PartialEq)]
pub enum PropertyValue {
    String(String),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    GeoPoint { latitude: f64, longitude: f64 },
}

/// Properties of a node or edge. Mirrors `vlite::Metadata`, stored compactly.
pub use super::property_map::Properties;

/// A property filter condition. Variants mirror `vlite::Condition`
/// one-to-one; the adapter validates with the `vlite` filter rules before
/// converting.
#[derive(Clone, Debug, PartialEq)]
pub enum PropertyCondition {
    Eq {
        field: String,
        value: PropertyValue,
    },
    Range {
        field: String,
        min: Option<PropertyValue>,
        max: Option<PropertyValue>,
    },
    GeoRadius {
        field: String,
        latitude: f64,
        longitude: f64,
        radius_meters: f64,
    },
}

/// A node or edge write precondition. Variants mirror `vlite::Expected`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Precondition {
    /// No precondition.
    Any,
    /// The key must not currently exist.
    Absent,
    /// The node or edge must exist with exactly this revision.
    Revision(u64),
}

/// Edge direction relative to a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
    Both,
}

/// A node as stored in [`super::GraphState`].
#[derive(Clone, Debug, PartialEq)]
pub struct GraphNode {
    pub id: NodeId,
    pub key: String,
    pub labels: BTreeSet<String>,
    pub properties: Properties,
    /// Commit sequence of the last `PutNode` or `PatchNode` for this node.
    pub revision: u64,
}

/// An edge as stored in [`super::GraphState`].
#[derive(Clone, Debug, PartialEq)]
pub struct GraphEdge {
    pub id: EdgeId,
    pub key: Option<String>,
    pub source: NodeId,
    pub relationship: String,
    pub destination: NodeId,
    pub properties: Properties,
    /// Commit sequence of the last `PutEdge` or `PatchEdge` for this edge.
    pub revision: u64,
}

/// REQ-005: collection name rules, `^[a-z][a-z0-9_-]{0,62}$`.
pub(crate) fn validate_graph_name(name: &str) -> Result<(), GraphError> {
    let bytes = name.as_bytes();
    let valid = (1..=MAX_GRAPH_NAME_BYTES).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes[1..]
            .iter()
            .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(GraphError::InvalidGraphName)
    }
}

fn key_valid(key: &str) -> bool {
    (1..=MAX_KEY_BYTES).contains(&key.len())
}

/// PD-001: 1..=256 UTF-8 bytes (UTF-8 validity is guaranteed by `str`).
pub(crate) fn validate_node_key(key: &str) -> Result<(), GraphError> {
    if key_valid(key) {
        Ok(())
    } else {
        Err(GraphError::InvalidNodeKey)
    }
}

pub(crate) fn validate_edge_key(key: &str) -> Result<(), GraphError> {
    if key_valid(key) {
        Ok(())
    } else {
        Err(GraphError::InvalidEdgeKey)
    }
}

fn identifier_valid(value: &str, max: usize) -> bool {
    (1..=max).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// PD-003: `[A-Za-z0-9_]{1,63}`.
pub(crate) fn validate_label(label: &str) -> Result<(), GraphError> {
    if identifier_valid(label, MAX_LABEL_BYTES) {
        Ok(())
    } else {
        Err(GraphError::InvalidLabel)
    }
}

pub(crate) fn validate_labels(labels: &BTreeSet<String>) -> Result<(), GraphError> {
    if labels.len() > MAX_LABELS {
        return Err(GraphError::TooManyLabels);
    }
    labels.iter().try_for_each(|label| validate_label(label))
}

/// REQ-007: `[A-Za-z0-9_]{1,63}`.
pub(crate) fn validate_relationship_type(relationship: &str) -> Result<(), GraphError> {
    if identifier_valid(relationship, MAX_RELATIONSHIP_TYPE_BYTES) {
        Ok(())
    } else {
        Err(GraphError::InvalidRelationshipType)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_names_follow_collection_name_rules() {
        for valid in ["g", "a-b_c9", &"a".repeat(63)] {
            assert_eq!(validate_graph_name(valid), Ok(()), "{valid}");
        }
        for invalid in [
            "",
            "9a",
            "_a",
            "-a",
            "A",
            "aB",
            "a b",
            "a.b",
            "é",
            &"a".repeat(64),
        ] {
            assert_eq!(
                validate_graph_name(invalid),
                Err(GraphError::InvalidGraphName),
                "{invalid}"
            );
        }
    }

    #[test]
    fn keys_are_1_to_256_utf8_bytes() {
        assert_eq!(validate_node_key("k"), Ok(()));
        assert_eq!(validate_node_key(&"x".repeat(256)), Ok(()));
        // 128 two-byte characters = 256 bytes.
        assert_eq!(validate_node_key(&"é".repeat(128)), Ok(()));
        assert_eq!(validate_node_key(""), Err(GraphError::InvalidNodeKey));
        assert_eq!(
            validate_node_key(&"x".repeat(257)),
            Err(GraphError::InvalidNodeKey)
        );
        assert_eq!(
            validate_node_key(&"é".repeat(129)),
            Err(GraphError::InvalidNodeKey)
        );
        assert_eq!(validate_edge_key(""), Err(GraphError::InvalidEdgeKey));
        assert_eq!(validate_edge_key(&"x".repeat(256)), Ok(()));
    }

    #[test]
    fn labels_and_relationship_types_are_ascii_identifiers() {
        for valid in ["A", "z9_", "_", &"L".repeat(63)] {
            assert_eq!(validate_label(valid), Ok(()));
            assert_eq!(validate_relationship_type(valid), Ok(()));
        }
        for invalid in ["", "a-b", "a b", "é", &"L".repeat(64)] {
            assert_eq!(validate_label(invalid), Err(GraphError::InvalidLabel));
            assert_eq!(
                validate_relationship_type(invalid),
                Err(GraphError::InvalidRelationshipType)
            );
        }
        let eight: BTreeSet<String> = (0..8).map(|i| format!("L{i}")).collect();
        assert_eq!(validate_labels(&eight), Ok(()));
        let nine: BTreeSet<String> = (0..9).map(|i| format!("L{i}")).collect();
        assert_eq!(validate_labels(&nine), Err(GraphError::TooManyLabels));
    }
}
