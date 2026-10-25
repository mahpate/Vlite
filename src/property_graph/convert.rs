//! Lossless conversions between public `vlite` types and the engine's mirror
//! types (Section 3.2.1 rule 2, ITEM-012), plus `GraphError` → `Error`.
//!
//! Inputs are validated with the existing `metadata` validators before they
//! are converted, so the engine never sees values `vlite` would reject (for
//! example non-finite floats). Node, edge and query APIs that consume these
//! conversions arrive with EPIC-004/EPIC-005.

use crate::metadata::{self, Condition, Metadata, MetadataValue};
use crate::{Error, Expected, Result};

use super::engine::{GraphError, Precondition, Properties, PropertyCondition, PropertyValue};

pub(crate) fn property_value(value: &MetadataValue) -> PropertyValue {
    match value {
        MetadataValue::String(value) => PropertyValue::String(value.clone()),
        MetadataValue::Integer(value) => PropertyValue::Integer(*value),
        MetadataValue::Float(value) => PropertyValue::Float(*value),
        MetadataValue::Boolean(value) => PropertyValue::Boolean(*value),
        MetadataValue::GeoPoint {
            latitude,
            longitude,
        } => PropertyValue::GeoPoint {
            latitude: *latitude,
            longitude: *longitude,
        },
    }
}

pub(crate) fn metadata_value(value: &PropertyValue) -> MetadataValue {
    match value {
        PropertyValue::String(value) => MetadataValue::String(value.clone()),
        PropertyValue::Integer(value) => MetadataValue::Integer(*value),
        PropertyValue::Float(value) => MetadataValue::Float(*value),
        PropertyValue::Boolean(value) => MetadataValue::Boolean(*value),
        PropertyValue::GeoPoint {
            latitude,
            longitude,
        } => MetadataValue::GeoPoint {
            latitude: *latitude,
            longitude: *longitude,
        },
    }
}

/// Validates `metadata` with the record-metadata rules, then converts it.
pub(crate) fn properties_from_metadata(metadata: &Metadata) -> Result<Properties> {
    metadata::validate(metadata)?;
    Ok(metadata
        .iter()
        .map(|(field, value)| (field.clone(), property_value(value)))
        .collect())
}

pub(crate) fn metadata_from_properties(properties: &Properties) -> Metadata {
    properties
        .iter()
        .map(|(field, value)| (field.clone(), metadata_value(value)))
        .collect()
}

/// Validates `conditions` with the record-filter rules, then converts them.
pub(crate) fn property_conditions(conditions: &[Condition]) -> Result<Vec<PropertyCondition>> {
    metadata::validate_filter(conditions)?;
    Ok(conditions
        .iter()
        .map(|condition| match condition {
            Condition::Eq { field, value } => PropertyCondition::Eq {
                field: field.clone(),
                value: property_value(value),
            },
            Condition::Range { field, min, max } => PropertyCondition::Range {
                field: field.clone(),
                min: min.as_ref().map(property_value),
                max: max.as_ref().map(property_value),
            },
            Condition::GeoRadius {
                field,
                latitude,
                longitude,
                radius_meters,
            } => PropertyCondition::GeoRadius {
                field: field.clone(),
                latitude: *latitude,
                longitude: *longitude,
                radius_meters: *radius_meters,
            },
        })
        .collect())
}

pub(crate) fn condition(condition: &PropertyCondition) -> Condition {
    match condition {
        PropertyCondition::Eq { field, value } => Condition::Eq {
            field: field.clone(),
            value: metadata_value(value),
        },
        PropertyCondition::Range { field, min, max } => Condition::Range {
            field: field.clone(),
            min: min.as_ref().map(metadata_value),
            max: max.as_ref().map(metadata_value),
        },
        PropertyCondition::GeoRadius {
            field,
            latitude,
            longitude,
            radius_meters,
        } => Condition::GeoRadius {
            field: field.clone(),
            latitude: *latitude,
            longitude: *longitude,
            radius_meters: *radius_meters,
        },
    }
}

