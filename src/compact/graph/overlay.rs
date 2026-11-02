//! Private committed-state projection candidate, not an ordered write planner.
//! Admission/folding may rebuild a source; reads never rebuild CSR or engine state.

use super::read::ReadEdge;
use super::*;
use crate::property_graph::engine::Direction;

#[derive(Clone, Copy, Debug)]
enum Deadline {
    Preserve,
    Set(Option<u64>),
}

impl Deadline {
    fn resolve(self, previous: Option<u64>) -> Option<u64> {
        match self {
            Self::Preserve => previous,
            Self::Set(value) => value,
        }
    }
}

struct Change<T> {
    id: u64,
    row: Option<T>,
    deadline: Deadline,
}

struct Commit {
    sequence: u64,
    nodes: Vec<Change<SourceNode>>,
    edges: Vec<Change<SourceEdge>>,
    high_water: Option<(u64, u64)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum OverlayError {
    Capacity { cap: usize, needed: usize },
    Sequence,
    DuplicateId,
    MissingRow,
    IdReuse,
    IdExhausted,
    Identity,
    IncidentEdges,
    HighWater,
    Invalid(CodecError),
}

#[derive(Clone)]
struct Version<T> {
    sequence: u64,
    row: Option<T>,
}

#[derive(Clone)]
struct Overlay<'g> {
    base: &'g CompactGraph,
    cap: usize,
    sequence: u64,
    next_node_id: u64,
    next_edge_id: u64,
    node_keys: BTreeMap<&'g str, u64>,
    edge_keys: BTreeMap<&'g str, u64>,
    nodes: BTreeMap<u64, Version<SourceNode>>,
    edges: BTreeMap<u64, Version<SourceEdge>>,
    // At most two memberships per live overlay edge. Replacements remove the
    // old memberships; base removals are resolved through the version maps.
    adjacency: BTreeMap<u64, BTreeSet<u64>>,
}

impl<'g> Overlay<'g> {
    fn new(base: &'g CompactGraph, cap: usize) -> Self {
        Self {
            base,
            cap,
            sequence: base.source.sequence,
            next_node_id: base.source.next_node_id,
            next_edge_id: base.source.next_edge_id,
            node_keys: base
                .source
                .nodes
                .iter()
                .filter_map(|node| node.key.as_deref().map(|key| (key, node.id)))
                .collect(),
            edge_keys: base
                .source
                .edges
                .iter()
                .filter_map(|edge| edge.key.as_deref().map(|key| (key, edge.id)))
                .collect(),
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
            adjacency: BTreeMap::new(),
        }
    }

    fn raw_node(&self, id: u64) -> Option<&SourceNode> {
        match self.nodes.get(&id) {
            Some(version) => version.row.as_ref(),
            None => self
                .base
                .node_ordinals
                .get(&id)
                .map(|&ordinal| &self.base.source.nodes[ordinal as usize]),
        }
    }

    fn raw_edge(&self, id: u64) -> Option<&SourceEdge> {
        match self.edges.get(&id) {
            Some(version) => version.row.as_ref(),
            None => self
                .base
                .source
                .edges
                .binary_search_by_key(&id, |edge| edge.id)
                .ok()
                .map(|index| &self.base.source.edges[index]),
        }
    }

    fn node(&self, id: u64, clock: u64) -> Option<&SourceNode> {
        self.raw_node(id)
            .filter(|node| node.deadline.is_none_or(|deadline| clock < deadline))
    }

