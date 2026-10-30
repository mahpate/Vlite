//! REQ-021..REQ-023: bounded breadth-first traversal and unweighted
//! shortest paths over the lazy adjacency.
//!
//! Both walk the graph in FIFO order and examine each expanded node's
//! adjacency in (relationship type, edge ID) order, so results depend only
//! on graph content. Paths are canonical: among the shortest paths from the
//! start set, the one whose edge-ID sequence is lexicographically smallest.
//! It is computed exactly over the BFS layer DAG (`best[w]` is the minimum of
//! `best[u] ++ [e]` over the predecessors `(u, e)` of `w`), because the hop
//! ceiling keeps every path at most 4 edges long.

use super::error::GraphError;
use super::filter::{NodeFilter, conditions_match};
use super::model::{Direction, EdgeId, GraphEdge, NodeId, PropertyCondition};
use super::query::{Exec, GraphPath, NodeSelector, QueryOutput, TraversedNode, count};
use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap};

/// The endpoint of `edge` reached from `node`; `node` itself for a self-loop.
pub(super) fn other_end(edge: &GraphEdge, node: NodeId) -> NodeId {
    if edge.source == node {
        edge.destination
    } else {
        edge.source
    }
}

/// A validated traversal with its start nodes resolved and deduplicated.
pub(super) struct TraversePlan<'q> {
    pub(super) start: Vec<(NodeId, &'q NodeSelector)>,
    pub(super) direction: Direction,
    pub(super) types: Option<&'q BTreeSet<String>>,
    pub(super) expand_filter: &'q NodeFilter,
    pub(super) return_filter: &'q NodeFilter,
    pub(super) edge_filter: &'q [PropertyCondition],
    pub(super) include_paths: bool,
}

/// Visited nodes in discovery order with their depth, and the
/// shortest-path predecessors `(node index, edge)` of each.
#[derive(Default)]
struct Layers {
    order: Vec<(NodeId, u64)>,
    index: HashMap<NodeId, usize>,
    preds: Vec<Vec<(usize, EdgeId)>>,
}

impl Layers {
    fn visit(&mut self, node: NodeId, depth: u64) -> usize {
        let position = self.order.len();
        self.order.push((node, depth));
        self.index.insert(node, position);
        self.preds.push(Vec::new());
        position
    }

    /// For each node, the lexicographically smallest edge sequence among its
    /// shortest paths from a start node, and its predecessor on that path
    /// (itself for start nodes). Predecessors always precede their node.
    fn canonical(&self) -> Vec<(usize, Vec<EdgeId>)> {
        let mut best: Vec<(usize, Vec<EdgeId>)> = Vec::with_capacity(self.preds.len());
        for (position, preds) in self.preds.iter().enumerate() {
            let choice = preds.iter().min_by(|(a, x), (b, y)| {
                (best[*a].1.as_slice(), x).cmp(&(best[*b].1.as_slice(), y))
            });
            best.push(match choice {
                None => (position, Vec::new()),
                Some(&(parent, edge)) => {
                    let mut edges = best[parent].1.clone();
                    edges.push(edge);
                    (parent, edges)
                }
            });
        }
        best
    }

    /// The canonical path ending at `position`.
    fn path(&self, best: &[(usize, Vec<EdgeId>)], position: usize) -> GraphPath {
        let mut nodes = vec![self.order[position].0];
        let mut current = position;
        while best[current].0 != current {
            current = best[current].0;
            nodes.push(self.order[current].0);
        }
        nodes.reverse();
        GraphPath {
            nodes,
            edges: best[position].1.clone(),
        }
    }
}

