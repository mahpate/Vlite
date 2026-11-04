use super::super::error::GraphError;
use super::super::model::{EdgeId, GraphEdge, GraphNode, NodeId, Properties, PropertyValue};
use super::super::state::Graph;
use super::eval::{
    BindingRow, BoundValue, CanonicalValue, OutputValue, active_edge_properties,
    canonical_identity, compare_sort_values, evaluate_predicate,
};
use super::meter::{Meter, MeterClock, ReadBounds};
use super::{
    AnchorKind, Direction, NodePattern, ParameterRef, Plan, RelationshipPattern, ReturnValue,
    Statement, plan,
};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ReadResult {
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<Vec<OutputValue>>,
    pub(crate) as_of_ms: Option<i64>,
    pub(crate) ttl_clock_ms: u64,
    pub(crate) view_sequence: u64,
}

#[derive(Debug)]
pub(crate) enum ReadError {
    Query(super::super::language::LanguageError),
    Graph(GraphError),
    Runtime(&'static str),
    UnsupportedFeature(u16),
    At { error: GraphError, position: usize },
}

impl From<GraphError> for ReadError {
    fn from(error: GraphError) -> Self {
        Self::Graph(error)
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute<
    C: MeterClock,
    R: FnOnce(Option<i64>, u64, bool) -> Result<Option<i64>, ReadError>,
>(
    graph: &Graph,
    view_sequence: u64,
    temporal_capable: bool,
    ttl_clock_ms: u64,
    text: &str,
    parameters: &[plan::ParameterEntry],
    bounds: ReadBounds,
    clock: &C,
    resolve_as_of: R,
) -> Result<Option<ReadResult>, ReadError> {
    let bounds = bounds.validate().map_err(ReadError::Graph)?;
    let planned = plan::plan(text, parameters).map_err(|error| match error {
        plan::PlanError::Query(error) => ReadError::Query(error),
        plan::PlanError::InvalidMetadata(error) => ReadError::Graph(error),
    })?;
    validate_hops(&planned.statement, bounds.graph.max_hops)?;
    if !planned.statement.mutations.is_empty() {
        return Ok(None);
    }
    let mut meter = Meter::new(bounds, clock).map_err(ReadError::Graph)?;
    if let Some(error) = empty_key_error(&planned) {
        return Err(ReadError::Graph(error));
    }
    let explicit_as_of = planned
        .statement
        .as_of
        .as_ref()
        .map(|reference| integer_parameter(&planned, reference))
        .transpose()?;
    let as_of_ms = resolve_as_of(explicit_as_of, ttl_clock_ms, temporal_capable)?;

    let Some(pattern) = &planned.statement.pattern else {
        return Err(ReadError::Query(super::super::language::LanguageError {
            kind: super::super::language::LanguageErrorKind::Syntax,
            position: text.len(),
            message: "a read statement requires MATCH".into(),
        }));
    };
    let rows = enumerate_rows(graph, pattern, &planned, &mut meter, as_of_ms, ttl_clock_ms)?;
    let mut rows = canonical_sort(rows, &planned, &mut meter)?;
    if let Some(predicate) = &planned.statement.predicate {
        let mut filtered = Vec::with_capacity(rows.len());
        for row in rows {
            if evaluate_predicate(
                predicate,
                &row,
                &planned,
                &mut meter,
                graph,
                as_of_ms,
                ttl_clock_ms,
            )
            .map_err(ReadError::Graph)?
                == Some(true)
            {
                filtered.push(row);
            }
        }
        rows = filtered;
    }
    let returning = planned
        .statement
        .returning
        .as_ref()
        .expect("validated read statement");
    let projected = project_rows(
        graph,
        rows,
        returning,
        &planned,
        &mut meter,
        as_of_ms,
        ttl_clock_ms,
    )?;
    let mut projected = scalar_sort(projected, returning, &planned, &mut meter)?;
    let skip = returning
        .skip
        .as_ref()
        .map(|reference| integer_parameter(&planned, reference))
        .transpose()?
        .unwrap_or(0);
    let limit = returning
        .limit
        .as_ref()
        .map(|reference| integer_parameter(&planned, reference))
        .transpose()?;
    let skip = usize::try_from(skip).unwrap_or(usize::MAX);
    projected = projected.into_iter().skip(skip).collect();
    if let Some(limit) = limit {
        projected.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    }
    meter.returned(projected.len()).map_err(ReadError::Graph)?;
    check_output(&projected, &mut meter)?;
    Ok(Some(ReadResult {
        columns: returning
            .items
            .iter()
            .map(|item| {
                item.alias.as_ref().map_or_else(
                    || match &item.value {
                        ReturnValue::Variable(variable) => variable.text.clone(),
                        ReturnValue::Field(field) => {
                            format!("{}.{}", field.variable.text, field.field.text)
                        }
                    },
                    |alias| alias.text.clone(),
                )
            })
            .collect(),
        rows: projected.into_iter().map(|(_, cells)| cells).collect(),
        as_of_ms,
        ttl_clock_ms,
        view_sequence,
    }))
}

fn validate_hops(statement: &Statement, max_hops: u64) -> Result<(), ReadError> {
    let Some((relationship, _)) = statement
        .pattern
        .as_ref()
        .and_then(|pattern| pattern.relationship.as_ref())
    else {
        return Ok(());
    };
    let hops = relationship.max_hops.unwrap_or(1);
    if hops > max_hops {
        return Err(ReadError::At {
            error: GraphError::InvalidQuery("subset hop bound exceeds max_hops"),
            position: relationship
                .hop_position
                // The supported bound prefix `*1..` is four ASCII bytes.
                .map(|position| position + 4)
                .unwrap_or(relationship.position),
        });
    }
    Ok(())
}

fn empty_key_error(plan: &Plan) -> Option<GraphError> {
    let Some(pattern) = &plan.statement.pattern else {
        return None;
    };
    let node_anchors = std::iter::once(&pattern.start.anchor)
        .chain(pattern.relationship.iter().map(|(_, end)| &end.anchor));
    for anchor in node_anchors.flatten() {
        if anchor.kind == AnchorKind::Key
            && matches!(
                plan.parameters.get(&anchor.parameter.name),
                Some(PropertyValue::String(value)) if value.is_empty()
            )
        {
            return Some(GraphError::InvalidNodeKey);
        }
    }
    if let Some((relationship, _)) = &pattern.relationship
        && let Some(anchor) = &relationship.anchor
        && anchor.kind == AnchorKind::Key
        && matches!(
            plan.parameters.get(&anchor.parameter.name),
            Some(PropertyValue::String(value)) if value.is_empty()
        )
    {
        return Some(GraphError::InvalidEdgeKey);
    }
    None
}

fn integer_parameter(plan: &Plan, reference: &ParameterRef) -> Result<i64, ReadError> {
    match plan.parameters.get(&reference.name) {
        Some(PropertyValue::Integer(value)) => Ok(*value),
        _ => Err(ReadError::Query(super::super::language::LanguageError {
            kind: super::super::language::LanguageErrorKind::Parameter,
            position: reference.position,
            message: "parameter must be an integer".into(),
        })),
    }
}

fn node_candidates<'a>(
    graph: &'a Graph,
    pattern: &NodePattern,
    plan: &Plan,
    meter: &mut Meter<'_, impl MeterClock>,
) -> Result<Box<dyn Iterator<Item = &'a GraphNode> + 'a>, GraphError> {
    let Some(anchor) = &pattern.anchor else {
        return Ok(Box::new(graph.nodes()));
    };
    meter.charge()?;
    meter.allocate(8)?;
    let Some(value) = plan.parameters.get(&anchor.parameter.name) else {
        return Ok(Box::new(std::iter::empty()));
    };
    let node = match (anchor.kind, value) {
        (AnchorKind::Key, PropertyValue::String(key)) => graph.node_by_key(key),
        (AnchorKind::Id, PropertyValue::Integer(id)) => u64::try_from(*id)
            .ok()
            .and_then(|id| graph.node(NodeId(id))),
        _ => None,
    };
    Ok(Box::new(node.into_iter()))
}

enum EdgeCandidates<'a> {
    Scan(&'a Graph),
    Anchored(Option<&'a GraphEdge>),
}

impl EdgeCandidates<'_> {
    fn iter(&self) -> Box<dyn Iterator<Item = &GraphEdge> + '_> {
        match self {
            Self::Scan(graph) => Box::new(graph.edges()),
            Self::Anchored(edge) => Box::new(edge.iter().copied()),
        }
    }
}

fn edge_candidates<'a>(
    graph: &'a Graph,
    pattern: &RelationshipPattern,
    plan: &Plan,
    meter: &mut Meter<'_, impl MeterClock>,
) -> Result<EdgeCandidates<'a>, GraphError> {
    let Some(anchor) = &pattern.anchor else {
        return Ok(EdgeCandidates::Scan(graph));
    };
    meter.charge()?;
    meter.allocate(8)?;
    let Some(value) = plan.parameters.get(&anchor.parameter.name) else {
        return Ok(EdgeCandidates::Anchored(None));
    };
    let edge = match (anchor.kind, value) {
        (AnchorKind::Key, PropertyValue::String(key)) => graph.edge_by_key(key),
        (AnchorKind::Id, PropertyValue::Integer(id)) => u64::try_from(*id)
            .ok()
            .and_then(|id| graph.edge(EdgeId(id))),
        _ => None,
    };
    Ok(EdgeCandidates::Anchored(edge))
}

