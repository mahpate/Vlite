//! Iterative algorithms over an extracted projection (Gap 5 S3): PageRank
//! by synchronous full steps and label propagation by in-place sweeps.
//!
//! Both reserve every buffer, charge every setup pass and prepay their
//! result rows before the first step, so a step never allocates. A step
//! interrupted by a budget, the deadline or cancellation is discarded: the
//! result keeps the last full step, or nothing before the first one.

use super::{
    AlgorithmError, AlgorithmOutput, AlgorithmTruncation, CommunityAssignment, Direction, EdgeId,
    Graph, GraphError, Halt, LabelPropagationOutput, Local, Meter, NodeId, NodeScore,
    PageRankOutput, Projection, PropertyValue, Seed, classify, orient, resolve, sort_cost,
};
use std::mem::size_of;

/// A resolved personalization: distinct seeds ascending with their masses
/// as given, and the masses' sum in that order.
pub(super) struct Teleport {
    pub(super) masses: Vec<(NodeId, f64)>,
    pub(super) total: f64,
}

/// Resolves, checks, sorts and sums personalization `entries`: one vertex
/// unit per entry, then the sort and one unit per entry for the
/// duplicate check and the sum. Every node must be a seed.
pub(super) fn teleport<S: Seed>(
    graph: &Graph,
    entries: &[(S, f64)],
    seeds: &[NodeId],
    expired_at: Option<u64>,
    meter: &mut Meter<'_>,
) -> Result<Teleport, Halt> {
    meter.reserve(entries.len(), size_of::<(NodeId, f64)>())?;
    let mut masses = Vec::with_capacity(entries.len());
    for (selector, mass) in entries {
        meter.vertex()?;
        let node = resolve(graph, selector.selector(), expired_at)?.id;
        let positive = mass.is_finite() && *mass > 0.0;
        if !positive {
            return Err(GraphError::InvalidQuery(
                "personalization masses must be positive and finite",
            )
            .into());
        }
        if seeds.binary_search(&node).is_err() {
            return Err(GraphError::InvalidQuery("personalization nodes must be seeds").into());
        }
        masses.push((node, *mass));
    }
    meter.vertices(sort_cost(masses.len()))?;
    masses.sort_unstable_by_key(|entry| entry.0);
    let mut total = 0.0;
    for (at, &(node, mass)) in masses.iter().enumerate() {
        meter.vertex()?;
        if at > 0 && masses[at - 1].0 == node {
            return Err(GraphError::InvalidQuery("personalization must not repeat a node").into());
        }
        total += mass;
    }
    if !total.is_finite() {
        return Err(AlgorithmError::NumericOverflow.into());
    }
    Ok(Teleport { masses, total })
}

/// The settings of one PageRank run.
pub(super) struct Rank<'a> {
    pub(super) damping: f64,
    pub(super) tolerance: f64,
    pub(super) weight_property: Option<&'a str>,
    pub(super) teleport: Option<&'a Teleport>,
    pub(super) max_iterations: u64,
}

/// An iterative result: the last full step, whether rows were cut to
/// `max_output_nodes`, and why the run stopped early (`None` when it
/// converged).
pub(super) struct Iterated {
    pub(super) output: AlgorithmOutput,
    pub(super) cut: bool,
    pub(super) stop: Option<AlgorithmTruncation>,
}

