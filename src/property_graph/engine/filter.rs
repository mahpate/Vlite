//! Property conditions and node filters.
//!
//! Evaluation is identical to `vlite::metadata::matches`: `Eq` is exact
//! value equality (so `Integer(1)` never equals `Float(1.0)`), `Range` only
//! compares Integer with Integer or Float with Float bounds (inclusive), and
//! `GeoRadius` only matches `GeoPoint` values within the haversine distance.
//! A missing field never matches. Validation mirrors
//! `vlite::metadata::validate_filter`.

use super::error::GraphError;
use super::limits::{MAX_CONDITIONS, MAX_FIELD_NAME_BYTES};
use super::model::{GraphNode, Properties, PropertyCondition, PropertyValue, validate_labels};
use super::properties;
use std::cmp::Ordering;
use std::collections::BTreeSet;

/// Labels and property conditions a node must all match. The default
/// (no labels, no conditions) matches every node.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeFilter {
    pub labels: BTreeSet<String>,
    pub conditions: Vec<PropertyCondition>,
}

impl NodeFilter {
    pub fn matches(&self, node: &GraphNode) -> bool {
        self.labels.is_subset(&node.labels) && conditions_match(&node.properties, &self.conditions)
    }

    pub(super) fn validate(&self) -> Result<(), GraphError> {
        validate_labels(&self.labels)?;
        validate_conditions(&self.conditions)
    }
}

fn numeric_compare(a: &PropertyValue, b: &PropertyValue) -> Option<Ordering> {
    match (a, b) {
        (PropertyValue::Integer(a), PropertyValue::Integer(b)) => Some(a.cmp(b)),
        (PropertyValue::Float(a), PropertyValue::Float(b)) => a.partial_cmp(b),
        _ => None,
    }
}

/// Haversine distance on a sphere of radius 6,371,008.8 m, computed exactly
/// as `vlite::metadata::geo_distance_meters`.
pub(crate) fn geo_distance_meters(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const EARTH_RADIUS_METERS: f64 = 6_371_008.8;
    let radians = std::f64::consts::PI / 180.0;
    let (lat1, lat2) = (lat1 * radians, lat2 * radians);
    let dlat = lat2 - lat1;
    let dlon = (lon2 - lon1) * radians;
    let a = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_METERS * a.sqrt().atan2((1.0 - a).max(0.0).sqrt())
}

/// True when `properties` satisfy every condition.
pub fn conditions_match(properties: &Properties, conditions: &[PropertyCondition]) -> bool {
    conditions.iter().all(|condition| match condition {
        PropertyCondition::Eq { field, value } => properties.get(field) == Some(value),
        PropertyCondition::Range { field, min, max } => {
            properties.get(field).is_some_and(|value| {
                min.as_ref().is_none_or(|min| {
                    numeric_compare(value, min).is_some_and(|ordering| ordering.is_ge())
                }) && max.as_ref().is_none_or(|max| {
                    numeric_compare(value, max).is_some_and(|ordering| ordering.is_le())
                })
            })
        }
        PropertyCondition::GeoRadius {
            field,
            latitude,
            longitude,
            radius_meters,
        } => properties.get(field).is_some_and(|value| {
            if let PropertyValue::GeoPoint {
                latitude: lat,
                longitude: lon,
            } = value
            {
                geo_distance_meters(*latitude, *longitude, *lat, *lon) <= *radius_meters
            } else {
                false
            }
        }),
    })
}