fn enumerate_rows(
    graph: &Graph,
    pattern: &super::Pattern,
    plan: &Plan,
    meter: &mut Meter<'_, impl MeterClock>,
    as_of_ms: Option<i64>,
    ttl_clock_ms: u64,
) -> Result<Vec<BindingRow>, ReadError> {
    let candidates =
        node_candidates(graph, &pattern.start, plan, meter).map_err(ReadError::Graph)?;
    let mut rows = Vec::new();
    for start in candidates {
        if pattern.start.anchor.is_none() {
            meter.charge().map_err(ReadError::Graph)?;
        }
        meter.visit(start.id.0).map_err(ReadError::Graph)?;
        if graph.node_expired(start.id, ttl_clock_ms)
            || !matches_node(&pattern.start, start, plan, meter).map_err(ReadError::Graph)?
        {
            continue;
        }
        let Some((relationship, end)) = &pattern.relationship else {
            prepare_binding(&mut rows, plan, meter, || {
                let mut row = BindingRow::new();
                if let Some(variable) = &pattern.start.variable {
                    row.insert(variable.text.clone(), BoundValue::Node(start.id.0));
                }
                row
            })
            .map_err(ReadError::Graph)?;
            continue;
        };
        let edges = edge_candidates(graph, relationship, plan, meter).map_err(ReadError::Graph)?;
        let maximum = relationship.max_hops.unwrap_or(1) as usize;
        let mut frontier = Vec::<(Vec<u64>, Vec<u64>)>::new();
        push_frontier(&mut frontier, &[], &[], start.id.0, None, meter)
            .map_err(ReadError::Graph)?;
        while let Some((node_ids, edge_ids)) = frontier.pop() {
            meter.charge().map_err(ReadError::Graph)?;
            let current = *node_ids.last().expect("frontier includes its start node");
            if !edge_ids.is_empty()
                && let Some(endpoint) = graph.node(NodeId(current))
                && (end.variable.as_ref().is_none_or(|name| {
                    pattern
                        .start
                        .variable
                        .as_ref()
                        .is_none_or(|start_name| start_name.text != name.text)
                        || current == start.id.0
                }))
                && matches_node(end, endpoint, plan, meter).map_err(ReadError::Graph)?
            {
                prepare_binding(&mut rows, plan, meter, || {
                    let mut row = BindingRow::new();
                    if let Some(variable) = &pattern.start.variable {
                        row.insert(variable.text.clone(), BoundValue::Node(start.id.0));
                    }
                    if let Some(variable) = &end.variable {
                        row.insert(variable.text.clone(), BoundValue::Node(current));
                    }
                    if let Some(variable) = &relationship.variable {
                        row.insert(variable.text.clone(), BoundValue::Edge(edge_ids[0]));
                    }
                    if let Some(variable) = &pattern.path_variable {
                        row.insert(
                            variable.text.clone(),
                            BoundValue::Path {
                                nodes: node_ids.clone(),
                                edges: edge_ids.clone(),
                            },
                        );
                    }
                    row
                })
                .map_err(ReadError::Graph)?;
            }
            if edge_ids.len() >= maximum {
                continue;
            }
            let mut degree = 0u64;
            for edge in edges.iter() {
                for destination in destinations(edge, current, relationship.direction) {
                    degree = degree.saturating_add(1);
                    meter.examine(degree).map_err(ReadError::Graph)?;
                    let Some(properties) =
                        active_edge_properties(graph, edge, as_of_ms, ttl_clock_ms, meter)
                            .map_err(ReadError::Graph)?
                    else {
                        continue;
                    };
                    if edge_ids.contains(&edge.id.0)
                        || !matches_relationship(relationship, edge, plan, properties, meter)
                            .map_err(ReadError::Graph)?
                    {
                        continue;
                    }
                    meter.visit(destination).map_err(ReadError::Graph)?;
                    push_frontier(
                        &mut frontier,
                        &node_ids,
                        &edge_ids,
                        destination,
                        Some(edge.id.0),
                        meter,
                    )
                    .map_err(ReadError::Graph)?;
                }
            }
        }
    }
    Ok(rows)
}