/// Runs `step` until it converges, `max_iterations` full steps finish or
/// it stops; `step` commits its state only when it finishes and reports
/// convergence. Returns the full steps and the stop reason; a stop in the
/// first step returns the reason itself.
fn iterate(
    max_iterations: u64,
    meter: &mut Meter<'_>,
    mut step: impl FnMut(&mut Meter<'_>) -> Result<bool, AlgorithmTruncation>,
) -> Result<(u64, Option<AlgorithmTruncation>), Halt> {
    let mut done = 0;
    loop {
        match step(meter) {
            Ok(true) => return Ok((done + 1, None)),
            Ok(false) => {
                done += 1;
                if done == max_iterations {
                    return Ok((done, Some(AlgorithmTruncation::IterationLimit)));
                }
            }
            Err(reason) if done == 0 => return Err(reason.into()),
            Err(reason) => return Ok((done, Some(reason))),
        }
    }
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

impl Local<'_> {
    /// PageRank over the computational arcs of `projection`.
    ///
    /// Setup validates every retained weight in edge-ID order, sums each
    /// node's out-weight in arc order (an edge forward, then reversed for a
    /// `Both` non-loop) and rejects a non-finite sum. A step, with `r` the
    /// last vector and `p` the teleport vector, takes three passes: the
    /// dangling mass (nodes with no out-weight) ascending; every arc by
    /// edge ID adding `r[tail] * (w / out[tail])` to its head; then each
    /// node's `(1 - d) * p + d * (arcs + dangling * p)` and its absolute
    /// change ascending.
    pub(super) fn page_rank(
        &self,
        projection: &Projection,
        rank: &Rank<'_>,
        meter: &mut Meter<'_>,
    ) -> Result<Iterated, Halt> {
        let (nodes, edges) = (&projection.nodes, &projection.edges);
        let count = nodes.len();
        let direction = self.direction;
        let both = direction == Direction::Both;
        meter.reserve(edges.len(), size_of::<f64>())?;
        meter.reserve(count, size_of::<f64>())?;
        meter.vertices(count as u64)?;
        let mut weights = Vec::with_capacity(edges.len());
        let mut sums = vec![0.0f64; count];
        for edge in edges {
            meter.edge()?;
            let weight = match rank.weight_property {
                None => 1.0,
                Some(field) => self.rank_weight(edge.id, field)?,
            };
            weights.push(weight);
            let (tail, head) = orient(edge, direction);
            sums[tail] += weight;
            if both && tail != head {
                sums[head] += weight;
            }
        }
        for sum in &sums {
            meter.vertex()?;
            if !sum.is_finite() {
                return Err(AlgorithmError::NumericOverflow.into());
            }
        }

        meter.reserve(count, size_of::<f64>())?;
        meter.vertices(count as u64)?;
        let teleport = match rank.teleport {
            None => vec![1.0 / count as f64; count],
            Some(teleport) => {
                let mut values = vec![0.0; count];
                for &(node, mass) in &teleport.masses {
                    meter.vertex()?;
                    let at = nodes.binary_search(&node).map_err(|_| {
                        GraphError::InvariantViolation("a personalized seed is outside the scope")
                    })?;
                    values[at] = mass / teleport.total;
                }
                values
            }
        };
        meter.reserve(count, size_of::<f64>())?;
        meter.vertices(count as u64)?;
        let mut ranks = teleport.clone();
        meter.reserve(count, size_of::<f64>())?;
        meter.vertices(count as u64)?;
        let mut next = vec![0.0f64; count];
        let (rows, cut) = kept_rows(count, self.limits.max_output_nodes);
        meter.reserve(rows, size_of::<NodeScore>())?;
        meter.vertices(rows as u64)?;
        let mut scores = Vec::with_capacity(rows);

        let damping = rank.damping;
        let mut residual = 0.0;
        let (iterations, stop) = iterate(rank.max_iterations, meter, |meter| {
            let mut dangling = 0.0;
            for (slot, (&sum, &value)) in next.iter_mut().zip(sums.iter().zip(&ranks)) {
                meter.vertex()?;
                *slot = 0.0;
                if sum == 0.0 {
                    dangling += value;
                }
            }
            let mut spread = |tail: usize, head: usize, weight: f64, meter: &mut Meter<'_>| {
                meter.edge()?;
                if sums[tail] != 0.0 {
                    next[head] += ranks[tail] * (weight / sums[tail]);
                }
                Ok::<(), AlgorithmTruncation>(())
            };
            for (edge, &weight) in edges.iter().zip(&weights) {
                let (tail, head) = orient(edge, direction);
                spread(tail, head, weight, meter)?;
                if both && tail != head {
                    spread(head, tail, weight, meter)?;
                }
            }
            let mut change = 0.0;
            for ((slot, &old), &jump) in next.iter_mut().zip(&ranks).zip(&teleport) {
                meter.vertex()?;
                let value = (1.0 - damping) * jump + damping * (*slot + dangling * jump);
                change += (value - old).abs();
                *slot = value;
            }
            std::mem::swap(&mut ranks, &mut next);
            residual = change;
            Ok(change <= rank.tolerance)
        })?;
        scores.extend(
            nodes[..rows]
                .iter()
                .zip(&ranks)
                .map(|(node, &score)| NodeScore {
                    node_id: node.0,
                    score,
                }),
        );
        Ok(Iterated {
            output: AlgorithmOutput::PageRank(PageRankOutput {
                scores,
                iterations,
                residual,
                converged: stop.is_none(),
            }),
            cut,
            stop,
        })
    }

    /// The PageRank weight of retained edge `id`: a valid exact weight (see
    /// `classify`) as an f64, -0.0 read as 0.
    fn rank_weight(&self, id: EdgeId, field: &str) -> Result<f64, Halt> {
        let stored = self.graph.edge(id).ok_or(GraphError::InvariantViolation(
            "a projected edge is missing",
        ))?;
        let value = stored
            .properties
            .get(field)
            .ok_or(AlgorithmError::MissingWeight(id))?;
        let invalid = |reason| AlgorithmError::InvalidWeight { edge: id, reason };
        classify(value).map_err(invalid)?;
        match value {
            PropertyValue::Integer(value) => Ok(*value as f64),
            PropertyValue::Float(value) => Ok(value.abs()),
            _ => Err(invalid("weights must be integers or floats").into()),
        }
    }
}

