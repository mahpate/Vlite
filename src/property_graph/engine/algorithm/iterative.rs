//! Iterative algorithms over an extracted projection (Gap 5 S3): PageRank
//! by synchronous full steps and label propagation by in-place sweeps.
//!
//! Both reserve every buffer before the first step, so a step never
//! allocates, and charge every pass as it runs: at most 64 operations run
//! between deadline and cancellation checks. A step stages its result rows
//! (and label propagation its canonical community IDs) in the same charged
//! per-node operations into a candidate buffer that it commits only when it
//! finishes. A step interrupted by a budget, the deadline or cancellation
//! is discarded: the result keeps the last full step's staged rows, or
//! nothing before the first one, and no work follows the step.

use super::{
    AlgorithmError, AlgorithmOutput, AlgorithmTruncation, CommunityAssignment, Direction, EdgeId,
    Graph, GraphError, Halt, LabelPropagationOutput, Local, Meter, NodeId, NodeScore,
    PageRankOutput, Projection, PropertyValue, Seed, classify, orient, resolve, sort_cost,
};
use std::mem::size_of;

#[cfg(test)]
thread_local! {
    /// Staging work done on this thread: one per charged unit that writes
    /// a result row or a canonical label (tests only).
    pub(super) static STAGED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Sort work done on this thread: one per element comparison or move
    /// (tests only).
    pub(super) static SORTED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Records one unit of staging work (a no-op outside tests).
fn staged() {
    #[cfg(test)]
    STAGED.with(|count| count.set(count.get() + 1));
}

/// Records one unit of sort work (a no-op outside tests).
fn sorted() {
    #[cfg(test)]
    SORTED.with(|count| count.set(count.get() + 1));
}

/// Sorts `items` ascending by `less`, stably (ties keep their order), by a
/// bottom-up merge sort. A buffer of `items.len()` items is reserved before
/// it is allocated. Each pass moves every item once, with at most one
/// comparison, for one vertex unit; the `sort_cost` units beyond the
/// `ceil(log2 n)` passes are charged after them, so a sort costs exactly
/// `sort_cost(n)` units, each checked as it runs.
pub(super) fn sort_by<T: Copy>(
    items: &mut Vec<T>,
    less: impl Fn(&T, &T) -> bool,
    meter: &mut Meter<'_>,
) -> Result<(), AlgorithmTruncation> {
    let count = items.len();
    meter.reserve(count, size_of::<T>())?;
    let mut buffer = Vec::with_capacity(count);
    let mut moved = 0u64;
    let mut width = 1usize;
    while width < count {
        buffer.clear();
        let mut low = 0;
        while low < count {
            let middle = low + width.min(count - low);
            let high = middle + width.min(count - middle);
            let (mut left, mut right) = (low, middle);
            while left < middle || right < high {
                meter.vertex()?;
                sorted();
                if right < high && (left == middle || less(&items[right], &items[left])) {
                    buffer.push(items[right]);
                    right += 1;
                } else {
                    buffer.push(items[left]);
                    left += 1;
                }
                moved += 1;
            }
            low = high;
        }
        std::mem::swap(items, &mut buffer);
        width = width.saturating_mul(2);
    }
    meter.vertices(sort_cost(count) - moved)
}

/// `count` copies of `value`, one vertex unit each as it is written.
fn filled<T: Clone>(
    count: usize,
    value: T,
    meter: &mut Meter<'_>,
) -> Result<Vec<T>, AlgorithmTruncation> {
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        meter.vertex()?;
        items.push(value.clone());
    }
    Ok(items)
}

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
    sort_by(&mut masses, |a, b| a.0 < b.0, meter)?;
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
    /// change ascending, staging the node's row in the same unit.
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
        let mut weights = Vec::with_capacity(edges.len());
        let mut sums = filled(count, 0.0f64, meter)?;
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
        let teleport = match rank.teleport {
            None => filled(count, 1.0 / count as f64, meter)?,
            Some(teleport) => {
                let mut values = filled(count, 0.0, meter)?;
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
        let mut ranks = Vec::with_capacity(count);
        for &jump in &teleport {
            meter.vertex()?;
            ranks.push(jump);
        }
        meter.reserve(count, size_of::<f64>())?;
        let mut next = filled(count, 0.0f64, meter)?;
        // The committed rows and the candidate rows of the running step.
        let (rows, cut) = kept_rows(count, self.limits.max_output_nodes);
        meter.reserve(rows.saturating_mul(2), size_of::<NodeScore>())?;
        let mut scores = Vec::with_capacity(rows);
        let mut candidate = Vec::with_capacity(rows);

        let damping = rank.damping;
        let mut residual = 0.0;
        let (iterations, stop) = iterate(rank.max_iterations, meter, |meter| {
            candidate.clear();
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
            // A node's unit also stages its row.
            let mut change = 0.0;
            for (at, ((slot, &old), &jump)) in
                next.iter_mut().zip(&ranks).zip(&teleport).enumerate()
            {
                meter.vertex()?;
                let value = (1.0 - damping) * jump + damping * (*slot + dangling * jump);
                change += (value - old).abs();
                *slot = value;
                if at < rows {
                    staged();
                    candidate.push(NodeScore {
                        node_id: nodes[at].0,
                        score: value,
                    });
                }
            }
            std::mem::swap(&mut ranks, &mut next);
            std::mem::swap(&mut scores, &mut candidate);
            residual = change;
            Ok(change <= rank.tolerance)
        })?;
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
/// neighbors), else takes the smallest most frequent label; the same unit
/// stages its row with the smallest node ID holding that label.
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
    sort_by(&mut pairs, |a, b| a < b, meter)?;
    let mut kept = 0;
    for at in 0..pairs.len() {
        meter.edge()?;
        if kept == 0 || pairs[at] != pairs[kept - 1] {
            pairs[kept] = pairs[at];
            kept += 1;
        }
    }
    pairs.truncate(kept);
    meter.reserve(count + 1, size_of::<usize>())?;
    let mut offsets = filled(count + 1, 0usize, meter)?;
    for &(node, _) in &pairs {
        meter.edge()?;
        offsets[node + 1] += 1;
    }
    for at in 0..count {
        meter.vertex()?;
        offsets[at + 1] += offsets[at];
    }

    meter.reserve(count.saturating_mul(2), size_of::<usize>())?;
    let mut previous = Vec::with_capacity(count);
    for node in 0..count {
        meter.vertex()?;
        previous.push(node);
    }
    let mut current = filled(count, 0usize, meter)?;
    meter.reserve(count, size_of::<(u64, u64)>())?;
    let mut votes = filled(count, (0u64, 0u64), meter)?;
    // The sweep that last saw each label and its first position then.
    meter.reserve(count, size_of::<(u64, usize)>())?;
    let mut firsts = filled(count, (0u64, 0usize), meter)?;
    // The committed rows and the candidate rows of the running sweep.
    let (rows, cut) = kept_rows(count, max_output_nodes);
    meter.reserve(rows.saturating_mul(2), size_of::<CommunityAssignment>())?;
    let mut assignments = Vec::with_capacity(rows);
    let mut candidate = Vec::with_capacity(rows);

    let mut attempt = 0u64;
    let (sweeps, stop) = iterate(max_iterations, meter, |meter| {
        attempt += 1;
        candidate.clear();
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
            // The node's unit also canonicalizes its label: nodes run
            // ascending, so a label's first position this sweep is final
            // once any node holding it is reached.
            staged();
            let first = &mut firsts[label];
            if first.0 != attempt {
                *first = (attempt, node);
            }
            if node < rows {
                candidate.push(CommunityAssignment {
                    node_id: nodes[node].0,
                    community_id: nodes[first.1].0,
                });
            }
        }
        std::mem::swap(&mut previous, &mut current);
        std::mem::swap(&mut assignments, &mut candidate);
        Ok(unchanged)
    })?;
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
