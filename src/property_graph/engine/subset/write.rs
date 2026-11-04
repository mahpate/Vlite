//! Private write executor for `vlite-cypher-1` (Gap 6 `gap6-write-executor`).
//!
//! One statement plans one batch: MATCH bindings are materialized once at the
//! captured clock, every mutation clause is staged into one [`Planner`], and
//! the caller commits the returned body as one frame or nothing at all.

use super::super::codec::GraphBody;
use super::super::error::GraphError;
use super::super::language::LanguageError;
use super::super::model::{
    self, EdgeId, GraphEdge, GraphId, GraphNode, NodeId, Precondition, Properties, PropertyValue,
};
use super::super::mutate::{EdgeSelector, GraphMutation, MutationOutcome, Planner};
use super::super::properties;
use super::super::state::{Graph, GraphState};
use super::eval::{
    BindingRow, BoundValue, CanonicalValue, OutputValue, compare_sort_values, evaluate_predicate,
    typed_equal,
};
use super::meter::{Meter, MeterClock, ReadBounds};
use super::plan::{self, ParameterEntry};
use super::{
    Anchor, AnchorKind, Assignment, Creation, CreationEdge, CreationNode, Direction, Mutation,
    Name, NodePattern, ParameterRef, Plan, PropertyPair, RelationshipPattern, Removal,
    ReturnClause, ReturnValue,
};
use std::cmp::Ordering;
use std::collections::BTreeSet;

/// DO-003: at most 4096 counted entries and 16 MiB of encoded body.
const MAX_OPERATIONS: usize = 4096;
const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;
/// Body header (4) and one opening reclaim entry (8 envelope + u64 clock).
const BODY_HEADER_BYTES: u64 = 4;
const RECLAIM_BYTES: u64 = 16;
const DELETED: GraphError = GraphError::InvalidQuery("subset binding was deleted");
const TEMPORAL: GraphError = GraphError::InvalidQuery("subset temporal mutation unsupported");

/// Statement accounting reported with every staged write.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WriteCounters {
    pub(crate) operations: usize,
    pub(crate) body_bytes: u64,
    pub(crate) reclaim_entries: usize,
    pub(crate) binding_rows: usize,
    pub(crate) detach_edges_examined: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WriteResult {
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<Vec<OutputValue>>,
    pub(crate) as_of_ms: Option<i64>,
    pub(crate) ttl_clock_ms: u64,
    pub(crate) view_sequence: u64,
    pub(crate) revision: Option<u64>,
    pub(crate) counters: WriteCounters,
}

/// A planned statement. `body` holds the reclaim entry (if any) and one entry
/// per staged operation; `expected` lists the precondition of each counted
/// entry in body order (`None` for creations). Commit `body` only when
/// `result.revision` is set.
#[derive(Debug)]
pub(crate) struct StagedWrite {
    pub(crate) body: GraphBody,
    pub(crate) expected: Vec<Option<Precondition>>,
    pub(crate) result: WriteResult,
}

#[derive(Debug)]
pub(crate) enum WriteError {
    Query(LanguageError),
    /// Envelope-level parameter metadata rejected before graph access.
    Metadata(GraphError),
    Graph(GraphError),
    SequenceExhausted,
}

impl From<GraphError> for WriteError {
    fn from(error: GraphError) -> Self {
        Self::Graph(error)
    }
}