pub(super) fn traverse(
    exec: &mut Exec<'_, '_>,
    plan: &TraversePlan<'_>,
) -> Result<QueryOutput, GraphError> {
    let graph = exec.graph;
    let limits = exec.limits;
    let mut layers = Layers::default();
    // Result edges in first-examined order, and their positions.
    let mut edges: Vec<EdgeId> = Vec::new();
    let mut positions: HashMap<EdgeId, usize> = HashMap::new();
    let mut seeded = true;
    for &(node, _) in &plan.start {
        if count(layers.order.len()) >= limits.max_visited_nodes {
            exec.truncation.visited();
            seeded = false;
            break;
        }
        layers.visit(node, 0);
    }
    let mut head = 0;
    'expand: while seeded && head < layers.order.len() {
        let (node, depth) = layers.order[head];
        let current = head;
        head += 1;
        // Depths never decrease in FIFO order.
        if depth >= limits.max_hops {
            break;
        }
        if !graph
            .node(node)
            .is_some_and(|record| plan.expand_filter.matches(record))
        {
            continue;
        }
        for (examined, (_, id)) in graph
            .adjacency(node, plan.direction, plan.types, None)
            .enumerate()
        {
            if count(examined) >= limits.max_edges_per_node {
                exec.truncation.degree(node);
                continue 'expand;
            }
            if let Err(stop) = exec.meter.charge() {
                exec.truncation.stop(stop);
                break 'expand;
            }
            let Some(edge) = graph.edge(id).filter(|edge| !exec.edge_hidden(edge)) else {
                continue;
            };
            if !conditions_match(&edge.properties, plan.edge_filter) {
                continue;
            }
            let next = other_end(edge, node);
            let next_position = match layers.index.get(&next) {
                Some(&position) => position,
                None if count(layers.order.len()) >= limits.max_visited_nodes => {
                    exec.truncation.visited();
                    break 'expand;
                }
                None => layers.visit(next, depth + 1),
            };
            if let Entry::Vacant(slot) = positions.entry(id) {
                slot.insert(edges.len());
                edges.push(id);
            }
            if plan.include_paths && layers.order[next_position].1 == depth + 1 {
                layers.preds[next_position].push((current, id));
            }
        }
    }
    exec.stats.nodes_visited = count(layers.order.len());
    exec.stats.edges_examined = exec.meter.spent();

    let mut nodes = Vec::new();
    let mut returned = Vec::new();
    for (position, &(node, depth)) in layers.order.iter().enumerate() {
        let Some(record) = graph.node(node) else {
            continue;
        };
        if !plan.return_filter.matches(record) {
            continue;
        }
        if count(nodes.len()) >= limits.max_output_nodes {
            exec.truncation.output();
            break;
        }
        nodes.push(TraversedNode {
            depth,
            node: exec.node_view(record),
        });
        returned.push(position);
    }
    let kept = usize::try_from(limits.max_output_edges)
        .unwrap_or(usize::MAX)
        .min(edges.len());
    if kept < edges.len() {
        exec.truncation.output();
    }
    let edge_views = edges[..kept]
        .iter()
        .map(|id| {
            graph
                .edge(*id)
                .ok_or(GraphError::InvariantViolation(
                    "traversal edge is missing during read",
                ))
                .and_then(|edge| exec.edge_view(edge))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut paths = Vec::new();
    if plan.include_paths {
        let best = layers.canonical();
        for position in returned {
            if layers.order[position].1 == 0 {
                continue;
            }
            if count(paths.len()) >= limits.max_output_paths {
                exec.truncation.output();
                break;
            }
            let path = layers.path(&best, position);
            // Edges are examined in BFS order, so a path whose last edge was
            // kept has all of its edges kept; checking each is cheap.
            if path
                .edges
                .iter()
                .all(|id| positions.get(id).is_some_and(|&at| at < kept))
            {
                paths.push(path);
            } else {
                exec.truncation.output();
            }
        }
    }
    Ok(QueryOutput::Traversal {
        nodes,
        edges: edge_views,
        paths,
    })
}

/// The answer of [`shortest_path`] before views are attached.
pub(super) enum Shortest {
    Found(GraphPath),
    NoPath,
    Exhausted,
}

/// Layered BFS from `source`. Any truncation ends the search as
/// `Exhausted`, so `Found` and `NoPath` are certified: every layer below
/// the destination's (or every layer up to `max_hops`) was fully expanded.
/// Nodes in the last layer are not recorded because they are never
/// expanded; the destination counts as a visited node.
pub(super) fn shortest_path(
    exec: &mut Exec<'_, '_>,
    source: NodeId,
    destination: NodeId,
    direction: Direction,
    types: Option<&BTreeSet<String>>,
) -> Shortest {
    let mut layers = Layers::default();
    layers.visit(source, 0);
    // Predecessors of the destination, which is never expanded.
    let mut arrivals = Vec::new();
    let outcome = if source == destination {
        Shortest::Found(GraphPath {
            nodes: vec![source],
            edges: Vec::new(),
        })
    } else {
        search(
            exec,
            &mut layers,
            &mut arrivals,
            destination,
            direction,
            types,
        )
    };
    exec.stats.nodes_visited =
        count(layers.order.len()).saturating_add(u64::from(!arrivals.is_empty()));
    exec.stats.edges_examined = exec.meter.spent();
    outcome
}

fn search(
    exec: &mut Exec<'_, '_>,
    layers: &mut Layers,
    arrivals: &mut Vec<(usize, EdgeId)>,
    destination: NodeId,
    direction: Direction,
    types: Option<&BTreeSet<String>>,
) -> Shortest {
    let graph = exec.graph;
    let limits = exec.limits;
    let mut layer = 0..layers.order.len();
    for depth in 0..limits.max_hops {
        let last = depth + 1 == limits.max_hops;
        for current in layer.clone() {
            let node = layers.order[current].0;
            for (examined, (_, id)) in graph.adjacency(node, direction, types, None).enumerate() {
                if count(examined) >= limits.max_edges_per_node {
                    exec.truncation.degree(node);
                    return Shortest::Exhausted;
                }
                if let Err(stop) = exec.meter.charge() {
                    exec.truncation.stop(stop);
                    return Shortest::Exhausted;
                }
                let Some(edge) = graph.edge(id).filter(|edge| !exec.edge_hidden(edge)) else {
                    continue;
                };
                let next = other_end(edge, node);
                if next == destination {
                    if arrivals.is_empty() && count(layers.order.len()) >= limits.max_visited_nodes
                    {
                        exec.truncation.visited();
                        return Shortest::Exhausted;
                    }
                    arrivals.push((current, id));
                    continue;
                }
                if last || !arrivals.is_empty() {
                    continue;
                }
                let next_position = match layers.index.get(&next) {
                    Some(&position) => position,
                    None if count(layers.order.len()) >= limits.max_visited_nodes => {
                        exec.truncation.visited();
                        return Shortest::Exhausted;
                    }
                    None => layers.visit(next, depth + 1),
                };
                if layers.order[next_position].1 == depth + 1 {
                    layers.preds[next_position].push((current, id));
                }
            }
        }
        if let Some((&first, rest)) = arrivals.split_first() {
            let best = layers.canonical();
            let mut chosen = first;
            for &(parent, edge) in rest {
                if (best[parent].1.as_slice(), edge) < (best[chosen.0].1.as_slice(), chosen.1) {
                    chosen = (parent, edge);
                }
            }
            let mut path = layers.path(&best, chosen.0);
            path.nodes.push(destination);
            path.edges.push(chosen.1);
            return Shortest::Found(path);
        }
        if layer.end == layers.order.len() {
            return Shortest::NoPath;
        }
        layer = layer.end..layers.order.len();
    }
    Shortest::NoPath
}

#[cfg(test)]
mod tests;
