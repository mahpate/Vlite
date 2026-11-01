//! Sampled betweenness over an extracted projection (Gap 5 S4): unweighted
//! Brandes from seeded sampled sources.
//!
//! Setup builds the computational arcs (parallel arcs collapsed, self-loops
//! dropped), offsets them by tail, ranks every node by
//! `(SplitMix64(seed XOR id), id)` and reserves every buffer, the result
//! rows included, before the first source, so a source never allocates.
//! Each source then runs a breadth-first search counting shortest paths,
//! accumulates dependencies in reverse search order and commits them in one
//! pass over the nodes that also resets the search state and stages the
//! source's candidate rows. Every unit of this work is charged as it runs,
//! so at most 64 operations run between deadline and cancellation checks.
//! A source interrupted by a budget, the deadline or cancellation is
//! discarded: the result keeps the rows staged by the last completed
//! source, or nothing before the first one, and no work follows it.

use super::iterative::Iterated;
use super::{
    AlgorithmError, AlgorithmOutput, AlgorithmTruncation, BetweennessOutput, Direction, Halt,
    Meter, NodeId, NodeScore, Projection, orient, sort_by, sort_dedup,
};
use std::mem::size_of;

#[cfg(test)]
thread_local! {
    /// Betweenness work done on this thread: one per charged unit (tests
    /// only). Sorts count in `SORTED` instead.
    pub(super) static WORKED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Records one unit of betweenness work (a no-op outside tests).
fn worked() {
    #[cfg(test)]
    WORKED.with(|count| count.set(count.get() + 1));
}

/// One `max_vertex_operations` unit of betweenness work.
fn vertex(meter: &mut Meter<'_>) -> Result<(), AlgorithmTruncation> {
    meter.vertex()?;
    worked();
    Ok(())
}

/// One `max_edges_examined` unit of betweenness work.
fn edge(meter: &mut Meter<'_>) -> Result<(), AlgorithmTruncation> {
    meter.edge()?;
    worked();
    Ok(())
}

/// SplitMix64 v1: wrapping arithmetic modulo 2^64.
pub(super) fn splitmix64(input: u64) -> u64 {
    let mut x = input.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// `count` copies of `value`, one vertex unit each as it is written.
fn filled<T: Clone>(
    count: usize,
    value: T,
    meter: &mut Meter<'_>,
) -> Result<Vec<T>, AlgorithmTruncation> {
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        vertex(meter)?;
        items.push(value.clone());
    }
    Ok(items)
}

/// The rows a node-level output keeps: every node, or the first
/// `max_output_nodes`.
fn kept_rows(count: usize, max_output_nodes: u64) -> (usize, bool) {
    if count as u64 > max_output_nodes {
        (max_output_nodes as usize, true)
    } else {
        (count, false)
    }
}

/// One node's state in the running source's search.
#[derive(Clone, Copy)]
struct Visit {
    /// Hops from the source; `usize::MAX` when not reached.
    distance: usize,
    /// Shortest paths from the source.
    paths: f64,
    /// The source's dependency on the node.
    dependency: f64,
}

const UNSEEN: Visit = Visit {
    distance: usize::MAX,
    paths: 0.0,
    dependency: 0.0,
};

/// The search state shared by every source.
struct Brandes<'a> {
    nodes: &'a [NodeId],
    /// Distinct `(tail, head)` positions ascending.
    arcs: Vec<(usize, usize)>,
    /// `arcs[offsets[v]..offsets[v + 1]]` leave node `v`.
    offsets: Vec<usize>,
    /// Every entry is `UNSEEN` between sources.
    visits: Vec<Visit>,
    /// Reached nodes in search order.
    order: Vec<usize>,
    /// Dependencies summed over the completed sources.
    totals: Vec<f64>,
}

impl Brandes<'_> {
    /// One source: the search, then the dependencies, then a pass over every
    /// node that adds its dependency to its total, resets its state and
    /// stages its row into `candidate` (the first `rows` nodes) as
    /// `scale * total / pairs`, or 0 when `pairs` is 0.
    fn source(
        &mut self,
        source: usize,
        (scale, pairs, rows): (f64, f64, usize),
        candidate: &mut Vec<NodeScore>,
        meter: &mut Meter<'_>,
    ) -> Result<(), Halt> {
        candidate.clear();
        self.order.clear();
        self.visits[source] = Visit {
            distance: 0,
            paths: 1.0,
            dependency: 0.0,
        };
        self.order.push(source);
        let mut next = 0;
        while next < self.order.len() {
            vertex(meter)?;
            let tail = self.order[next];
            next += 1;
            let Visit {
                distance, paths, ..
            } = self.visits[tail];
            for &(_, head) in &self.arcs[self.offsets[tail]..self.offsets[tail + 1]] {
                edge(meter)?;
                let reached = &mut self.visits[head];
                if reached.distance == usize::MAX {
                    reached.distance = distance + 1;
                    self.order.push(head);
                }
                if reached.distance == distance + 1 {
                    reached.paths += paths;
                    if !reached.paths.is_finite() {
                        return Err(AlgorithmError::NumericOverflow.into());
                    }
                }
            }
        }
        for &tail in self.order[1..].iter().rev() {
            vertex(meter)?;
            let Visit {
                distance, paths, ..
            } = self.visits[tail];
            let mut dependency = 0.0;
            for &(_, head) in &self.arcs[self.offsets[tail]..self.offsets[tail + 1]] {
                edge(meter)?;
                let after = self.visits[head];
                if after.distance == distance + 1 {
                    dependency += paths / after.paths * (1.0 + after.dependency);
                }
            }
            if !dependency.is_finite() {
                return Err(AlgorithmError::NumericOverflow.into());
            }
            self.visits[tail].dependency = dependency;
        }
        for at in 0..self.nodes.len() {
            vertex(meter)?;
            self.totals[at] += self.visits[at].dependency;
            self.visits[at] = UNSEEN;
            if at < rows {
                let score = if pairs > 0.0 {
                    scale * self.totals[at] / pairs
                } else {
                    0.0
                };
                candidate.push(NodeScore {
                    node_id: self.nodes[at].0,
                    score,
                });
            }
        }
        Ok(())
    }
}