    fn edge(&self, id: u64, clock: u64, as_of: Option<i64>) -> Option<ReadEdge<'_>> {
        let edge = self.raw_edge(id)?;
        if edge.deadline.is_some_and(|deadline| clock >= deadline)
            || self.node(edge.source, clock).is_none()
            || self.node(edge.target, clock).is_none()
        {
            return None;
        }
        let properties = match (as_of, &edge.segments) {
            (Some(time), Some(segments)) => {
                let end = segments
                    .partition_point(|segment| segment.valid_from.is_none_or(|from| from <= time));
                let segment = segments.get(end.checked_sub(1)?)?;
                if segment.valid_to.is_some_and(|to| time >= to) {
                    return None;
                }
                &segment.properties
            }
            _ => &edge.properties,
        };
        Some(ReadEdge { edge, properties })
    }

    fn node_by_key(&self, key: &str, clock: u64) -> Option<&SourceNode> {
        self.nodes
            .values()
            .filter_map(|version| version.row.as_ref())
            .find(|node| node.key.as_deref() == Some(key))
            .and_then(|node| self.node(node.id, clock))
            .or_else(|| {
                self.node(*self.node_keys.get(key)?, clock)
                    .filter(|node| node.key.as_deref() == Some(key))
            })
    }

    fn edge_by_key(&self, key: &str, clock: u64, as_of: Option<i64>) -> Option<ReadEdge<'_>> {
        self.edges
            .values()
            .filter_map(|version| version.row.as_ref())
            .find(|edge| edge.key.as_deref() == Some(key))
            .and_then(|edge| self.edge(edge.id, clock, as_of))
            .or_else(|| {
                self.edge(*self.edge_keys.get(key)?, clock, as_of)
                    .filter(|read| read.edge.key.as_deref() == Some(key))
            })
    }

    fn neighbors(
        &self,
        node: u64,
        direction: Direction,
        types: Option<&BTreeSet<String>>,
        clock: u64,
        as_of: Option<i64>,
    ) -> Vec<ReadEdge<'_>> {
        if self.node(node, clock).is_none() {
            return Vec::new();
        }
        let mut ids = BTreeSet::new();
        if let Some(&ordinal) = self.base.node_ordinals.get(&node) {
            for (csr, enabled) in [
                (&self.base.out, direction != Direction::In),
                (&self.base.incoming, direction != Direction::Out),
            ] {
                if enabled {
                    let index = ordinal as usize;
                    for &edge in
                        &csr.edges[csr.offsets[index] as usize..csr.offsets[index + 1] as usize]
                    {
                        ids.insert(self.base.edge_rows[edge as usize].id);
                    }
                }
            }
        }
        if let Some(additions) = self.adjacency.get(&node) {
            ids.extend(additions);
        }
        let mut result: Vec<_> = ids
            .into_iter()
            .filter_map(|id| self.edge(id, clock, as_of))
            .filter(|read| {
                let edge = read.edge;
                (direction != Direction::In && edge.source == node
                    || direction != Direction::Out && edge.target == node)
                    && types.is_none_or(|types| types.contains(&edge.relationship))
            })
            .collect();
        result.sort_by(|left, right| {
            (&left.edge.relationship, left.edge.id).cmp(&(&right.edge.relationship, right.edge.id))
        });
        result
    }

    fn node_ids(&self) -> BTreeSet<u64> {
        self.base
            .source
            .nodes
            .iter()
            .map(|node| node.id)
            .chain(self.nodes.keys().copied())
            .collect()
    }

    fn edge_ids(&self) -> BTreeSet<u64> {
        self.base
            .source
            .edges
            .iter()
            .map(|edge| edge.id)
            .chain(self.edges.keys().copied())
            .collect()
    }

    fn counts(&self, clock: u64) -> (usize, usize) {
        (
            self.node_ids()
                .into_iter()
                .filter(|&id| self.node(id, clock).is_some())
                .count(),
            self.edge_ids()
                .into_iter()
                .filter(|&id| self.edge(id, clock, None).is_some())
                .count(),
        )
    }

    fn source(&self) -> GraphSource {
        let edges: Vec<_> = self
            .edge_ids()
            .into_iter()
            .filter_map(|id| self.raw_edge(id).cloned())
            .collect();
        GraphSource {
            sequence: self.sequence,
            next_node_id: self.next_node_id,
            next_edge_id: self.next_edge_id,
            temporal_enabled: edges.iter().any(|edge| edge.segments.is_some()),
            nodes: self
                .node_ids()
                .into_iter()
                .filter_map(|id| self.raw_node(id).cloned())
                .collect(),
            edges,
            ..self.base.source.clone()
        }
    }

    /// Physical fold: retain deadlines/expired rows and high-water marks.
    /// Reclamation and durable replacement are deliberately not implemented.
    fn fold(&self) -> Result<CompactGraph, OverlayError> {
        decode(&encode(&self.source()).map_err(OverlayError::Invalid)?)
            .map_err(OverlayError::Invalid)
    }

    fn apply(&mut self, mut commit: Commit) -> Result<(), OverlayError> {
        if commit.sequence <= self.sequence {
            return Err(OverlayError::Sequence);
        }
        commit.nodes.sort_by_key(|change| change.id);
        commit.edges.sort_by_key(|change| change.id);
        if commit.nodes.windows(2).any(|pair| pair[0].id == pair[1].id)
            || commit.edges.windows(2).any(|pair| pair[0].id == pair[1].id)
        {
            return Err(OverlayError::DuplicateId);
        }
        let needed = self.nodes.len()
            + self.edges.len()
            + commit
                .nodes
                .iter()
                .filter(|change| !self.nodes.contains_key(&change.id))
                .count()
            + commit
                .edges
                .iter()
                .filter(|change| !self.edges.contains_key(&change.id))
                .count();
        if needed > self.cap {
            return Err(OverlayError::Capacity {
                cap: self.cap,
                needed,
            });
        }
        let mut candidate = self.clone();
        candidate.sequence = commit.sequence;
        let mut deleted_nodes = BTreeSet::new();
        for change in commit.nodes {
            let previous = candidate.raw_node(change.id);
            let row = match change.row {
                Some(mut row) => {
                    if row.id != change.id
                        || previous.is_some_and(|previous| row.key != previous.key)
                    {
                        return Err(OverlayError::Identity);
                    }
                    row.deadline = change
                        .deadline
                        .resolve(previous.and_then(|node| node.deadline));
                    if previous.is_none() {
                        allocate(&mut candidate.next_node_id, change.id)?;
                    }
                    row.labels.sort();
                    Some(row)
                }
                None => {
                    if previous.is_none() {
                        return Err(OverlayError::MissingRow);
                    }
                    deleted_nodes.insert(change.id);
                    None
                }
            };
            candidate.nodes.insert(
                change.id,
                Version {
                    sequence: commit.sequence,
                    row,
                },
            );
        }
        for change in commit.edges {
            let previous = candidate.raw_edge(change.id);
            let row = match change.row {
                Some(mut row) => {
                    if row.id != change.id
                        || previous.is_some_and(|previous| {
                            (row.source, row.target, &row.relationship, &row.key)
                                != (
                                    previous.source,
                                    previous.target,
                                    &previous.relationship,
                                    &previous.key,
                                )
                        })
                    {
                        return Err(OverlayError::Identity);
                    }
                    row.deadline = change
                        .deadline
                        .resolve(previous.and_then(|edge| edge.deadline));
                    if previous.is_none() {
                        allocate(&mut candidate.next_edge_id, change.id)?;
                    }
                    Some(row)
                }
                None => {
                    if previous.is_none() {
                        return Err(OverlayError::MissingRow);
                    }
                    None
                }
            };
            candidate.edges.insert(
                change.id,
                Version {
                    sequence: commit.sequence,
                    row,
                },
            );
        }
        if candidate.edge_ids().into_iter().any(|id| {
            candidate.raw_edge(id).is_some_and(|edge| {
                deleted_nodes.contains(&edge.source) || deleted_nodes.contains(&edge.target)
            })
        }) {
            return Err(OverlayError::IncidentEdges);
        }
        if let Some((nodes, edges)) = commit.high_water {
            if nodes < candidate.next_node_id || edges < candidate.next_edge_id {
                return Err(OverlayError::HighWater);
            }
            candidate.next_node_id = nodes;
            candidate.next_edge_id = edges;
        }
        // Validate keys, physical endpoints, properties, schema/temporal state
        // and graph-local attachment ownership before publishing any delta.
        encode(&candidate.source()).map_err(OverlayError::Invalid)?;
        candidate.adjacency.clear();
        for version in candidate.edges.values() {
            if let Some(edge) = &version.row {
                for node in [edge.source, edge.target] {
                    candidate.adjacency.entry(node).or_default().insert(edge.id);
                }
            }
        }
        *self = candidate;
        Ok(())
    }
}

fn allocate(mark: &mut u64, id: u64) -> Result<(), OverlayError> {
    if id < *mark {
        return Err(OverlayError::IdReuse);
    }
    *mark = id.checked_add(1).ok_or(OverlayError::IdExhausted)?;
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod seeded;