fn destinations(edge: &GraphEdge, current: u64, direction: Direction) -> impl Iterator<Item = u64> {
    let mut destinations = [None, None];
    if matches!(direction, Direction::Out | Direction::Both) && edge.source.0 == current {
        destinations[0] = Some(edge.destination.0);
    }
    if matches!(direction, Direction::In | Direction::Both)
        && edge.destination.0 == current
        && (edge.source.0 != current || direction == Direction::In)
    {
        destinations[1] = Some(edge.source.0);
    }
    destinations.into_iter().flatten()
}

fn push_frontier(
    frontier: &mut Vec<(Vec<u64>, Vec<u64>)>,
    node_ids: &[u64],
    edge_ids: &[u64],
    next_node: u64,
    next_edge: Option<u64>,
    meter: &mut Meter<'_, impl MeterClock>,
) -> Result<(), GraphError> {
    meter.frontier_insert(frontier.len() + 1)?;
    let bytes = 8usize
        .saturating_mul(node_ids.len() + edge_ids.len() + 1 + usize::from(next_edge.is_some()));
    meter.allocate(bytes as u64)?;
    let mut node_ids = node_ids.to_vec();
    let mut edge_ids = edge_ids.to_vec();
    node_ids.push(next_node);
    if let Some(edge_id) = next_edge {
        edge_ids.push(edge_id);
    }
    frontier.push((node_ids, edge_ids));
    Ok(())
}

