use super::super::error::GraphError;
use super::super::model::PropertyValue;
use super::{Compare, Plan, Predicate, SortDirection};
use std::cmp::Ordering;

#[derive(Clone, Debug)]
pub(crate) enum BoundValue {
    Node(u64),
    Edge(u64),
    Path { nodes: Vec<u64>, edges: Vec<u64> },
}

pub(crate) type BindingRow = std::collections::BTreeMap<String, BoundValue>;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OutputValue {
    Scalar(Option<PropertyValue>),
    Node(super::super::model::GraphNode),
    Edge {
        edge: super::super::model::GraphEdge,
        properties: super::super::model::Properties,
    },
    Path {
        nodes: Vec<u64>,
        edges: Vec<u64>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CanonicalValue {
    Entity(u64),
    Path {
        length: usize,
        edges: Vec<u64>,
        nodes: Vec<u64>,
    },
}

pub(crate) fn canonical_identity(row: &BindingRow, plan: &Plan) -> Vec<CanonicalValue> {
    plan.statement
        .pattern
        .iter()
        .flat_map(|pattern| {
            let mut names = Vec::new();
            if let Some(name) = &pattern.start.variable {
                names.push(name.text.as_str());
            }
            if let Some((relationship, end)) = &pattern.relationship {
                if let Some(name) = &relationship.variable {
                    names.push(name.text.as_str());
                }
                if let Some(name) = &end.variable {
                    names.push(name.text.as_str());
                }
            }
            if let Some(name) = &pattern.path_variable {
                names.push(name.text.as_str());
            }
            names
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter_map(|name| row.get(name))
        .map(|value| match value {
            BoundValue::Node(id) | BoundValue::Edge(id) => CanonicalValue::Entity(*id),
            BoundValue::Path { nodes, edges } => CanonicalValue::Path {
                length: edges.len(),
                edges: edges.clone(),
                nodes: nodes.clone(),
            },
        })
        .collect()
}

pub(crate) fn evaluate_predicate(
    predicate: &Predicate,
    row: &BindingRow,
    plan: &Plan,
    meter: &mut super::meter::Meter<'_, impl super::meter::MeterClock>,
    graph: &super::super::state::Graph,
    as_of_ms: Option<i64>,
    ttl_clock_ms: u64,
) -> Result<Option<bool>, GraphError> {
    match predicate {
        Predicate::And(terms) => {
            let mut result = Some(true);
            for term in terms {
                let value =
                    evaluate_predicate(term, row, plan, meter, graph, as_of_ms, ttl_clock_ms)?;
                result = and(result, value);
            }
            Ok(result)
        }
        Predicate::Or(terms) => {
            let mut result = Some(false);
            for term in terms {
                let value =
                    evaluate_predicate(term, row, plan, meter, graph, as_of_ms, ttl_clock_ms)?;
                result = or(result, value);
            }
            Ok(result)
        }
        Predicate::Not(term) => {
            Ok(
                evaluate_predicate(term, row, plan, meter, graph, as_of_ms, ttl_clock_ms)?
                    .map(|value| !value),
            )
        }
        Predicate::Compare {
            variable,
            field,
            operation,
            parameter,
        } => {
            meter.charge()?;
            let actual = bound_properties(row, &variable.text, graph, as_of_ms, ttl_clock_ms)?
                .and_then(|properties| properties.get(&field.text));
            let expected = plan
                .parameters
                .get(&parameter.name)
                .expect("validated parameter")
                .clone();
            compare(actual, *operation, Some(&expected))
        }
        Predicate::IsNull {
            variable,
            field,
            negated,
        } => {
            meter.charge()?;
            let actual = bound_properties(row, &variable.text, graph, as_of_ms, ttl_clock_ms)?
                .and_then(|properties| properties.get(&field.text));
            Ok(Some(if *negated {
                actual.is_some()
            } else {
                actual.is_none()
            }))
        }
    }
}

pub(crate) fn bound_properties<'a>(
    row: &BindingRow,
    variable: &str,
    graph: &'a super::super::state::Graph,
    as_of_ms: Option<i64>,
    ttl_clock_ms: u64,
) -> Result<Option<&'a super::super::model::Properties>, GraphError> {
    let Some(value) = row.get(variable) else {
        return Ok(None);
    };
    match value {
        BoundValue::Node(id) => Ok(graph
            .node(super::super::model::NodeId(*id))
            .map(|node| &node.properties)),
        BoundValue::Edge(id) => {
            let Some(edge) = graph.edge(super::super::model::EdgeId(*id)) else {
                return Ok(None);
            };
            Ok(edge_properties_at(graph, edge, as_of_ms, ttl_clock_ms))
        }
        BoundValue::Path { .. } => Ok(None),
    }
}

pub(crate) fn active_edge_properties<'a>(
    graph: &'a super::super::state::Graph,
    edge: &'a super::super::model::GraphEdge,
    as_of_ms: Option<i64>,
    ttl_clock_ms: u64,
    meter: &mut super::meter::Meter<'_, impl super::meter::MeterClock>,
) -> Result<Option<&'a super::super::model::Properties>, GraphError> {
    if graph.edge_expired(edge, ttl_clock_ms) {
        return Ok(None);
    }
    if let Some(properties) = edge_properties_at(graph, edge, as_of_ms, ttl_clock_ms) {
        if graph.segments(edge.id).is_some() {
            meter.allocate(8 * temporal_edge_field_count(graph, edge) as u64)?;
        }
        Ok(Some(properties))
    } else {
        Ok(None)
    }
}

