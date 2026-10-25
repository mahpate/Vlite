//! An independent reference model of the EPIC-005 read semantics (TEST-006):
//! plain maps of live nodes and edges, a textbook FIFO breadth-first search
//! and brute-force enumeration of canonical paths. It shares no code with
//! the engine, only the documented semantics.
//!
//! - Adjacency is ordered by (relationship type, edge ID); `Both` lists a
//!   self-loop once.
//! - Nodes at depth `max_hops` or failing the expand filter are not expanded.
//! - Every qualifying edge examined from an expanded node is returned once,
//!   in first-examined order.
//! - A path is the lexicographically smallest edge-ID sequence among the
//!   shortest walks from any start node whose non-final nodes are expanded.

#![allow(dead_code)]

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use vlite::{Condition, Metadata, MetadataValue};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Out,
    In,
    Both,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub labels: BTreeSet<String>,
    pub properties: Metadata,
}

#[derive(Clone, Debug)]
pub struct Edge {
    pub source: u64,
    pub target: u64,
    pub relationship: String,
    pub properties: Metadata,
}

/// A node filter of the model: all `labels` present and all conditions met.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub labels: Vec<String>,
    pub conditions: Vec<Condition>,
}

#[derive(Clone, Debug)]
pub struct Query {
    pub start: Vec<u64>,
    pub dir: Dir,
    pub types: Option<Vec<String>>,
    pub expand: Filter,
    pub ret: Filter,
    pub edge: Vec<Condition>,
    pub max_hops: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// Returned nodes in discovery order with their depth.
    pub nodes: Vec<(u64, u64)>,
    pub edges: Vec<u64>,
    /// `(nodes, edges)` per returned non-start node, in node order.
    pub paths: Vec<(Vec<u64>, Vec<u64>)>,
}

/// A walk: its nodes and edges.
pub type Walk = (Vec<u64>, Vec<u64>);

#[derive(Clone, Debug, Default)]
pub struct Model {
    pub nodes: BTreeMap<u64, Node>,
    pub edges: BTreeMap<u64, Edge>,
}

/// Integer and string condition semantics: the generators only use `Eq` and
/// integer `Range` (full parity with record filters, including floats and
/// geo, is proven by the adapter tests).
pub fn conditions_hold(properties: &Metadata, conditions: &[Condition]) -> bool {
    let integer = |bound: &Option<MetadataValue>| match bound {
        None => None,
        Some(MetadataValue::Integer(bound)) => Some(*bound),
        Some(other) => panic!("the model supports integer bounds only: {other:?}"),
    };
    conditions.iter().all(|condition| match condition {
        Condition::Eq { field, value } => properties.get(field) == Some(value),
        Condition::Range { field, min, max } => match properties.get(field) {
            Some(MetadataValue::Integer(value)) => {
                integer(min).is_none_or(|min| *value >= min)
                    && integer(max).is_none_or(|max| *value <= max)
            }
            _ => false,
        },
        Condition::GeoRadius { .. } => panic!("the model does not support geo conditions"),
    })
}

impl Filter {
    fn holds(&self, node: &Node) -> bool {
        self.labels.iter().all(|label| node.labels.contains(label))
            && conditions_hold(&node.properties, &self.conditions)
    }
}

impl Model {
    /// The edges incident to `node` in `dir`, ordered by (type, ID).
    pub fn adjacency(&self, node: u64, dir: Dir, types: Option<&[String]>) -> Vec<u64> {
        let mut list: Vec<(&str, u64)> = self
            .edges
            .iter()
            .filter(|(_, edge)| match dir {
                Dir::Out => edge.source == node,
                Dir::In => edge.target == node,
                Dir::Both => edge.source == node || edge.target == node,
            })
            .filter(|(_, edge)| types.is_none_or(|types| types.contains(&edge.relationship)))
            .map(|(id, edge)| (edge.relationship.as_str(), *id))
            .collect();
        list.sort_unstable();
        list.into_iter().map(|(_, id)| id).collect()
    }