fn prepare_binding(
    rows: &mut Vec<BindingRow>,
    plan: &Plan,
    meter: &mut Meter<'_, impl MeterClock>,
    build: impl FnOnce() -> BindingRow,
) -> Result<(), GraphError> {
    meter.binding_insert(rows.len() + 1)?;
    meter.allocate(8 * statement_bindings_len(&plan.statement) as u64)?;
    let row = build();
    rows.push(row);
    Ok(())
}

fn statement_bindings_len(statement: &Statement) -> usize {
    let Some(pattern) = &statement.pattern else {
        return 0;
    };
    let mut names = BTreeSet::new();
    if let Some(name) = &pattern.start.variable {
        names.insert(name.text.as_str());
    }
    if let Some((relationship, end)) = &pattern.relationship {
        if let Some(name) = &relationship.variable {
            names.insert(name.text.as_str());
        }
        if let Some(name) = &end.variable {
            names.insert(name.text.as_str());
        }
    }
    if let Some(name) = &pattern.path_variable {
        names.insert(name.text.as_str());
    }
    names.len()
}

fn matches_node(
    pattern: &NodePattern,
    node: &GraphNode,
    plan: &Plan,
    meter: &mut Meter<'_, impl MeterClock>,
) -> Result<bool, GraphError> {
    if let Some(anchor) = &pattern.anchor {
        let value = plan
            .parameters
            .get(&anchor.parameter.name)
            .expect("validated anchor parameter");
        let matches = match (anchor.kind, value) {
            (AnchorKind::Key, PropertyValue::String(key)) => node.key == *key,
            (AnchorKind::Id, PropertyValue::Integer(id)) => {
                u64::try_from(*id).is_ok_and(|id| id == node.id.0)
            }
            _ => false,
        };
        if !matches {
            return Ok(false);
        }
    }
    if !pattern
        .labels
        .iter()
        .all(|label| node.labels.contains(&label.text))
    {
        return Ok(false);
    }
    let mut matched = true;
    for property in &pattern.properties {
        meter.charge()?;
        let actual = node.properties.get(&property.name.text);
        let expected = plan
            .parameters
            .get(&property.parameter.name)
            .expect("validated parameter");
        matched &= actual.is_some_and(|actual| super::eval::typed_equal(actual, expected));
    }
    Ok(matched)
}