/// Plans one write statement against `state` at `ttl_clock_ms` (the clock C
/// captured once by the caller). Returns `None` for a statement without
/// mutations. Planning never changes `state`: the planner's tentative
/// changes are rolled back before this returns.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute<C: MeterClock>(
    state: &mut GraphState,
    graph_id: GraphId,
    view_sequence: u64,
    temporal_capable: bool,
    ttl_clock_ms: u64,
    text: &str,
    parameters: &[ParameterEntry],
    bounds: ReadBounds,
    clock: &C,
) -> Result<Option<StagedWrite>, WriteError> {
    let mut bounds = bounds.validate()?;
    let planned = plan::plan(text, parameters).map_err(|error| match error {
        plan::PlanError::Query(error) => WriteError::Query(error),
        plan::PlanError::InvalidMetadata(error) => WriteError::Metadata(error),
    })?;
    if planned.statement.mutations.is_empty() {
        return Ok(None);
    }
    let relationship = planned
        .statement
        .pattern
        .as_ref()
        .is_some_and(|pattern| pattern.relationship.is_some());
    if relationship && bounds.graph.max_hops < 1 {
        return Err(GraphError::InvalidQuery("subset hop bound exceeds max_hops").into());
    }
    validate_keys(&planned)?;
    // Writes run under the write deadline, not the read deadline.
    bounds.graph.deadline_ms = bounds.write_deadline_ms;
    let mut meter = Meter::new(bounds, clock)?;
    let as_of_ms = if temporal_capable {
        Some(i64::try_from(ttl_clock_ms).map_err(|_| {
            GraphError::InvalidQuery("the captured clock is not a temporal instant")
        })?)
    } else {
        None
    };
    let graph = state
        .graph(graph_id)
        .ok_or(GraphError::GraphNotFound(graph_id))?;
    let rows = materialize(graph, &planned, &mut meter, as_of_ms, ttl_clock_ms)?;
    let sequence = view_sequence
        .checked_add(1)
        .ok_or(WriteError::SequenceExhausted)?;
    let mut planner = state.planner(sequence, graph_id)?;
    planner.reclaim_expired(ttl_clock_ms);
    let reclaim_entries = planner.len();
    let mut transaction = Transaction {
        planner,
        plan: &planned,
        meter: &mut meter,
        as_of_ms,
        bytes: BODY_HEADER_BYTES + RECLAIM_BYTES * reclaim_entries as u64,
        expected: Vec::new(),
        touched: BTreeSet::new(),
        detach_edges_examined: 0,
        rows,
    };
    for mutation in &planned.statement.mutations {
        transaction.apply(mutation)?;
    }
    let (columns, rows) = match &planned.statement.returning {
        Some(returning) => transaction.project(returning)?,
        None => (Vec::new(), Vec::new()),
    };
    transaction.meter.check("finish")?;
    let operations = transaction.expected.len();
    let counters = WriteCounters {
        operations,
        body_bytes: if operations > 0 { transaction.bytes } else { 0 },
        reclaim_entries: if operations > 0 { reclaim_entries } else { 0 },
        binding_rows: transaction.rows.len(),
        detach_edges_examined: transaction.detach_edges_examined,
    };
    let expected = std::mem::take(&mut transaction.expected);
    let body = transaction.planner.finish();
    Ok(Some(StagedWrite {
        body,
        expected,
        result: WriteResult {
            columns,
            rows,
            as_of_ms,
            ttl_clock_ms,
            view_sequence,
            revision: (operations > 0).then_some(sequence),
            counters,
        },
    }))
}

/// Every KEY reference names a 1..=256-byte string before any graph access.
fn validate_keys(plan: &Plan) -> Result<(), GraphError> {
    let mut references = Vec::new();
    if let Some(pattern) = &plan.statement.pattern {
        let mut anchors = vec![&pattern.start.anchor];
        if let Some((relationship, end)) = &pattern.relationship {
            anchors.extend([&relationship.anchor, &end.anchor]);
        }
        references.extend(
            anchors
                .into_iter()
                .flatten()
                .filter(|anchor| anchor.kind == AnchorKind::Key)
                .map(|anchor| &anchor.parameter),
        );
    }
    for mutation in &plan.statement.mutations {
        let creations: &[Creation] = match mutation {
            Mutation::Create { creations, .. } => creations,
            Mutation::Merge { creation, .. } => std::slice::from_ref(creation),
            _ => &[],
        };
        for creation in creations {
            match creation {
                Creation::Node(node) => references.push(&node.key),
                Creation::Edge(edge) => references.extend(edge.key.iter()),
            }
        }
    }
    for reference in references {
        match plan.parameters.get(&reference.name) {
            Some(PropertyValue::String(key)) => model::validate_node_key(key)?,
            _ => return Err(GraphError::InvalidNodeKey),
        }
    }
    Ok(())
}

fn parameter<'a>(plan: &'a Plan, reference: &ParameterRef) -> &'a PropertyValue {
    plan.parameters
        .get(&reference.name)
        .expect("validated parameter")
}

fn string_parameter<'a>(plan: &'a Plan, reference: &ParameterRef) -> &'a str {
    match parameter(plan, reference) {
        PropertyValue::String(value) => value,
        _ => unreachable!("validated KEY parameter"),
    }
}

/// What one pattern element is compared against.
struct Candidate<'a> {
    key: Option<&'a str>,
    id: u64,
    labels: Option<&'a BTreeSet<String>>,
    relationship: Option<&'a str>,
    properties: &'a Properties,
}