/// Label propagation over the undirected simple graph of `projection`.
///
/// Setup collects each non-loop edge as two neighbor entries, sorts them
/// and drops repeats (parallel edges collapse), then offsets them by node.
/// Labels are node positions, so the smallest label is the smallest node
/// ID. A sweep visits nodes ascending; a node reads its smaller neighbors'
/// labels from this sweep and the others' from the last one, counts them,
/// and keeps its label when it ties for the most frequent (or it has no
/// neighbors), else takes the smallest most frequent label.
pub(super) fn label_propagation(
    projection: &Projection,
    max_iterations: u64,
    max_output_nodes: u64,
    meter: &mut Meter<'_>,
) -> Result<Iterated, Halt> {
    let (nodes, edges) = (&projection.nodes, &projection.edges);
    let count = nodes.len();
    let capacity = edges.len().saturating_mul(2);
    meter.reserve(capacity, size_of::<(usize, usize)>())?;
    let mut pairs = Vec::with_capacity(capacity);
    for edge in edges {
        meter.edge()?;
        if edge.source != edge.destination {
            pairs.push((edge.source, edge.destination));
            pairs.push((edge.destination, edge.source));
        }
    }
    meter.vertices(sort_cost(pairs.len()))?;
    pairs.sort_unstable();
    for _ in 0..pairs.len() {
        meter.edge()?;
    }
    pairs.dedup();
    meter.reserve(count + 1, size_of::<usize>())?;
    meter.vertices(count as u64 + 1)?;
    let mut offsets = vec![0usize; count + 1];
    for &(node, _) in &pairs {
        meter.edge()?;
        offsets[node + 1] += 1;
    }
    meter.vertices(count as u64)?;
    for at in 0..count {
        offsets[at + 1] += offsets[at];
    }

    meter.reserve(count.saturating_mul(2), size_of::<usize>())?;
    meter.vertices(2 * count as u64)?;
    let mut previous: Vec<usize> = (0..count).collect();
    let mut current = previous.clone();
    meter.reserve(count, size_of::<(u64, u64)>())?;
    meter.vertices(count as u64)?;
    let mut votes = vec![(0u64, 0u64); count];
    let (rows, cut) = kept_rows(count, max_output_nodes);
    meter.reserve(rows, size_of::<CommunityAssignment>())?;
    meter.vertices(2 * count as u64 + rows as u64)?;
    let mut assignments = Vec::with_capacity(rows);

    let mut attempt = 0u64;
    let (sweeps, stop) = iterate(max_iterations, meter, |meter| {
        attempt += 1;
        let mut unchanged = true;
        for node in 0..count {
            meter.vertex()?;
            let stamp = attempt * count as u64 + node as u64 + 1;
            let own = previous[node];
            let (mut best, mut most) = (own, 0);
            for &(_, other) in &pairs[offsets[node]..offsets[node + 1]] {
                meter.edge()?;
                let label = if other < node {
                    current[other]
                } else {
                    previous[other]
                };
                let vote = &mut votes[label];
                if vote.0 != stamp {
                    *vote = (stamp, 0);
                }
                vote.1 += 1;
                if vote.1 > most || (vote.1 == most && label < best) {
                    (best, most) = (label, vote.1);
                }
            }
            let held = if votes[own].0 == stamp {
                votes[own].1
            } else {
                0
            };
            let label = if most == 0 || held == most { own } else { best };
            unchanged &= label == own;
            current[node] = label;
        }
        std::mem::swap(&mut previous, &mut current);
        Ok(unchanged)
    })?;
    // The first position of every label of the last full sweep.
    let first = &mut current;
    first.fill(usize::MAX);
    for (node, &label) in previous.iter().enumerate() {
        if first[label] == usize::MAX {
            first[label] = node;
        }
    }
    assignments.extend(nodes[..rows].iter().zip(&previous).map(|(node, &label)| {
        CommunityAssignment {
            node_id: node.0,
            community_id: nodes[first[label]].0,
        }
    }));
    Ok(Iterated {
        output: AlgorithmOutput::LabelPropagation(LabelPropagationOutput {
            assignments,
            sweeps,
            converged: stop.is_none(),
        }),
        cut,
        stop,
    })
}