#[allow(clippy::too_many_arguments)]
fn matches_relationship(
    pattern: &RelationshipPattern,
    edge: &GraphEdge,
    plan: &Plan,
    properties: &Properties,
    meter: &mut Meter<'_, impl MeterClock>,
) -> Result<bool, GraphError> {
    if !pattern.types.is_empty()
        && !pattern
            .types
            .iter()
            .any(|relationship_type| relationship_type.text == edge.relationship)
    {
        return Ok(false);
    }
    let mut matched = true;
    for property in &pattern.properties {
        meter.charge()?;
        let actual = properties.get(&property.name.text);
        let expected = plan
            .parameters
            .get(&property.parameter.name)
            .expect("validated parameter");
        matched &= actual.is_some_and(|actual| super::eval::typed_equal(actual, expected));
    }
    Ok(matched)
}

fn canonical_sort(
    mut rows: Vec<BindingRow>,
    plan: &Plan,
    meter: &mut Meter<'_, impl MeterClock>,
) -> Result<Vec<BindingRow>, GraphError> {
    if plan
        .statement
        .pattern
        .as_ref()
        .is_some_and(|pattern| pattern.relationship.is_some())
    {
        meter.check("sort")?;
        meter.allocate(8 * rows.len() as u64)?;
        let failure = RefCell::new(None);
        rows.sort_by(|left, right| {
            if failure.borrow().is_some() {
                return Ordering::Equal;
            }
            if let Err(error) = meter.charge().and_then(|()| meter.check("sort")) {
                *failure.borrow_mut() = Some(error);
                return Ordering::Equal;
            }
            canonical_identity(left, plan).cmp(&canonical_identity(right, plan))
        });
        if let Some(error) = failure.into_inner() {
            return Err(error);
        }
    }
    Ok(rows)
}

type ProjectedRow = (Vec<CanonicalValue>, Vec<OutputValue>);