/// Whether the anchor, labels or types and properties match; each property
/// costs one work unit and the first mismatch stops the comparison.
fn matches(
    anchor: Option<&Anchor>,
    labels: &[Name],
    pairs: &[PropertyPair],
    candidate: Candidate<'_>,
    plan: &Plan,
    meter: &mut Meter<'_, impl MeterClock>,
) -> Result<bool, GraphError> {
    if let Some(anchor) = anchor {
        let matched = match (anchor.kind, parameter(plan, &anchor.parameter)) {
            (AnchorKind::Key, PropertyValue::String(wanted)) => {
                candidate.key == Some(wanted.as_str())
            }
            (AnchorKind::Id, PropertyValue::Integer(wanted)) => {
                u64::try_from(*wanted).is_ok_and(|wanted| wanted == candidate.id)
            }
            _ => false,
        };
        if !matched {
            return Ok(false);
        }
    }
    let named = match (candidate.labels, candidate.relationship) {
        (Some(present), _) => labels.iter().all(|label| present.contains(&label.text)),
        (None, Some(relationship)) => {
            labels.is_empty() || labels.iter().any(|name| name.text == relationship)
        }
        (None, None) => true,
    };
    if !named {
        return Ok(false);
    }
    for pair in pairs {
        meter.charge()?;
        let wanted = parameter(plan, &pair.parameter);
        if !candidate
            .properties
            .get(&pair.name.text)
            .is_some_and(|actual| typed_equal(actual, wanted))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn matches_node(
    pattern: &NodePattern,
    node: &GraphNode,
    plan: &Plan,
    meter: &mut Meter<'_, impl MeterClock>,
) -> Result<bool, GraphError> {
    let candidate = Candidate {
        key: Some(&node.key),
        id: node.id.0,
        labels: Some(&node.labels),
        relationship: None,
        properties: &node.properties,
    };
    matches(
        pattern.anchor.as_ref(),
        &pattern.labels,
        &pattern.properties,
        candidate,
        plan,
        meter,
    )
}

fn matches_edge(
    pattern: &RelationshipPattern,
    edge: &GraphEdge,
    properties: &Properties,
    plan: &Plan,
    meter: &mut Meter<'_, impl MeterClock>,
) -> Result<bool, GraphError> {
    let candidate = Candidate {
        key: edge.key.as_deref(),
        id: edge.id.0,
        labels: None,
        relationship: Some(&edge.relationship),
        properties,
    };
    matches(
        pattern.anchor.as_ref(),
        &pattern.types,
        &pattern.properties,
        candidate,
        plan,
        meter,
    )
}

/// The properties an edge shows at the captured clock: a temporal edge shows
/// the segment valid at it, or nothing.
fn edge_properties<'a>(
    graph: &'a Graph,
    edge: &'a GraphEdge,
    as_of_ms: Option<i64>,
) -> Option<&'a Properties> {
    match graph.segments(edge.id) {
        Some(segments) => as_of_ms
            .and_then(|instant| super::super::temporal::segment_at(segments, instant))
            .map(|segment| &segment.properties),
        None => Some(&edge.properties),
    }
}

/// Incident edges of `node` in ascending ID order; a self-loop appears once.
fn incident(graph: &Graph, node: u64, direction: Direction) -> Vec<EdgeId> {
    let direction = match direction {
        Direction::Out => model::Direction::Out,
        Direction::In => model::Direction::In,
        Direction::Both => model::Direction::Both,
    };
    let mut edges = graph
        .adjacency(NodeId(node), direction, None, None)
        .map(|(_, id)| id)
        .collect::<Vec<_>>();
    edges.sort_unstable();
    edges
}