    fn other(&self, edge: u64, node: u64) -> u64 {
        let edge = &self.edges[&edge];
        if edge.source == node {
            edge.target
        } else {
            edge.source
        }
    }

    /// `(edge, next node)` for each step from `node`; with `filtered`, only
    /// edges passing the query's edge filter.
    fn steps(&self, node: u64, query: &Query, filtered: bool) -> Vec<(u64, u64)> {
        self.adjacency(node, query.dir, query.types.as_deref())
            .into_iter()
            .filter(|edge| !filtered || conditions_hold(&self.edges[edge].properties, &query.edge))
            .map(|edge| (edge, self.other(edge, node)))
            .collect()
    }

    fn expandable(&self, node: u64, query: &Query) -> bool {
        query.expand.holds(&self.nodes[&node])
    }

    pub fn traverse(&self, query: &Query) -> Answer {
        let mut order: Vec<(u64, u64)> = Vec::new();
        let mut depth: HashMap<u64, u64> = HashMap::new();
        for &start in &query.start {
            if let Entry::Vacant(slot) = depth.entry(start) {
                slot.insert(0);
                order.push((start, 0));
            }
        }
        let mut edges = Vec::new();
        let mut seen_edges = BTreeSet::new();
        let mut head = 0;
        while head < order.len() {
            let (node, level) = order[head];
            head += 1;
            if level >= query.max_hops || !self.expandable(node, query) {
                continue;
            }
            for (edge, next) in self.steps(node, query, true) {
                if let Entry::Vacant(slot) = depth.entry(next) {
                    slot.insert(level + 1);
                    order.push((next, level + 1));
                }
                if seen_edges.insert(edge) {
                    edges.push(edge);
                }
            }
        }
        let nodes: Vec<(u64, u64)> = order
            .into_iter()
            .filter(|(node, _)| query.ret.holds(&self.nodes[node]))
            .collect();
        let paths = nodes
            .iter()
            .filter(|(_, level)| *level > 0)
            .map(|&(node, level)| {
                self.best_walk(&query.start, node, level, query, true)
                    .expect("a discovered node has a walk of its depth")
            })
            .collect();
        Answer {
            nodes,
            edges,
            paths,
        }
    }

    /// The smallest edge-ID sequence of exactly `length` steps from any
    /// start to `target`, by exhaustive enumeration. With `filtered`, every
    /// non-final node must pass the expand filter and every edge the edge
    /// filter.
    fn best_walk(
        &self,
        starts: &[u64],
        target: u64,
        length: u64,
        query: &Query,
        filtered: bool,
    ) -> Option<Walk> {
        let mut best: Option<Walk> = None;
        let mut stack: Vec<Walk> = starts
            .iter()
            .map(|&start| (vec![start], Vec::new()))
            .collect();
        while let Some((nodes, edges)) = stack.pop() {
            let at = *nodes.last().unwrap();
            if edges.len() as u64 == length {
                if at == target && best.as_ref().is_none_or(|(_, best)| edges < *best) {
                    best = Some((nodes, edges));
                }
                continue;
            }
            if filtered && !self.expandable(at, query) {
                continue;
            }
            for (edge, next) in self.steps(at, query, filtered) {
                let mut nodes = nodes.clone();
                let mut edges = edges.clone();
                nodes.push(next);
                edges.push(edge);
                stack.push((nodes, edges));
            }
        }
        best
    }

    /// The canonical shortest path within `max_hops` (direction and types
    /// only), or `None`.
    pub fn shortest_path(
        &self,
        source: u64,
        destination: u64,
        dir: Dir,
        types: Option<Vec<String>>,
        max_hops: u64,
    ) -> Option<Walk> {
        let query = Query {
            start: vec![source],
            dir,
            types,
            expand: Filter::default(),
            ret: Filter::default(),
            edge: Vec::new(),
            max_hops,
        };
        (0..=max_hops)
            .find_map(|length| self.best_walk(&[source], destination, length, &query, false))
    }
}