fn project_rows(
    graph: &Graph,
    rows: Vec<BindingRow>,
    returning: &super::ReturnClause,
    plan: &Plan,
    meter: &mut Meter<'_, impl MeterClock>,
    as_of_ms: Option<i64>,
    ttl_clock_ms: u64,
) -> Result<Vec<ProjectedRow>, ReadError> {
    let mut projected = Vec::with_capacity(rows.len());
    for row in rows {
        meter.charge().map_err(ReadError::Graph)?;
        meter
            .allocate(8 * (1 + returning.items.len()) as u64)
            .map_err(ReadError::Graph)?;
        let mut cells = Vec::with_capacity(returning.items.len());
        for item in &returning.items {
            let (variable, field) = match &item.value {
                ReturnValue::Variable(variable) => (&variable.text, None),
                ReturnValue::Field(field) => (&field.variable.text, Some(&field.field.text)),
            };
            let value = row.get(variable).expect("validated return binding");
            if let Some(field) = field {
                let properties =
                    super::eval::bound_properties(&row, variable, graph, as_of_ms, ttl_clock_ms)
                        .map_err(ReadError::Graph)?;
                let value = properties
                    .and_then(|properties| properties.get(field))
                    .cloned();
                meter
                    .allocate(value.as_ref().map_or(1, property_value_size))
                    .map_err(ReadError::Graph)?;
                cells.push(OutputValue::Scalar(value));
            } else {
                match value {
                    BoundValue::Node(id) => {
                        let node = graph
                            .node(NodeId(*id))
                            .expect("validated bound node")
                            .clone();
                        meter
                            .allocate(node_result_size(&node))
                            .map_err(ReadError::Graph)?;
                        cells.push(OutputValue::Node(node));
                    }
                    BoundValue::Edge(id) => {
                        let edge = graph
                            .edge(EdgeId(*id))
                            .expect("validated bound edge")
                            .clone();
                        let properties = active_edge_properties(
                            graph,
                            graph.edge(EdgeId(*id)).expect("bound edge"),
                            as_of_ms,
                            ttl_clock_ms,
                            meter,
                        )
                        .map_err(ReadError::Graph)?
                        .expect("edge stayed visible in one read view")
                        .clone();
                        meter
                            .allocate(edge_result_size(&edge, &properties))
                            .map_err(ReadError::Graph)?;
                        cells.push(OutputValue::Edge { edge, properties });
                    }
                    BoundValue::Path { nodes, edges } => {
                        meter
                            .allocate(8 * (1 + edges.len() + nodes.len()) as u64)
                            .map_err(ReadError::Graph)?;
                        cells.push(OutputValue::Path {
                            nodes: nodes.clone(),
                            edges: edges.clone(),
                        });
                    }
                }
            }
        }
        projected.push((canonical_identity(&row, plan), cells));
    }
    Ok(projected)
}

fn property_value_size(value: &PropertyValue) -> u64 {
    match value {
        PropertyValue::String(value) => (5 + value.len()) as u64,
        PropertyValue::Integer(_) | PropertyValue::Float(_) => 9,
        PropertyValue::Boolean(_) => 2,
        PropertyValue::GeoPoint { .. } => 17,
    }
}

fn node_result_size(node: &GraphNode) -> u64 {
    let key = 4 + node.key.len();
    let labels = 4 + node
        .labels
        .iter()
        .map(|label| 4 + label.len())
        .sum::<usize>();
    (16 + key + labels + properties_size(&node.properties)) as u64
}

fn edge_result_size(edge: &GraphEdge, properties: &Properties) -> u64 {
    let key = edge.key.as_ref().map_or(1, |key| 4 + key.len());
    let relationship = 16 + 4 + edge.relationship.len();
    (16 + key + relationship + properties_size(properties)) as u64
}

fn properties_size(properties: &Properties) -> usize {
    4 + properties
        .iter()
        .map(|(name, value)| 2 + name.len() + property_value_size(value) as usize)
        .sum::<usize>()
}