/// The row's canonical identity over every bound name in byte order.
fn identity(row: &BindingRow) -> Vec<CanonicalValue> {
    row.values()
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

fn precharge_sort(count: usize, meter: &mut Meter<'_, impl MeterClock>) -> Result<(), GraphError> {
    // The same deterministic comparison bound as the read executor.
    let comparisons = match count {
        0 | 1 => 0,
        2 => 1,
        _ => (count as u64).saturating_mul(u64::from(usize::BITS - (count - 1).leading_zeros())),
    };
    for _ in 0..comparisons {
        meter.charge()?;
        meter.check("sort")?;
    }
    Ok(())
}

/// MATCH and WHERE, once, against the graph before any mutation and at the
/// captured clock.
fn materialize(
    graph: &Graph,
    plan: &Plan,
    meter: &mut Meter<'_, impl MeterClock>,
    as_of_ms: Option<i64>,
    ttl_clock_ms: u64,
) -> Result<Vec<BindingRow>, GraphError> {
    let Some(pattern) = &plan.statement.pattern else {
        meter.binding_insert(1)?;
        return Ok(vec![BindingRow::new()]);
    };
    let row_bytes = 8 * (1 + 3 * u64::from(pattern.relationship.is_some()));
    let start = &pattern.start;
    let starts: Vec<&GraphNode> = match &start.anchor {
        None => graph.nodes().collect(),
        Some(anchor) => {
            meter.charge()?;
            let node = match (anchor.kind, parameter(plan, &anchor.parameter)) {
                (AnchorKind::Key, PropertyValue::String(key)) => graph.node_by_key(key),
                (AnchorKind::Id, PropertyValue::Integer(id)) => u64::try_from(*id)
                    .ok()
                    .and_then(|id| graph.node(NodeId(id))),
                _ => None,
            };
            node.into_iter().collect()
        }
    };
    let mut rows = Vec::new();
    for node in starts {
        if start.anchor.is_none() {
            meter.examine(0)?;
        }
        meter.visit(node.id.0)?;
        if graph.node_expired(node.id, ttl_clock_ms) || !matches_node(start, node, plan, meter)? {
            continue;
        }
        let Some((relationship, end)) = &pattern.relationship else {
            meter.binding_insert(rows.len() + 1)?;
            meter.allocate(row_bytes)?;
            let mut row = BindingRow::new();
            if let Some(variable) = &start.variable {
                row.insert(variable.text.clone(), BoundValue::Node(node.id.0));
            }
            rows.push(row);
            continue;
        };
        if relationship.anchor.is_some() {
            meter.charge()?;
        }
        let same_variable = matches!(
            (&start.variable, &end.variable),
            (Some(first), Some(second)) if first.text == second.text
        );
        let mut degree = 0;
        for edge_id in incident(graph, node.id.0, relationship.direction) {
            let edge = graph.edge(edge_id).expect("adjacency edge");
            let destination = if edge.source == node.id && relationship.direction != Direction::In {
                edge.destination
            } else {
                edge.source
            };
            degree += 1;
            meter.examine(degree)?;
            if graph.edge_expired(edge, ttl_clock_ms) {
                continue;
            }
            let Some(properties) = edge_properties(graph, edge, as_of_ms) else {
                continue;
            };
            let target = graph.node(destination).expect("live endpoint");
            if !matches_edge(relationship, edge, properties, plan, meter)?
                || !matches_node(end, target, plan, meter)?
                || (same_variable && destination != node.id)
            {
                continue;
            }
            meter.visit(destination.0)?;
            meter.binding_insert(rows.len() + 1)?;
            meter.allocate(row_bytes)?;
            let mut row = BindingRow::new();
            if let Some(variable) = &start.variable {
                row.insert(variable.text.clone(), BoundValue::Node(node.id.0));
            }
            if let Some(variable) = &end.variable {
                row.insert(variable.text.clone(), BoundValue::Node(destination.0));
            }
            if let Some(variable) = &relationship.variable {
                row.insert(variable.text.clone(), BoundValue::Edge(edge.id.0));
            }
            if let Some(variable) = &pattern.path_variable {
                row.insert(
                    variable.text.clone(),
                    BoundValue::Path {
                        nodes: vec![node.id.0, destination.0],
                        edges: vec![edge.id.0],
                    },
                );
            }
            rows.push(row);
        }
    }
    if pattern.relationship.is_some() {
        meter.check("sort")?;
        meter.allocate(8 * rows.len() as u64)?;
        precharge_sort(rows.len(), meter)?;
        rows.sort_by_cached_key(identity);
        meter.check("sort")?;
    }
    if let Some(predicate) = &plan.statement.predicate {
        let mut filtered = Vec::with_capacity(rows.len());
        for row in rows {
            if evaluate_predicate(predicate, &row, plan, meter, graph, as_of_ms, ttl_clock_ms)?
                == Some(true)
            {
                filtered.push(row);
            }
        }
        rows = filtered;
    }
    Ok(rows)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Node,
    Edge,
}

/// One item of a SET or REMOVE clause.
enum Target<'a> {
    Set(&'a str, &'a ParameterRef),
    Unset(&'a str),
    AddLabel(&'a str),
    RemoveLabel(&'a str),
}

/// One pending SET/REMOVE operation of one entity.
struct Patch {
    kind: Kind,
    id: u64,
    set: Properties,
    unset: BTreeSet<String>,
    add_labels: BTreeSet<String>,
    remove_labels: BTreeSet<String>,
}

/// `1 + Σ(1 + len)`, after the label-set rules.
fn labels_size(labels: &BTreeSet<String>) -> Result<u64, GraphError> {
    model::validate_labels(labels)?;
    Ok(1 + labels
        .iter()
        .map(|label| 1 + label.len() as u64)
        .sum::<u64>())
}

fn unset_size(unset: &BTreeSet<String>) -> u64 {
    1 + unset.iter().map(|name| 2 + name.len() as u64).sum::<u64>()
}

struct Transaction<'s, 'p, 'm, C> {
    planner: Planner<'s>,
    plan: &'p Plan,
    meter: &'p mut Meter<'m, C>,
    as_of_ms: Option<i64>,
    /// Encoded body bytes: header, reclaim and every staged entry.
    bytes: u64,
    /// One precondition per staged operation (`None` for creations).
    expected: Vec<Option<Precondition>>,
    touched: BTreeSet<(Kind, u64)>,
    detach_edges_examined: u64,
    rows: Vec<BindingRow>,
}

impl<C: MeterClock> Transaction<'_, '_, '_, C> {
    fn graph(&self) -> &Graph {
        self.planner.graph()
    }

    fn apply(&mut self, mutation: &Mutation) -> Result<(), WriteError> {
        match mutation {
            Mutation::Create { creations, .. } => self.create(creations, false),
            Mutation::Merge { creation, .. } => self.create(std::slice::from_ref(creation), true),
            Mutation::Set { assignments, .. } => {
                let targets = assignments
                    .iter()
                    .map(|assignment| match assignment {
                        Assignment::Property { target, parameter } => {
                            (&target.variable, Target::Set(&target.field.text, parameter))
                        }
                        Assignment::Label { variable, label } => {
                            (variable, Target::AddLabel(&label.text))
                        }
                    })
                    .collect::<Vec<_>>();
                self.patch(&targets)
            }
            Mutation::Remove { removals, .. } => {
                let targets = removals
                    .iter()
                    .map(|removal| match removal {
                        Removal::Property(target) => {
                            (&target.variable, Target::Unset(&target.field.text))
                        }
                        Removal::Label { variable, label } => {
                            (variable, Target::RemoveLabel(&label.text))
                        }
                    })
                    .collect::<Vec<_>>();
                self.patch(&targets)
            }
            Mutation::Delete {
                variables, detach, ..
            } => self.delete(variables, *detach),
        }
    }

    /// The live entity a binding names; a deleted one is an error.
    fn resolve(&self, row: &BindingRow, name: &str) -> Result<(Kind, u64), GraphError> {
        let graph = self.graph();
        match row.get(name).expect("validated binding") {
            BoundValue::Node(id) if graph.node(NodeId(*id)).is_some() => Ok((Kind::Node, *id)),
            BoundValue::Edge(id) if graph.edge(EdgeId(*id)).is_some() => Ok((Kind::Edge, *id)),
            _ => Err(DELETED),
        }
    }

    /// The precondition of the first operation on an entity in this batch;
    /// later ones accept any revision.
    fn precondition(&mut self, kind: Kind, id: u64) -> Precondition {
        if !self.touched.insert((kind, id)) {
            return Precondition::Any;
        }
        let graph = self.graph();
        Precondition::Revision(match kind {
            Kind::Node => graph.node(NodeId(id)).expect("live node").revision,
            Kind::Edge => graph.edge(EdgeId(id)).expect("live edge").revision,
        })
    }

    /// Charges and counts one operation, then plans it.
    fn stage(
        &mut self,
        size: impl FnOnce() -> Result<u64, GraphError>,
        mutation: GraphMutation,
        expected: Option<Precondition>,
    ) -> Result<MutationOutcome, GraphError> {
        self.meter.charge()?;
        if self.expected.len() >= MAX_OPERATIONS {
            return Err(GraphError::BatchSize("more than 4096 entries"));
        }
        let size = size()?;
        self.bytes += size;
        if self.bytes > MAX_BODY_BYTES {
            return Err(GraphError::BatchSize("graph body exceeds 16 MiB"));
        }
        self.meter.allocate(size)?;
        let outcome = self.planner.mutate(&mutation)?;
        self.expected.push(expected);
        Ok(outcome)
    }

    fn properties(&self, pairs: &[PropertyPair]) -> Result<(Properties, u64), GraphError> {
        let properties = pairs
            .iter()
            .map(|pair| {
                (
                    pair.name.text.clone(),
                    parameter(self.plan, &pair.parameter).clone(),
                )
            })
            .collect::<Properties>();
        let size = properties::validate(&properties)? as u64;
        Ok((properties, size))
    }

    /// CREATE or MERGE: row-major, each row gaining the bound variables.
    fn create(&mut self, creations: &[Creation], merge: bool) -> Result<(), WriteError> {
        for index in 0..self.rows.len() {
            for creation in creations {
                let (variable, bound) = match creation {
                    Creation::Node(node) => (&node.variable, self.create_node(node, merge)?),
                    Creation::Edge(edge) => {
                        let row = self.rows[index].clone();
                        (&edge.variable, self.create_edge(edge, &row, merge)?)
                    }
                };
                self.rows[index].insert(variable.text.clone(), bound);
            }
        }
        Ok(())
    }

    fn create_node(&mut self, node: &CreationNode, merge: bool) -> Result<BoundValue, GraphError> {
        let key = string_parameter(self.plan, &node.key);
        model::validate_node_key(key)?;
        let (properties, properties_size) = self.properties(&node.properties)?;
        let labels = node
            .labels
            .iter()
            .map(|label| label.text.clone())
            .collect::<BTreeSet<_>>();
        let labels_bytes = labels_size(&labels)?;
        self.meter.charge()?;
        if let Some(existing) = self.graph().node_by_key(key) {
            if !merge {
                return Err(GraphError::NodeKeyExists(key.into()));
            }
            let fits = properties.iter().all(|(name, value)| {
                existing
                    .properties
                    .get(name)
                    .is_some_and(|current| typed_equal(value, current))
            }) && labels.is_subset(&existing.labels);
            if !fits {
                return Err(GraphError::EdgeKeyConflict(key.into()));
            }
            return Ok(BoundValue::Node(existing.id.0));
        }
        if self.graph().next_node_id() == u64::MAX {
            return Err(GraphError::IdSpaceExhausted);
        }
        let size = 8 + 8 + 2 + key.len() as u64 + labels_bytes + 4 + properties_size;
        let mutation = GraphMutation::CreateNode {
            key: key.into(),
            labels,
            properties,
        };
        let MutationOutcome::Node { id, .. } = self.stage(|| Ok(size), mutation, None)? else {
            unreachable!("CreateNode plans a node")
        };
        self.touched.insert((Kind::Node, id.0));
        Ok(BoundValue::Node(id.0))
    }

    fn create_edge(
        &mut self,
        edge: &CreationEdge,
        row: &BindingRow,
        merge: bool,
    ) -> Result<BoundValue, GraphError> {
        let key = edge
            .key
            .as_ref()
            .map(|reference| string_parameter(self.plan, reference));
        if let Some(key) = key {
            model::validate_edge_key(key)?;
        }
        let (properties, properties_size) = self.properties(&edge.properties)?;
        let (_, source) = self.resolve(row, &edge.source.text)?;
        let (_, destination) = self.resolve(row, &edge.destination.text)?;
        let relationship = &edge.relationship_type.text;
        model::validate_relationship_type(relationship)?;
        if let Some(key) = key {
            self.meter.charge()?;
            let graph = self.graph();
            if let Some(existing) = graph.edge_by_key(key) {
                if graph.segments(existing.id).is_some() {
                    return Err(if merge {
                        TEMPORAL
                    } else {
                        GraphError::EdgeKeyConflict(key.into())
                    });
                }
                let same = |value: &PropertyValue, current: &PropertyValue| {
                    if merge {
                        typed_equal(value, current)
                    } else {
                        bit_equal(value, current)
                    }
                };
                let fits = properties.iter().all(|(name, value)| {
                    existing
                        .properties
                        .get(name)
                        .is_some_and(|current| same(value, current))
                }) && (merge || existing.properties.len() == properties.len())
                    && existing.source.0 == source
                    && existing.destination.0 == destination
                    && existing.relationship == *relationship;
                if !fits {
                    return Err(GraphError::EdgeKeyConflict(key.into()));
                }
                return Ok(BoundValue::Edge(existing.id.0));
            }
        }
        let graph = self.graph();
        if graph.next_edge_id() == u64::MAX {
            return Err(GraphError::IdSpaceExhausted);
        }
        let node_key = |id: u64| graph.node(NodeId(id)).expect("live endpoint").key.clone();
        let mutation = GraphMutation::CreateEdge {
            key: key.map(str::to_owned),
            source: node_key(source),
            relationship: relationship.clone(),
            destination: node_key(destination),
            properties,
        };
        let size = 8
            + 24
            + 1
            + relationship.len() as u64
            + 2
            + key.map_or(0, |key| key.len() as u64)
            + 4
            + properties_size;
        let MutationOutcome::Edge { id, .. } = self.stage(|| Ok(size), mutation, None)? else {
            unreachable!("CreateEdge plans an edge")
        };
        self.touched.insert((Kind::Edge, id.0));
        Ok(BoundValue::Edge(id.0))
    }

    /// One SET or REMOVE clause: one operation per entity, in order of first
    /// appearance across rows and items.
    fn patch(&mut self, targets: &[(&Name, Target<'_>)]) -> Result<(), WriteError> {
        let mut pending: Vec<Patch> = Vec::new();
        for row in &self.rows {
            for (variable, target) in targets {
                let (kind, id) = self.resolve(row, &variable.text)?;
                if kind == Kind::Edge && self.graph().segments(EdgeId(id)).is_some() {
                    return Err(TEMPORAL.into());
                }
                let index = match pending
                    .iter()
                    .position(|patch| patch.kind == kind && patch.id == id)
                {
                    Some(index) => index,
                    None => {
                        pending.push(Patch {
                            kind,
                            id,
                            set: Properties::new(),
                            unset: BTreeSet::new(),
                            add_labels: BTreeSet::new(),
                            remove_labels: BTreeSet::new(),
                        });
                        pending.len() - 1
                    }
                };
                let patch = &mut pending[index];
                match target {
                    Target::Set(name, reference) => {
                        patch
                            .set
                            .insert((*name).to_owned(), parameter(self.plan, reference).clone());
                    }
                    Target::Unset(name) => {
                        patch.unset.insert((*name).to_owned());
                    }
                    Target::AddLabel(name) => {
                        patch.add_labels.insert((*name).to_owned());
                    }
                    Target::RemoveLabel(name) => {
                        patch.remove_labels.insert((*name).to_owned());
                    }
                }
            }
        }
        for patch in pending {
            self.stage_patch(patch)?;
        }
        Ok(())
    }

    fn stage_patch(&mut self, patch: Patch) -> Result<(), GraphError> {
        let expected = self.precondition(patch.kind, patch.id);
        let set_size = properties::validate(&patch.set)? as u64;
        let unset_bytes = unset_size(&patch.unset);
        let graph = self.graph();
        let (current, labels, key) = match patch.kind {
            Kind::Node => {
                let node = graph.node(NodeId(patch.id)).expect("live node");
                (&node.properties, Some(&node.labels), Some(node.key.clone()))
            }
            Kind::Edge => {
                let edge = graph.edge(EdgeId(patch.id)).expect("live edge");
                (&edge.properties, None, None)
            }
        };
        let mut merged = current.clone();
        merged.extend(patch.set.clone());
        merged.retain(|name, _| !patch.unset.contains(name));
        properties::validate(&merged)?;
        if let Some(labels) = labels {
            let merged = labels
                .union(&patch.add_labels)
                .filter(|label| !patch.remove_labels.contains(*label))
                .cloned()
                .collect::<BTreeSet<_>>();
            model::validate_labels(&merged)?;
        }
        match key {
            Some(key) => {
                let (add, remove) = (patch.add_labels.clone(), patch.remove_labels.clone());
                let size = || {
                    Ok(
                        16 + labels_size(&add)?
                            + labels_size(&remove)?
                            + 4
                            + set_size
                            + unset_bytes,
                    )
                };
                let mutation = GraphMutation::PatchNode {
                    key,
                    add_labels: patch.add_labels,
                    remove_labels: patch.remove_labels,
                    set: patch.set,
                    unset: patch.unset,
                    expected,
                };
                self.stage(size, mutation, Some(expected))?;
            }
            None => {
                let mutation = GraphMutation::PatchEdge {
                    edge: EdgeSelector::Id(EdgeId(patch.id)),
                    set: patch.set,
                    unset: patch.unset,
                    expected,
                };
                self.stage(
                    || Ok(16 + 4 + set_size + unset_bytes),
                    mutation,
                    Some(expected),
                )?;
            }
        }
        Ok(())
    }

    /// One DELETE clause: relationships in ascending ID order, then nodes.
    fn delete(&mut self, variables: &[Name], detach: bool) -> Result<(), WriteError> {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for row in &self.rows {
            for variable in variables {
                let (kind, id) = self.resolve(row, &variable.text)?;
                let targets = if kind == Kind::Node {
                    &mut nodes
                } else {
                    &mut edges
                };
                if !targets.contains(&id) {
                    targets.push(id);
                }
            }
        }
        if detach {
            // Bounded discovery before any deletion is staged.
            for &node in &nodes {
                let mut degree = 0;
                for edge in incident(self.graph(), node, Direction::Both) {
                    degree += 1;
                    self.meter.examine(degree)?;
                    self.detach_edges_examined += 1;
                    if !edges.contains(&edge.0) {
                        if self.expected.len() + nodes.len() + edges.len() + 1 > MAX_OPERATIONS {
                            return Err(GraphError::BatchSize("more than 4096 entries").into());
                        }
                        edges.push(edge.0);
                    }
                }
            }
        }
        edges.sort_unstable();
        nodes.sort_unstable();
        for id in edges {
            let expected = self.precondition(Kind::Edge, id);
            let mutation = GraphMutation::DeleteEdge {
                edge: EdgeSelector::Id(EdgeId(id)),
                expected,
            };
            self.stage(|| Ok(16), mutation, Some(expected))?;
        }
        for id in nodes {
            let incident = incident(self.graph(), id, Direction::Both).len();
            if incident > 0 {
                return Err(GraphError::NodeHasEdges {
                    node: NodeId(id),
                    incident,
                }
                .into());
            }
            let expected = self.precondition(Kind::Node, id);
            let key = self
                .graph()
                .node(NodeId(id))
                .expect("live node")
                .key
                .clone();
            let mutation = GraphMutation::DeleteNode { key, expected };
            self.stage(|| Ok(16), mutation, Some(expected))?;
        }
        Ok(())
    }

    fn cell(&self, row: &BindingRow, value: &ReturnValue) -> Result<OutputValue, GraphError> {
        let (variable, field) = match value {
            ReturnValue::Variable(variable) => (&variable.text, None),
            ReturnValue::Field(field) => (&field.variable.text, Some(&field.field.text)),
        };
        let graph = self.graph();
        Ok(match row.get(variable).expect("validated return binding") {
            BoundValue::Path { nodes, edges } => {
                if nodes.iter().any(|id| graph.node(NodeId(*id)).is_none())
                    || edges.iter().any(|id| graph.edge(EdgeId(*id)).is_none())
                {
                    return Err(DELETED);
                }
                OutputValue::Path {
                    nodes: nodes.clone(),
                    edges: edges.clone(),
                }
            }
            BoundValue::Node(id) => {
                let node = graph.node(NodeId(*id)).ok_or(DELETED)?;
                match field {
                    Some(field) => OutputValue::Scalar(node.properties.get(field).cloned()),
                    None => OutputValue::Node(node.clone()),
                }
            }
            BoundValue::Edge(id) => {
                let edge = graph.edge(EdgeId(*id)).ok_or(DELETED)?;
                let properties = edge_properties(graph, edge, self.as_of_ms);
                match field {
                    Some(field) => OutputValue::Scalar(
                        properties
                            .and_then(|properties| properties.get(field))
                            .cloned(),
                    ),
                    None => OutputValue::Edge {
                        edge: edge.clone(),
                        properties: properties.cloned().unwrap_or_default(),
                    },
                }
            }
        })
    }

    /// RETURN over the mutated graph; LIMIT never limits mutations.
    fn project(
        &mut self,
        returning: &ReturnClause,
    ) -> Result<(Vec<String>, Vec<Vec<OutputValue>>), GraphError> {
        let mut projected = Vec::with_capacity(self.rows.len());
        for row in &self.rows {
            self.meter.charge()?;
            self.meter
                .allocate(8 * (1 + returning.items.len()) as u64)?;
            let cells = returning
                .items
                .iter()
                .map(|item| self.cell(row, &item.value))
                .collect::<Result<Vec<_>, _>>()?;
            projected.push((identity(row), cells));
        }
        let columns = returning
            .items
            .iter()
            .map(|item| match (&item.alias, &item.value) {
                (Some(alias), _) => alias.text.clone(),
                (None, ReturnValue::Variable(variable)) => variable.text.clone(),
                (None, ReturnValue::Field(field)) => {
                    format!("{}.{}", field.variable.text, field.field.text)
                }
            })
            .collect::<Vec<_>>();
        if !returning.order_by.is_empty() {
            self.meter.check("sort")?;
            self.meter.allocate(8 * projected.len() as u64)?;
            precharge_sort(projected.len(), self.meter)?;
            let scalar = |cell: &OutputValue| match cell {
                OutputValue::Scalar(value) => value.clone(),
                _ => None,
            };
            projected.sort_by(|left, right| {
                for item in &returning.order_by {
                    let Some(index) = columns.iter().position(|name| *name == item.name.text)
                    else {
                        continue;
                    };
                    let ordering = compare_sort_values(
                        &scalar(&left.1[index]),
                        &scalar(&right.1[index]),
                        item.direction,
                    );
                    if ordering != Ordering::Equal {
                        return ordering;
                    }
                }
                left.0.cmp(&right.0)
            });
            self.meter.check("sort")?;
        }
        let integer = |reference: &Option<ParameterRef>| {
            reference
                .as_ref()
                .map(|reference| match parameter(self.plan, reference) {
                    PropertyValue::Integer(value) => usize::try_from(*value).unwrap_or(usize::MAX),
                    _ => unreachable!("validated non-negative integer"),
                })
        };
        let skip = integer(&returning.skip).unwrap_or(0);
        let mut rows = projected
            .into_iter()
            .skip(skip)
            .map(|(_, cells)| cells)
            .collect::<Vec<_>>();
        if let Some(limit) = integer(&returning.limit) {
            rows.truncate(limit);
        }
        self.meter.returned(rows.len())?;
        let mut nodes = BTreeSet::new();
        let mut edges = BTreeSet::new();
        let mut paths = 0;
        for cell in rows.iter().flatten() {
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
        self.meter.output(nodes.len(), edges.len(), paths)?;
        Ok((columns, rows))
    }
}

/// Bit-exact equality: `-0.0` differs from `0.0` and integers from floats.
fn bit_equal(left: &PropertyValue, right: &PropertyValue) -> bool {
    match (left, right) {
        (PropertyValue::Float(left), PropertyValue::Float(right)) => {
            left.to_bits() == right.to_bits()
        }
        (
            PropertyValue::GeoPoint {
                latitude: a,
                longitude: b,
            },
            PropertyValue::GeoPoint {
                latitude: c,
                longitude: d,
            },
        ) => a.to_bits() == c.to_bits() && b.to_bits() == d.to_bits(),
        _ => left == right,
    }
}