fn edge_properties_at<'a>(
    graph: &'a super::super::state::Graph,
    edge: &'a super::super::model::GraphEdge,
    as_of_ms: Option<i64>,
    ttl_clock_ms: u64,
) -> Option<&'a super::super::model::Properties> {
    if graph.edge_expired(edge, ttl_clock_ms) {
        return None;
    }
    if let Some(segments) = graph.segments(edge.id) {
        let segment =
            as_of_ms.and_then(|instant| super::super::temporal::segment_at(segments, instant))?;
        Some(&segment.properties)
    } else {
        Some(&edge.properties)
    }
}

fn temporal_edge_field_count(
    graph: &super::super::state::Graph,
    edge: &super::super::model::GraphEdge,
) -> usize {
    // The logical scratch model counts fields in the fixture entity, not Rust
    // struct bytes. The base entity has seven fields and segments adds one.
    8 + usize::from(graph.edge_expiry(edge.id).is_some())
}

fn compare(
    left: Option<&PropertyValue>,
    operation: Compare,
    right: Option<&PropertyValue>,
) -> Result<Option<bool>, GraphError> {
    let (Some(left), Some(right)) = (left, right) else {
        return Ok(None);
    };
    if matches!(operation, Compare::Equal | Compare::NotEqual) {
        let equal = typed_equal(left, right);
        return Ok(Some(if operation == Compare::Equal {
            equal
        } else {
            !equal
        }));
    }
    let ordering = match (left, right) {
        (PropertyValue::Integer(left), PropertyValue::Integer(right)) => left.cmp(right),
        (PropertyValue::Float(left), PropertyValue::Float(right)) => {
            left.partial_cmp(right).expect("stored floats are finite")
        }
        (PropertyValue::String(left), PropertyValue::String(right)) => {
            left.as_bytes().cmp(right.as_bytes())
        }
        _ => {
            return Err(GraphError::InvalidQuery("subset comparison type mismatch"));
        }
    };
    Ok(Some(match operation {
        Compare::Less => ordering == Ordering::Less,
        Compare::LessEqual => ordering != Ordering::Greater,
        Compare::Greater => ordering == Ordering::Greater,
        Compare::GreaterEqual => ordering != Ordering::Less,
        Compare::Equal | Compare::NotEqual => unreachable!(),
    }))
}

pub(crate) fn typed_equal(left: &PropertyValue, right: &PropertyValue) -> bool {
    match (left, right) {
        (PropertyValue::String(left), PropertyValue::String(right)) => left == right,
        (PropertyValue::Integer(left), PropertyValue::Integer(right)) => left == right,
        (PropertyValue::Float(left), PropertyValue::Float(right)) => left == right,
        (PropertyValue::Boolean(left), PropertyValue::Boolean(right)) => left == right,
        (
            PropertyValue::GeoPoint {
                latitude: left_lat,
                longitude: left_lon,
            },
            PropertyValue::GeoPoint {
                latitude: right_lat,
                longitude: right_lon,
            },
        ) => left_lat == right_lat && left_lon == right_lon,
        _ => false,
    }
}

pub(crate) fn compare_sort_values(
    left: &Option<PropertyValue>,
    right: &Option<PropertyValue>,
    direction: SortDirection,
) -> Ordering {
    let ordering = match (left, right) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(left), Some(right)) => {
            let rank = scalar_rank(left).cmp(&scalar_rank(right));
            if rank != Ordering::Equal {
                rank
            } else {
                match (left, right) {
                    (PropertyValue::Boolean(left), PropertyValue::Boolean(right)) => {
                        left.cmp(right)
                    }
                    (PropertyValue::Integer(left), PropertyValue::Integer(right)) => {
                        left.cmp(right)
                    }
                    (PropertyValue::Float(left), PropertyValue::Float(right)) => {
                        left.partial_cmp(right).expect("stored floats are finite")
                    }
                    (PropertyValue::String(left), PropertyValue::String(right)) => {
                        left.as_bytes().cmp(right.as_bytes())
                    }
                    (
                        PropertyValue::GeoPoint {
                            latitude: left_lat,
                            longitude: left_lon,
                        },
                        PropertyValue::GeoPoint {
                            latitude: right_lat,
                            longitude: right_lon,
                        },
                    ) => left_lat
                        .partial_cmp(right_lat)
                        .expect("stored coordinates are finite")
                        .then_with(|| {
                            left_lon
                                .partial_cmp(right_lon)
                                .expect("stored coordinates are finite")
                        }),
                    _ => Ordering::Equal,
                }
            }
        }
    };
    match direction {
        SortDirection::Ascending => ordering,
        SortDirection::Descending => ordering.reverse(),
    }
}

fn scalar_rank(value: &PropertyValue) -> u8 {
    match value {
        PropertyValue::Boolean(_) => 0,
        PropertyValue::Integer(_) => 1,
        PropertyValue::Float(_) => 2,
        PropertyValue::String(_) => 3,
        PropertyValue::GeoPoint { .. } => 4,
    }
}

pub(super) fn and(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

pub(super) fn or(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}