fn scalar_sort(
    mut projected: Vec<ProjectedRow>,
    returning: &super::ReturnClause,
    _plan: &Plan,
    meter: &mut Meter<'_, impl MeterClock>,
) -> Result<Vec<ProjectedRow>, GraphError> {
    if returning.order_by.is_empty() {
        return Ok(projected);
    }
    meter.check("sort")?;
    meter.allocate(8 * projected.len() as u64)?;
    let columns = returning
        .items
        .iter()
        .map(|item| {
            item.alias.as_ref().map_or_else(
                || match &item.value {
                    ReturnValue::Variable(variable) => variable.text.clone(),
                    ReturnValue::Field(field) => {
                        format!("{}.{}", field.variable.text, field.field.text)
                    }
                },
                |alias| alias.text.clone(),
            )
        })
        .collect::<Vec<_>>();
    let failure = RefCell::new(None);
    projected.sort_by(|left, right| {
        if failure.borrow().is_some() {
            return Ordering::Equal;
        }
        if let Err(error) = meter.charge().and_then(|()| meter.check("sort")) {
            *failure.borrow_mut() = Some(error);
            return Ordering::Equal;
        }
        for item in &returning.order_by {
            let Some(index) = columns.iter().position(|name| name == &item.name.text) else {
                continue;
            };
            let left_value = scalar_value(&left.1[index]);
            let right_value = scalar_value(&right.1[index]);
            let ordering = compare_sort_values(&left_value, &right_value, item.direction);
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        left.0.cmp(&right.0)
    });
    if let Some(error) = failure.into_inner() {
        return Err(error);
    }
    Ok(projected)
}

fn scalar_value(value: &OutputValue) -> Option<PropertyValue> {
    match value {
        OutputValue::Scalar(value) => value.clone(),
        _ => None,
    }
}

fn check_output(
    rows: &[ProjectedRow],
    meter: &mut Meter<'_, impl MeterClock>,
) -> Result<(), GraphError> {
    let mut nodes = BTreeSet::new();
    let mut edges = BTreeSet::new();
    let mut paths = 0usize;
    for (_, cells) in rows {
        for cell in cells {
            match cell {
                OutputValue::Node(node) => {
                    nodes.insert(node.id.0);
                }
                OutputValue::Edge { edge, .. } => {
                    edges.insert(edge.id.0);
                }
                OutputValue::Path {
                    nodes: path_nodes,
                    edges: path_edges,
                } => {
                    paths += 1;
                    nodes.extend(path_nodes);
                    edges.extend(path_edges);
                }
                OutputValue::Scalar(_) => {}
            }
        }
    }
    meter.output(nodes.len(), edges.len(), paths)
}

#[cfg(test)]
mod tests {
    use super::super::meter::MeterClock;
    use super::*;

    struct Fake {
        elapsed: u64,
        cancelled: bool,
    }

    impl MeterClock for Fake {
        fn elapsed_ms(&self, _phase: &'static str, _work: u64) -> u64 {
            self.elapsed
        }

        fn cancelled(&self, _phase: &'static str, _work: u64) -> bool {
            self.cancelled
        }
    }

    #[test]
    fn three_valued_logic_evaluates_both_sides() {
        assert_eq!(super::super::eval::and(None, Some(false)), Some(false));
        assert_eq!(super::super::eval::or(None, Some(true)), Some(true));
        assert_eq!(super::super::eval::and(None, Some(true)), None);
        assert_eq!(super::super::eval::or(None, Some(false)), None);
    }

    #[test]
    fn meter_clock_is_checked_before_read_work() {
        let fake = Fake {
            elapsed: ReadBounds::DEFAULT.graph.deadline_ms,
            cancelled: false,
        };
        assert!(matches!(
            Meter::new(ReadBounds::DEFAULT, &fake),
            Err(GraphError::InvalidQuery("subset deadline exceeded"))
        ));
    }
}