/// The `vlite::metadata::validate_filter` rules. Invalid values report
/// `InvalidProperties` (as `validate_filter` reports `InvalidMetadata`);
/// structural problems report `InvalidQuery`.
pub(super) fn validate_conditions(conditions: &[PropertyCondition]) -> Result<(), GraphError> {
    if conditions.len() > MAX_CONDITIONS {
        return Err(GraphError::InvalidQuery(
            "filters have at most 64 conditions",
        ));
    }
    for condition in conditions {
        let field = match condition {
            PropertyCondition::Eq { field, value } => {
                properties::validate_value(value)?;
                field
            }
            PropertyCondition::Range { field, min, max } => {
                if min.is_none() && max.is_none() {
                    return Err(GraphError::InvalidQuery("range requires min or max"));
                }
                for value in min.iter().chain(max) {
                    properties::validate_value(value)?;
                    if !matches!(value, PropertyValue::Integer(_) | PropertyValue::Float(_)) {
                        return Err(GraphError::InvalidQuery(
                            "ranges require integer or float bounds",
                        ));
                    }
                }
                if let (Some(min), Some(max)) = (min, max)
                    && numeric_compare(min, max).is_none_or(|ordering| ordering.is_gt())
                {
                    return Err(GraphError::InvalidQuery(
                        "range bounds must have the same numeric type and min <= max",
                    ));
                }
                field
            }
            PropertyCondition::GeoRadius {
                field,
                latitude,
                longitude,
                radius_meters,
            } => {
                if !latitude.is_finite()
                    || !(-90.0..=90.0).contains(latitude)
                    || !longitude.is_finite()
                    || !(-180.0..=180.0).contains(longitude)
                    || !radius_meters.is_finite()
                    || *radius_meters < 0.0
                {
                    return Err(GraphError::InvalidQuery(
                        "geo radius requires valid coordinates and nonnegative finite meters",
                    ));
                }
                field
            }
        };
        if !(1..=MAX_FIELD_NAME_BYTES).contains(&field.len()) {
            return Err(GraphError::InvalidQuery(
                "field names must be 1..=256 UTF-8 bytes",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(value: i64) -> PropertyValue {
        PropertyValue::Integer(value)
    }

    fn float(value: f64) -> PropertyValue {
        PropertyValue::Float(value)
    }

    fn geo(latitude: f64, longitude: f64) -> PropertyValue {
        PropertyValue::GeoPoint {
            latitude,
            longitude,
        }
    }

    fn eq(value: PropertyValue) -> PropertyCondition {
        PropertyCondition::Eq {
            field: "f".into(),
            value,
        }
    }

    fn range(min: Option<PropertyValue>, max: Option<PropertyValue>) -> PropertyCondition {
        PropertyCondition::Range {
            field: "f".into(),
            min,
            max,
        }
    }

    fn radius(latitude: f64, longitude: f64, radius_meters: f64) -> PropertyCondition {
        PropertyCondition::GeoRadius {
            field: "f".into(),
            latitude,
            longitude,
            radius_meters,
        }
    }

    fn with(value: PropertyValue) -> Properties {
        Properties::from([("f".to_owned(), value)])
    }

    #[test]
    fn condition_table() {
        // One degree of latitude on the reference sphere.
        let degree = geo_distance_meters(0.0, 0.0, 1.0, 0.0);
        assert!((degree - 111_195.08).abs() < 0.01, "{degree}");
        let table: Vec<(PropertyValue, PropertyCondition, bool)> = vec![
            (int(1), eq(int(1)), true),
            (int(1), eq(float(1.0)), false),
            (float(1.0), eq(int(1)), false),
            (float(0.0), eq(float(-0.0)), true),
            (
                PropertyValue::String("a".into()),
                eq(PropertyValue::String("a".into())),
                true,
            ),
            (
                PropertyValue::String("a".into()),
                eq(PropertyValue::String("A".into())),
                false,
            ),
            (
                PropertyValue::Boolean(true),
                eq(PropertyValue::Boolean(true)),
                true,
            ),
            (PropertyValue::Boolean(true), eq(int(1)), false),
            (geo(1.0, 2.0), eq(geo(1.0, 2.0)), true),
            (geo(1.0, 2.0), eq(geo(2.0, 1.0)), false),
            (int(5), range(Some(int(5)), Some(int(5))), true),
            (int(4), range(Some(int(5)), None), false),
            (int(6), range(None, Some(int(5))), false),
            (int(i64::MIN), range(None, Some(int(i64::MIN))), true),
            (int(5), range(Some(float(1.0)), None), false),
            (float(5.0), range(Some(int(1)), None), false),
            (float(2.5), range(Some(float(2.5)), Some(float(3.0))), true),
            (float(-0.0), range(Some(float(0.0)), Some(float(0.0))), true),
            (float(3.0000001), range(None, Some(float(3.0))), false),
            (
                PropertyValue::String("5".into()),
                range(Some(int(1)), None),
                false,
            ),
            (
                PropertyValue::Boolean(true),
                range(Some(int(0)), None),
                false,
            ),
            (geo(1.0, 0.0), range(Some(int(0)), None), false),
            (geo(0.0, 0.0), radius(0.0, 0.0, 0.0), true),
            (geo(1.0, 0.0), radius(0.0, 0.0, degree), true),
            (geo(1.0, 0.0), radius(0.0, 0.0, degree.next_down()), false),
            (geo(-90.0, 0.0), radius(90.0, 0.0, 30_000_000.0), true),
            (geo(0.0, 180.0), radius(0.0, -180.0, 1.0), true),
            (int(0), radius(0.0, 0.0, 1e9), false),
            (
                PropertyValue::String("0,0".into()),
                radius(0.0, 0.0, 1e9),
                false,
            ),
        ];
        for (value, condition, expected) in table {
            let properties = with(value.clone());
            assert_eq!(
                conditions_match(&properties, std::slice::from_ref(&condition)),
                expected,
                "{value:?} {condition:?}"
            );
            // A missing field never matches.
            assert!(!conditions_match(
                &Properties::new(),
                std::slice::from_ref(&condition)
            ));
            let renamed = Properties::from([("g".to_owned(), value)]);
            assert!(!conditions_match(&renamed, &[condition]));
        }
    }

    #[test]
    fn conditions_are_a_conjunction() {
        let properties = Properties::from([
            ("a".to_owned(), int(1)),
            ("b".to_owned(), PropertyValue::Boolean(true)),
        ]);
        let a = PropertyCondition::Eq {
            field: "a".into(),
            value: int(1),
        };
        let b = PropertyCondition::Eq {
            field: "b".into(),
            value: PropertyValue::Boolean(false),
        };
        assert!(conditions_match(&properties, &[]));
        assert!(conditions_match(&properties, std::slice::from_ref(&a)));
        assert!(!conditions_match(&properties, &[a.clone(), b.clone()]));
        assert!(!conditions_match(&properties, &[b, a]));
    }

    #[test]
    fn node_filters_require_every_label_and_condition() {
        let node = GraphNode {
            id: super::super::model::NodeId(1),
            key: "n".into(),
            labels: ["A", "B"].map(String::from).into(),
            properties: with(int(3)),
            revision: 1,
        };
        let filter = |labels: &[&str], conditions: Vec<PropertyCondition>| NodeFilter {
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
            conditions,
        };
        assert!(NodeFilter::default().matches(&node));
        assert!(filter(&["A"], vec![]).matches(&node));
        assert!(filter(&["A", "B"], vec![eq(int(3))]).matches(&node));
        assert!(!filter(&["A", "C"], vec![]).matches(&node));
        assert!(!filter(&["A"], vec![eq(int(4))]).matches(&node));
    }

    #[test]
    fn validation_mirrors_the_metadata_filter_rules() {
        let valid = vec![
            eq(float(-0.0)),
            eq(PropertyValue::String("x".repeat(4096))),
            range(Some(int(1)), Some(int(1))),
            range(None, Some(float(2.0))),
            radius(-90.0, 180.0, 0.0),
            PropertyCondition::Eq {
                field: "k".repeat(256),
                value: int(0),
            },
        ];
        assert_eq!(validate_conditions(&valid), Ok(()));
        assert_eq!(validate_conditions(&vec![eq(int(0)); 64]), Ok(()));
        assert!(matches!(
            validate_conditions(&vec![eq(int(0)); 65]),
            Err(GraphError::InvalidQuery(_))
        ));
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for condition in [eq(float(bad)), range(Some(float(bad)), None)] {
                assert!(matches!(
                    validate_conditions(&[condition]),
                    Err(GraphError::InvalidProperties(_))
                ));
            }
            for condition in [
                radius(bad, 0.0, 1.0),
                radius(0.0, bad, 1.0),
                radius(0.0, 0.0, bad),
            ] {
                assert!(matches!(
                    validate_conditions(&[condition]),
                    Err(GraphError::InvalidQuery(_))
                ));
            }
        }
        for condition in [
            range(None, None),
            range(Some(int(2)), Some(int(1))),
            range(Some(int(1)), Some(float(2.0))),
            range(Some(PropertyValue::Boolean(true)), None),
            range(Some(PropertyValue::String("a".into())), None),
            radius(0.0, 0.0, -1.0),
            radius(90.5, 0.0, 1.0),
            radius(0.0, 180.5, 1.0),
            PropertyCondition::Eq {
                field: String::new(),
                value: int(0),
            },
            PropertyCondition::Eq {
                field: "k".repeat(257),
                value: int(0),
            },
        ] {
            assert!(
                matches!(
                    validate_conditions(std::slice::from_ref(&condition)),
                    Err(GraphError::InvalidQuery(_))
                ),
                "{condition:?}"
            );
        }
        assert!(matches!(
            validate_conditions(&[eq(PropertyValue::String("x".repeat(4097)))]),
            Err(GraphError::InvalidProperties(_))
        ));
        assert!(matches!(
            validate_conditions(&[eq(geo(91.0, 0.0))]),
            Err(GraphError::InvalidProperties(_))
        ));
        let too_many_labels = NodeFilter {
            labels: (0..9).map(|index| format!("L{index}")).collect(),
            conditions: vec![],
        };
        assert_eq!(too_many_labels.validate(), Err(GraphError::TooManyLabels));
        let bad_label = NodeFilter {
            labels: ["a-b".to_owned()].into(),
            conditions: vec![],
        };
        assert_eq!(bad_label.validate(), Err(GraphError::InvalidLabel));
    }
}