pub(crate) fn precondition(expected: Expected) -> Precondition {
    match expected {
        Expected::Any => Precondition::Any,
        Expected::Absent => Precondition::Absent,
        Expected::Revision(revision) => Precondition::Revision(revision),
    }
}

pub(crate) fn expected(precondition: Precondition) -> Expected {
    match precondition {
        Precondition::Any => Expected::Any,
        Precondition::Absent => Expected::Absent,
        Precondition::Revision(revision) => Expected::Revision(revision),
    }
}

/// Maps engine errors to public errors. Graph lifecycle, size and property
/// errors have exact equivalents, as do node, edge, attachment and
/// precondition errors (EPIC-004) and read-limit and stale-cursor errors
/// (EPIC-005); key, label and type validation errors, malformed or
/// mismatched cursors and invalid read queries become `InvalidQuery`.
pub(crate) fn graph_error(error: GraphError) -> Error {
    match error {
        GraphError::InvalidGraphName => Error::InvalidGraphName,
        GraphError::GraphNameExists(_) => Error::GraphExists,
        GraphError::GraphNotFound(_) => Error::GraphNotFound,
        GraphError::GraphNotEmpty(_) => Error::GraphNotEmpty,
        GraphError::GraphIdUnavailable(_) => Error::TooManyGraphs,
        GraphError::BatchSize(_) => Error::BatchTooLarge,
        GraphError::InvalidProperties(reason) => Error::InvalidMetadata(reason),
        GraphError::InvalidNodeKey => Error::InvalidQuery("node keys must be 1..=256 UTF-8 bytes"),
        GraphError::InvalidEdgeKey => Error::InvalidQuery("edge keys must be 1..=256 UTF-8 bytes"),
        GraphError::InvalidLabel => Error::InvalidQuery("labels must match ^[A-Za-z0-9_]{1,63}$"),
        GraphError::TooManyLabels => Error::InvalidQuery("a node has at most 8 labels"),
        GraphError::InvalidRelationshipType => {
            Error::InvalidQuery("relationship types must match ^[A-Za-z0-9_]{1,63}$")
        }
        GraphError::NodeKeyExists(_) => Error::GraphKeyExists,
        GraphError::EdgeKeyExists(_) | GraphError::EdgeKeyConflict(_) => Error::GraphKeyConflict,
        GraphError::NodeNotFound(_) | GraphError::UnknownNodeKey(_) => Error::GraphNodeNotFound,
        GraphError::EdgeNotFound(_) | GraphError::UnknownEdgeKey(_) => Error::GraphEdgeNotFound,
        GraphError::NodeHasEdges { incident, .. } => Error::GraphNodeHasEdges { incident },
        GraphError::NodeAlreadyAttached { .. } | GraphError::RecordAlreadyAttached { .. } => {
            Error::GraphAttachmentConflict
        }
        GraphError::AttachmentNotFound { .. } => Error::GraphAttachmentNotFound,
        GraphError::PreconditionFailed { current } => Error::RevisionMismatch { current },
        GraphError::LimitExceedsCeiling {
            limit,
            requested,
            ceiling,
        } => Error::GraphLimitExceedsCeiling {
            limit,
            requested,
            ceiling,
        },
        GraphError::CursorStale => Error::GraphCursorStale,
        GraphError::InvalidCursor(reason) | GraphError::InvalidQuery(reason) => {
            Error::InvalidQuery(reason)
        }
        // A key/ID mismatch only arises from malformed bodies, never from
        // planned mutations.
        GraphError::NodeKeyMismatch(_) => Error::InvalidQuery("graph node key mismatch"),
        GraphError::NodeIdUnavailable(_)
        | GraphError::EdgeIdUnavailable(_)
        | GraphError::IdSpaceExhausted
        | GraphError::HighWaterDecrease => Error::InvalidQuery("graph ID space unavailable"),
        // Encoding, sequencing and invariant failures mean the batch or the
        // in-memory state is malformed, never a caller input problem.
        GraphError::Corrupt(_)
        | GraphError::UnknownEntryTag(_)
        | GraphError::InvalidVectorMutation(_)
        | GraphError::SequenceNotIncreasing { .. }
        | GraphError::InvariantViolation(_) => {
            Error::InvalidQuery("graph batch rejected by graph state")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::engine::{EdgeId, GraphId, NodeId, properties};
    use super::*;

    fn every_value() -> Vec<MetadataValue> {
        vec![
            MetadataValue::String(String::new()),
            MetadataValue::String("x".repeat(4096)),
            MetadataValue::Integer(i64::MIN),
            MetadataValue::Integer(0),
            MetadataValue::Integer(i64::MAX),
            MetadataValue::Float(-0.0),
            MetadataValue::Float(f64::MIN_POSITIVE),
            MetadataValue::Float(f64::MAX),
            MetadataValue::Boolean(true),
            MetadataValue::Boolean(false),
            MetadataValue::GeoPoint {
                latitude: -90.0,
                longitude: 180.0,
            },
            MetadataValue::GeoPoint {
                latitude: 47.6,
                longitude: -122.3,
            },
        ]
    }

    #[test]
    fn metadata_values_round_trip_losslessly_for_every_variant() {
        for value in every_value() {
            let converted = property_value(&value);
            // Variant names match one-to-one.
            assert_eq!(format!("{converted:?}"), format!("{value:?}"));
            let back = metadata_value(&converted);
            assert_eq!(back, value);
            if let (MetadataValue::Float(a), MetadataValue::Float(b)) = (&back, &value) {
                assert_eq!(a.to_bits(), b.to_bits(), "sign and bits kept");
            }
        }
    }

    #[test]
    fn metadata_converts_to_byte_identical_properties() {
        let metadata: Metadata = every_value()
            .into_iter()
            .enumerate()
            .map(|(index, value)| (format!("f{index:02}"), value))
            .chain([("k".repeat(256), MetadataValue::Boolean(true))])
            .collect();
        let converted = properties_from_metadata(&metadata).unwrap();
        assert_eq!(metadata_from_properties(&converted), metadata);
        assert_eq!(
            properties::encode(&converted).unwrap(),
            metadata::encode(&metadata)
        );
        assert_eq!(
            properties::validate(&converted).unwrap(),
            metadata::validate(&metadata).unwrap()
        );
        assert!(
            properties_from_metadata(&Metadata::new())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn invalid_metadata_is_rejected_before_conversion() {
        let reject = |value: MetadataValue| {
            let metadata = Metadata::from([("f".to_string(), value)]);
            matches!(
                properties_from_metadata(&metadata),
                Err(Error::InvalidMetadata(_))
            )
        };
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(reject(MetadataValue::Float(bad)));
            assert!(reject(MetadataValue::GeoPoint {
                latitude: bad,
                longitude: 0.0,
            }));
            assert!(reject(MetadataValue::GeoPoint {
                latitude: 0.0,
                longitude: bad,
            }));
        }
        assert!(reject(MetadataValue::GeoPoint {
            latitude: 90.5,
            longitude: 0.0,
        }));
        assert!(reject(MetadataValue::String("x".repeat(4097))));
        let empty_field = Metadata::from([(String::new(), MetadataValue::Boolean(true))]);
        assert!(properties_from_metadata(&empty_field).is_err());
        let long_field = Metadata::from([("k".repeat(257), MetadataValue::Boolean(true))]);
        assert!(properties_from_metadata(&long_field).is_err());
        let too_many: Metadata = (0..65)
            .map(|index| (format!("f{index}"), MetadataValue::Integer(index)))
            .collect();
        assert!(properties_from_metadata(&too_many).is_err());
        let too_big: Metadata = (0..17)
            .map(|index| (format!("f{index}"), MetadataValue::String("x".repeat(4096))))
            .collect();
        assert!(properties_from_metadata(&too_big).is_err());
    }

    #[test]
    fn conditions_round_trip_losslessly_for_every_variant() {
        let conditions = vec![
            Condition::Eq {
                field: "s".into(),
                value: MetadataValue::String("v".into()),
            },
            Condition::Eq {
                field: "g".into(),
                value: MetadataValue::GeoPoint {
                    latitude: 1.5,
                    longitude: -2.5,
                },
            },
            Condition::Eq {
                field: "b".into(),
                value: MetadataValue::Boolean(false),
            },
            Condition::Range {
                field: "i".into(),
                min: Some(MetadataValue::Integer(-3)),
                max: Some(MetadataValue::Integer(7)),
            },
            Condition::Range {
                field: "f".into(),
                min: None,
                max: Some(MetadataValue::Float(2.5)),
            },
            Condition::Range {
                field: "f".into(),
                min: Some(MetadataValue::Float(-0.0)),
                max: None,
            },
            Condition::GeoRadius {
                field: "g".into(),
                latitude: 90.0,
                longitude: -180.0,
                radius_meters: 0.0,
            },
        ];
        let converted = property_conditions(&conditions).unwrap();
        assert_eq!(converted.len(), conditions.len());
        for (converted, original) in converted.iter().zip(&conditions) {
            assert_eq!(format!("{converted:?}"), format!("{original:?}"));
            assert_eq!(&condition(converted), original);
        }
        assert!(property_conditions(&[]).unwrap().is_empty());
    }

    #[test]
    fn invalid_conditions_are_rejected_before_conversion() {
        let eq = |value| Condition::Eq {
            field: "f".into(),
            value,
        };
        let geo = |latitude, longitude, radius_meters| Condition::GeoRadius {
            field: "g".into(),
            latitude,
            longitude,
            radius_meters,
        };
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for condition in [
                eq(MetadataValue::Float(bad)),
                Condition::Range {
                    field: "f".into(),
                    min: Some(MetadataValue::Float(bad)),
                    max: None,
                },
                geo(bad, 0.0, 1.0),
                geo(0.0, bad, 1.0),
                geo(0.0, 0.0, bad),
            ] {
                assert!(
                    property_conditions(std::slice::from_ref(&condition)).is_err(),
                    "{condition:?}"
                );
            }
        }
        for condition in [
            Condition::Range {
                field: "f".into(),
                min: None,
                max: None,
            },
            Condition::Range {
                field: "f".into(),
                min: Some(MetadataValue::Integer(2)),
                max: Some(MetadataValue::Integer(1)),
            },
            Condition::Range {
                field: "f".into(),
                min: Some(MetadataValue::Integer(1)),
                max: Some(MetadataValue::Float(2.0)),
            },
            Condition::Range {
                field: "f".into(),
                min: Some(MetadataValue::Boolean(true)),
                max: None,
            },
            geo(0.0, 0.0, -1.0),
            Condition::Eq {
                field: String::new(),
                value: MetadataValue::Boolean(true),
            },
        ] {
            assert!(
                property_conditions(std::slice::from_ref(&condition)).is_err(),
                "{condition:?}"
            );
        }
        let too_many = vec![eq(MetadataValue::Boolean(true)); 65];
        assert!(matches!(
            property_conditions(&too_many),
            Err(Error::InvalidFilter(_))
        ));
    }

    #[test]
    fn expected_round_trips_for_every_variant() {
        for value in [
            Expected::Any,
            Expected::Absent,
            Expected::Revision(0),
            Expected::Revision(u64::MAX),
        ] {
            let converted = precondition(value);
            assert_eq!(format!("{converted:?}"), format!("{value:?}"));
            assert_eq!(expected(converted), value);
        }
    }

    #[test]
    fn graph_errors_map_to_public_errors() {
        let graph = GraphId(3);
        let node = NodeId(1);
        assert!(matches!(
            graph_error(GraphError::InvalidGraphName),
            Error::InvalidGraphName
        ));
        assert!(matches!(
            graph_error(GraphError::GraphNameExists("g".into())),
            Error::GraphExists
        ));
        assert!(matches!(
            graph_error(GraphError::GraphNotFound(graph)),
            Error::GraphNotFound
        ));
        assert!(matches!(
            graph_error(GraphError::GraphNotEmpty(graph)),
            Error::GraphNotEmpty
        ));
        assert!(matches!(
            graph_error(GraphError::GraphIdUnavailable(graph)),
            Error::TooManyGraphs
        ));
        assert!(matches!(
            graph_error(GraphError::BatchSize("x")),
            Error::BatchTooLarge
        ));
        assert!(matches!(
            graph_error(GraphError::InvalidProperties("reason")),
            Error::InvalidMetadata("reason")
        ));
        assert!(matches!(
            graph_error(GraphError::NodeKeyExists("k".into())),
            Error::GraphKeyExists
        ));
        for error in [
            GraphError::EdgeKeyExists("k".into()),
            GraphError::EdgeKeyConflict("k".into()),
        ] {
            assert!(matches!(graph_error(error), Error::GraphKeyConflict));
        }
        for error in [
            GraphError::NodeNotFound(node),
            GraphError::UnknownNodeKey("k".into()),
        ] {
            assert!(matches!(graph_error(error), Error::GraphNodeNotFound));
        }
        for error in [
            GraphError::EdgeNotFound(EdgeId(1)),
            GraphError::UnknownEdgeKey("k".into()),
        ] {
            assert!(matches!(graph_error(error), Error::GraphEdgeNotFound));
        }
        assert!(matches!(
            graph_error(GraphError::NodeHasEdges { node, incident: 2 }),
            Error::GraphNodeHasEdges { incident: 2 }
        ));
        for error in [
            GraphError::NodeAlreadyAttached {
                node,
                collection: 0,
            },
            GraphError::RecordAlreadyAttached {
                collection: 0,
                record: 1,
                graph,
                node,
            },
        ] {
            assert!(matches!(graph_error(error), Error::GraphAttachmentConflict));
        }
        assert!(matches!(
            graph_error(GraphError::AttachmentNotFound {
                node,
                collection: 0,
            }),
            Error::GraphAttachmentNotFound
        ));
        for current in [None, Some(7)] {
            assert!(matches!(
                graph_error(GraphError::PreconditionFailed { current }),
                Error::RevisionMismatch { current: c } if c == current
            ));
        }
        assert!(matches!(
            graph_error(GraphError::LimitExceedsCeiling {
                limit: "max_hops",
                requested: 5,
                ceiling: 4,
            }),
            Error::GraphLimitExceedsCeiling {
                limit: "max_hops",
                requested: 5,
                ceiling: 4,
            }
        ));
        assert!(matches!(
            graph_error(GraphError::CursorStale),
            Error::GraphCursorStale
        ));
        assert!(matches!(
            graph_error(GraphError::InvalidCursor("corrupt cursor")),
            Error::InvalidQuery("corrupt cursor")
        ));
        assert!(matches!(
            graph_error(GraphError::InvalidQuery("limit must be at least 1")),
            Error::InvalidQuery("limit must be at least 1")
        ));
        let display = graph_error(GraphError::LimitExceedsCeiling {
            limit: "deadline_ms",
            requested: 5_001,
            ceiling: 5_000,
        })
        .to_string();
        assert_eq!(
            display,
            "graph limit deadline_ms = 5001 exceeds its ceiling 5000"
        );
        assert!(
            graph_error(GraphError::CursorStale)
                .to_string()
                .contains("cursor")
        );
        for error in [
            GraphError::Corrupt("x"),
            GraphError::UnknownEntryTag(12),
            GraphError::InvalidNodeKey,
            GraphError::InvalidEdgeKey,
            GraphError::InvalidLabel,
            GraphError::TooManyLabels,
            GraphError::InvalidRelationshipType,
            GraphError::InvalidVectorMutation("x"),
            GraphError::NodeIdUnavailable(node),
            GraphError::EdgeIdUnavailable(EdgeId(1)),
            GraphError::IdSpaceExhausted,
            GraphError::HighWaterDecrease,
            GraphError::NodeKeyMismatch(node),
            GraphError::SequenceNotIncreasing {
                last: 2,
                sequence: 1,
            },
            GraphError::InvariantViolation("x"),
        ] {
            assert!(
                matches!(graph_error(error.clone()), Error::InvalidQuery(_)),
                "{error:?}"
            );
        }
    }
}