/// Sampled betweenness of `projection` walked in `direction` (see
/// [`super::Algorithm::Betweenness`]).
///
/// Setup, in this order (which also orders `MemoryLimit` stops): the arcs
/// (one edge unit per retained edge; `Both` adds each reverse arc), their
/// sort with duplicates dropped, the offsets (`n + 1` fills, one edge unit
/// per distinct arc, `n` prefix sums), one hash per node and their sort,
/// one copy per selected source, `n` state and `n` total fills, the queue
/// and two row buffers. A source charges a vertex unit per reached node
/// and an edge unit per arc it scans in the search, the same again (the
/// source itself aside) for the dependencies, and a vertex unit per node
/// for the commit. A stop in a later source keeps the completed ones; a
/// stop in the first returns the reason. Non-finite path counts or
/// dependencies are [`AlgorithmError::NumericOverflow`].
pub(super) fn betweenness(
    projection: &Projection,
    direction: Direction,
    sampling_seed: u64,
    max_sampled_sources: u64,
    max_output_nodes: u64,
    meter: &mut Meter<'_>,
) -> Result<Iterated, Halt> {
    let (nodes, edges) = (&projection.nodes, &projection.edges);
    let count = nodes.len();
    let both = matches!(direction, Direction::Both);
    let capacity = if both {
        edges.len().saturating_mul(2)
    } else {
        edges.len()
    };
    meter.reserve(capacity, size_of::<(usize, usize)>())?;
    let mut arcs = Vec::with_capacity(capacity);
    for projected in edges {
        super::visited();
        edge(meter)?;
        let (tail, head) = orient(projected, direction);
        if tail != head {
            arcs.push((tail, head));
            if both {
                arcs.push((head, tail));
            }
        }
    }
    sort_dedup(&mut arcs, meter)?;
    meter.reserve(count + 1, size_of::<usize>())?;
    let mut offsets = filled(count + 1, 0usize, meter)?;
    for &(tail, _) in &arcs {
        edge(meter)?;
        offsets[tail + 1] += 1;
    }
    for at in 0..count {
        vertex(meter)?;
        offsets[at + 1] += offsets[at];
    }

    // Positions ascend with node IDs, so (hash, position) orders as
    // (hash, ID).
    meter.reserve(count, size_of::<(u64, usize)>())?;
    let mut ranked = Vec::with_capacity(count);
    for (at, node) in nodes.iter().enumerate() {
        vertex(meter)?;
        ranked.push((splitmix64(sampling_seed ^ node.0), at));
    }
    sort_by(&mut ranked, |a, b| a < b, meter)?;
    let sampled = (count as u64).min(max_sampled_sources) as usize;
    meter.reserve(sampled.saturating_mul(2), size_of::<u64>())?;
    let mut selected = Vec::with_capacity(sampled);
    for &(_, at) in &ranked[..sampled] {
        vertex(meter)?;
        selected.push(nodes[at].0);
    }
    let mut completed = Vec::with_capacity(sampled);

    meter.reserve(count, size_of::<Visit>())?;
    let visits = filled(count, UNSEEN, meter)?;
    meter.reserve(count, size_of::<usize>())?;
    let order = Vec::with_capacity(count);
    meter.reserve(count, size_of::<f64>())?;
    let totals = filled(count, 0.0, meter)?;
    // The committed rows and the candidate rows of the running source.
    let (rows, cut) = kept_rows(count, max_output_nodes);
    meter.reserve(rows.saturating_mul(2), size_of::<NodeScore>())?;
    let mut scores = Vec::with_capacity(rows);
    let mut candidate = Vec::with_capacity(rows);

    let mut search = Brandes {
        nodes,
        arcs,
        offsets,
        visits,
        order,
        totals,
    };
    let n = count as f64;
    // Exact: scopes hold far fewer than 2^26 nodes.
    let pairs = if count > 2 {
        (n - 1.0) * (n - 2.0)
    } else {
        0.0
    };
    let mut stop = None;
    for (done, &(_, source)) in ranked[..sampled].iter().enumerate() {
        let scale = n / (done + 1) as f64;
        match search.source(source, (scale, pairs, rows), &mut candidate, meter) {
            Ok(()) => {
                std::mem::swap(&mut scores, &mut candidate);
                completed.push(selected[done]);
            }
            Err(Halt::Truncated(reason)) if done > 0 => {
                stop = Some(reason);
                break;
            }
            Err(halt) => return Err(halt),
        }
    }
    let completed_source_count = completed.len() as u64;
    Ok(Iterated {
        output: AlgorithmOutput::Betweenness(BetweennessOutput {
            scores,
            selected_sources: selected,
            completed_sources: completed,
            completed_source_count,
            estimated: completed_source_count < count as u64,
        }),
        cut,
        stop,
    })
}
