use super::super::error::GraphError;
use super::super::limits::GraphLimits;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};

const DEFAULT_BINDING_ROWS: u64 = 1_000;
const MAX_BINDING_ROWS: u64 = 10_000;
const DEFAULT_FRONTIER_ENTRIES: u64 = 1_000;
const MAX_FRONTIER_ENTRIES: u64 = 10_000;
const DEFAULT_RETURNED_ROWS: u64 = 100;
const MAX_RETURNED_ROWS: u64 = 1_000;
const DEFAULT_WORK_UNITS: u64 = 100_000;
const MAX_WORK_UNITS: u64 = 1_000_000;
const DEFAULT_SCRATCH_BYTES: u64 = 32 * 1024 * 1024;
const MAX_SCRATCH_BYTES: u64 = 128 * 1024 * 1024;
const DEFAULT_WRITE_DEADLINE_MS: u64 = 100;
const MAX_WRITE_DEADLINE_MS: u64 = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReadBounds {
    pub(crate) graph: GraphLimits,
    pub(crate) max_binding_rows: u64,
    pub(crate) max_frontier_entries: u64,
    pub(crate) max_returned_rows: u64,
    pub(crate) max_work_units: u64,
    pub(crate) max_scratch_bytes: u64,
    pub(crate) write_deadline_ms: u64,
}

impl ReadBounds {
    pub(crate) const DEFAULT: Self = Self {
        graph: GraphLimits::DEFAULT,
        max_binding_rows: DEFAULT_BINDING_ROWS,
        max_frontier_entries: DEFAULT_FRONTIER_ENTRIES,
        max_returned_rows: DEFAULT_RETURNED_ROWS,
        max_work_units: DEFAULT_WORK_UNITS,
        max_scratch_bytes: DEFAULT_SCRATCH_BYTES,
        write_deadline_ms: DEFAULT_WRITE_DEADLINE_MS,
    };

    pub(crate) fn validate(self) -> Result<Self, GraphError> {
        let limits = self.graph;
        let ceilings = GraphLimits::CEILING;
        for (limit, requested, ceiling) in [
            ("max_hops", limits.max_hops, ceilings.max_hops),
            (
                "max_visited_nodes",
                limits.max_visited_nodes,
                ceilings.max_visited_nodes,
            ),
            (
                "max_edges_examined",
                limits.max_edges_examined,
                ceilings.max_edges_examined,
            ),
            (
                "max_edges_per_node",
                limits.max_edges_per_node,
                ceilings.max_edges_per_node,
            ),
            ("deadline_ms", limits.deadline_ms, ceilings.deadline_ms),
            (
                "max_output_nodes",
                limits.max_output_nodes,
                ceilings.max_output_nodes,
            ),
            (
                "max_output_edges",
                limits.max_output_edges,
                ceilings.max_output_edges,
            ),
            (
                "max_output_paths",
                limits.max_output_paths,
                ceilings.max_output_paths,
            ),
            ("max_binding_rows", self.max_binding_rows, MAX_BINDING_ROWS),
            (
                "max_frontier_entries",
                self.max_frontier_entries,
                MAX_FRONTIER_ENTRIES,
            ),
            (
                "max_returned_rows",
                self.max_returned_rows,
                MAX_RETURNED_ROWS,
            ),
            ("max_work_units", self.max_work_units, MAX_WORK_UNITS),
            (
                "max_scratch_bytes",
                self.max_scratch_bytes,
                MAX_SCRATCH_BYTES,
            ),
            (
                "write_deadline_ms",
                self.write_deadline_ms,
                MAX_WRITE_DEADLINE_MS,
            ),
        ] {
            if requested > ceiling {
                return Err(GraphError::LimitExceedsCeiling {
                    limit,
                    requested,
                    ceiling,
                });
            }
        }
        if [
            limits.max_visited_nodes,
            limits.max_edges_examined,
            limits.max_edges_per_node,
            limits.deadline_ms,
            self.max_binding_rows,
            self.max_frontier_entries,
            self.max_work_units,
            self.max_scratch_bytes,
            self.write_deadline_ms,
        ]
        .contains(&0)
        {
            return Err(GraphError::InvalidQuery(
                "subset work limits must be positive",
            ));
        }
        Ok(self)
    }

    pub(crate) fn with_overrides(mut self, values: [Option<u64>; 14]) -> Result<Self, GraphError> {
        self.graph.max_hops = values[0].unwrap_or(self.graph.max_hops);
        self.graph.max_visited_nodes = values[1].unwrap_or(self.graph.max_visited_nodes);
        self.graph.max_edges_examined = values[2].unwrap_or(self.graph.max_edges_examined);
        self.graph.max_edges_per_node = values[3].unwrap_or(self.graph.max_edges_per_node);
        self.graph.deadline_ms = values[4].unwrap_or(self.graph.deadline_ms);
        self.graph.max_output_nodes = values[5].unwrap_or(self.graph.max_output_nodes);
        self.graph.max_output_edges = values[6].unwrap_or(self.graph.max_output_edges);
        self.graph.max_output_paths = values[7].unwrap_or(self.graph.max_output_paths);
        self.max_binding_rows = values[8].unwrap_or(self.max_binding_rows);
        self.max_frontier_entries = values[9].unwrap_or(self.max_frontier_entries);
        self.max_returned_rows = values[10].unwrap_or(self.max_returned_rows);
        self.max_work_units = values[11].unwrap_or(self.max_work_units);
        self.max_scratch_bytes = values[12].unwrap_or(self.max_scratch_bytes);
        self.write_deadline_ms = values[13].unwrap_or(self.write_deadline_ms);
        self.validate()
    }
}

impl Default for ReadBounds {
    fn default() -> Self {
        Self::DEFAULT
    }
}

pub(crate) trait MeterClock {
    fn elapsed_ms(&self, phase: &'static str, work: u64) -> u64;
    fn cancelled(&self, phase: &'static str, work: u64) -> bool;
}

pub(crate) struct SystemClock<'a, F> {
    elapsed: F,
    started: u64,
    cancel: Option<&'a AtomicBool>,
}

impl<'a, F: Fn() -> u64> SystemClock<'a, F> {
    pub(crate) fn new(cancel: Option<&'a AtomicBool>, elapsed: F) -> Self {
        let started = elapsed();
        Self {
            elapsed,
            started,
            cancel,
        }
    }
}

impl<F: Fn() -> u64> MeterClock for SystemClock<'_, F> {
    fn elapsed_ms(&self, _phase: &'static str, _work: u64) -> u64 {
        (self.elapsed)().saturating_sub(self.started)
    }

    fn cancelled(&self, _phase: &'static str, _work: u64) -> bool {
        self.cancel.is_some_and(|flag| flag.load(Ordering::Relaxed))
    }
}

pub(crate) struct Meter<'a, C> {
    pub(crate) bounds: ReadBounds,
    pub(crate) clock: &'a C,
    pub(crate) work: u64,
    scratch: u64,
    edges: u64,
    visited: BTreeSet<u64>,
}

impl<'a, C: MeterClock> Meter<'a, C> {
    pub(crate) fn new(bounds: ReadBounds, clock: &'a C) -> Result<Self, GraphError> {
        let meter = Self {
            bounds,
            clock,
            work: 0,
            scratch: 0,
            edges: 0,
            visited: BTreeSet::new(),
        };
        meter.check("before")?;
        Ok(meter)
    }

    pub(crate) fn check(&self, phase: &'static str) -> Result<(), GraphError> {
        if self.clock.cancelled(phase, self.work) {
            return Err(GraphError::InvalidQuery("subset cancelled"));
        }
        if self.clock.elapsed_ms(phase, self.work) >= self.bounds.graph.deadline_ms {
            return Err(GraphError::InvalidQuery("subset deadline exceeded"));
        }
        Ok(())
    }

    pub(crate) fn charge(&mut self) -> Result<(), GraphError> {
        self.work = self.work.saturating_add(1);
        if self.work > self.bounds.max_work_units {
            return Err(GraphError::InvalidQuery("subset work budget exhausted"));
        }
        if self.work.is_multiple_of(64) {
            self.check("interval")?;
        }
        Ok(())
    }

    pub(crate) fn allocate(&mut self, bytes: u64) -> Result<(), GraphError> {
        self.scratch = self.scratch.saturating_add(bytes);
        if self.scratch > self.bounds.max_scratch_bytes {
            return Err(GraphError::InvalidQuery("subset scratch budget exhausted"));
        }
        Ok(())
    }

    pub(crate) fn visit(&mut self, node_id: u64) -> Result<(), GraphError> {
        self.visited.insert(node_id);
        if self.visited.len() as u64 > self.bounds.graph.max_visited_nodes {
            return Err(GraphError::InvalidQuery(
                "subset visited-node budget exhausted",
            ));
        }
        Ok(())
    }

    pub(crate) fn examine(&mut self, degree: u64) -> Result<(), GraphError> {
        self.charge()?;
        self.edges = self.edges.saturating_add(1);
        if self.edges > self.bounds.graph.max_edges_examined {
            return Err(GraphError::InvalidQuery("subset edge budget exhausted"));
        }
        if degree > self.bounds.graph.max_edges_per_node {
            return Err(GraphError::InvalidQuery("subset degree budget exhausted"));
        }
        Ok(())
    }

    pub(crate) fn frontier_insert(&mut self, entries: usize) -> Result<(), GraphError> {
        self.charge()?;
        if entries as u64 > self.bounds.max_frontier_entries {
            return Err(GraphError::InvalidQuery("subset frontier budget exhausted"));
        }
        Ok(())
    }

    pub(crate) fn binding_insert(&self, rows: usize) -> Result<(), GraphError> {
        if rows as u64 > self.bounds.max_binding_rows {
            return Err(GraphError::InvalidQuery(
                "subset binding-row budget exhausted",
            ));
        }
        Ok(())
    }

    pub(crate) fn returned(&self, rows: usize) -> Result<(), GraphError> {
        if rows as u64 > self.bounds.max_returned_rows {
            return Err(GraphError::InvalidQuery(
                "subset result-row budget exhausted",
            ));
        }
        Ok(())
    }

    pub(crate) fn output(
        &self,
        nodes: usize,
        edges: usize,
        paths: usize,
    ) -> Result<(), GraphError> {
        let limits = self.bounds.graph;
        if nodes as u64 > limits.max_output_nodes
            || edges as u64 > limits.max_output_edges
            || paths as u64 > limits.max_output_paths
        {
            return Err(GraphError::InvalidQuery("subset output budget exhausted"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoTime;

    impl MeterClock for NoTime {
        fn elapsed_ms(&self, _phase: &'static str, _work: u64) -> u64 {
            0
        }

        fn cancelled(&self, _phase: &'static str, _work: u64) -> bool {
            false
        }
    }

    #[test]
    fn bounds_validate_ceilings_before_zero_work_limits() {
        let bounds = ReadBounds::DEFAULT
            .with_overrides([
                Some(5),
                None,
                None,
                None,
                Some(0),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ])
            .unwrap_err();
        assert!(matches!(
            bounds,
            GraphError::LimitExceedsCeiling {
                limit: "max_hops",
                ..
            }
        ));

        let bounds = ReadBounds::DEFAULT
            .with_overrides([
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(MAX_SCRATCH_BYTES + 1),
                None,
            ])
            .unwrap_err();
        assert!(matches!(
            bounds,
            GraphError::LimitExceedsCeiling {
                limit: "max_scratch_bytes",
                ..
            }
        ));

        assert_eq!(
            ReadBounds::DEFAULT
                .with_overrides([
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(0),
                    None,
                    None,
                    None,
                    None,
                    None
                ])
                .unwrap_err(),
            GraphError::InvalidQuery("subset work limits must be positive")
        );
    }

    #[test]
    fn meter_stops_at_exact_budgets_without_partial_success() {
        let clock = NoTime;
        let bounds = ReadBounds::DEFAULT
            .with_overrides([
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(1),
                None,
                None,
            ])
            .unwrap();
        let mut meter = Meter::new(bounds, &clock).unwrap();
        meter.charge().unwrap();
        assert_eq!(
            meter.charge(),
            Err(GraphError::InvalidQuery("subset work budget exhausted"))
        );
    }

    #[test]
    fn every_meter_cap_accepts_minus_one_and_cap_then_rejects_plus_one() {
        type Action = fn(&mut Meter<'_, NoTime>, usize) -> Result<(), GraphError>;
        let checks: [(&str, Action); 11] = [
            ("subset visited-node budget exhausted", |meter, n| {
                meter.visit(n as u64)
            }),
            ("subset edge budget exhausted", |meter, _| meter.examine(1)),
            ("subset degree budget exhausted", |meter, n| {
                meter.examine(n as u64)
            }),
            ("subset frontier budget exhausted", |meter, n| {
                meter.frontier_insert(n)
            }),
            ("subset binding-row budget exhausted", |meter, n| {
                meter.binding_insert(n)
            }),
            ("subset result-row budget exhausted", |meter, n| {
                meter.returned(n)
            }),
            ("subset work budget exhausted", |meter, _| meter.charge()),
            ("subset scratch budget exhausted", |meter, _| {
                meter.allocate(1)
            }),
            ("subset output budget exhausted", |meter, n| {
                meter.output(n, 0, 0)
            }),
            ("subset output budget exhausted", |meter, n| {
                meter.output(0, n, 0)
            }),
            ("subset output budget exhausted", |meter, n| {
                meter.output(0, 0, n)
            }),
        ];
        let clock = NoTime;
        for (index, (reason, action)) in checks.into_iter().enumerate() {
            let mut bounds = ReadBounds::DEFAULT;
            match index {
                0 => bounds.graph.max_visited_nodes = 2,
                1 => bounds.graph.max_edges_examined = 2,
                2 => bounds.graph.max_edges_per_node = 2,
                3 => bounds.max_frontier_entries = 2,
                4 => bounds.max_binding_rows = 2,
                5 => bounds.max_returned_rows = 2,
                6 => bounds.max_work_units = 2,
                7 => bounds.max_scratch_bytes = 2,
                8 => bounds.graph.max_output_nodes = 2,
                9 => bounds.graph.max_output_edges = 2,
                10 => bounds.graph.max_output_paths = 2,
                _ => unreachable!(),
            }
            let mut meter = Meter::new(bounds, &clock).unwrap();
            assert_eq!(action(&mut meter, 1), Ok(()), "{index}: cap-1");
            assert_eq!(action(&mut meter, 2), Ok(()), "{index}: cap");
            assert_eq!(
                action(&mut meter, 3),
                Err(GraphError::InvalidQuery(reason)),
                "{index}: cap+1"
            );
        }
    }

    #[test]
    fn every_caller_ceiling_is_exact_and_never_clamped() {
        let graph = GraphLimits::CEILING;
        let ceilings = [
            graph.max_hops,
            graph.max_visited_nodes,
            graph.max_edges_examined,
            graph.max_edges_per_node,
            graph.deadline_ms,
            graph.max_output_nodes,
            graph.max_output_edges,
            graph.max_output_paths,
            MAX_BINDING_ROWS,
            MAX_FRONTIER_ENTRIES,
            MAX_RETURNED_ROWS,
            MAX_WORK_UNITS,
            MAX_SCRATCH_BYTES,
            MAX_WRITE_DEADLINE_MS,
        ];
        for (index, ceiling) in ceilings.into_iter().enumerate() {
            for requested in [ceiling - 1, ceiling, ceiling + 1] {
                let mut values = [None; 14];
                values[index] = Some(requested);
                let result = ReadBounds::DEFAULT.with_overrides(values);
                if requested > ceiling {
                    assert!(
                        matches!(result, Err(GraphError::LimitExceedsCeiling { requested: actual, .. }) if actual == requested)
                    );
                } else {
                    assert!(result.is_ok(), "{index}: {requested}");
                }
            }
        }
    }

    #[test]
    fn scratch_larger_than_one_byte_and_frontier_stop_fail_closed() {
        let clock = NoTime;
        let mut bounds = ReadBounds::DEFAULT;
        bounds.max_scratch_bytes = 256;
        bounds.max_frontier_entries = 2;
        let mut meter = Meter::new(bounds, &clock).unwrap();
        meter.allocate(255).unwrap();
        meter.allocate(1).unwrap();
        assert_eq!(
            meter.allocate(1),
            Err(GraphError::InvalidQuery("subset scratch budget exhausted"))
        );
        assert_eq!(
            meter.frontier_insert(3),
            Err(GraphError::InvalidQuery("subset frontier budget exhausted"))
        );
    }

    #[test]
    fn system_clock_uses_only_the_injected_clock_and_cancellation_flag() {
        let cancel = AtomicBool::new(false);
        let value = std::cell::Cell::new(1);
        let clock = SystemClock::new(Some(&cancel), || value.get());
        assert_eq!(clock.elapsed_ms("before", 0), 0);
        value.set(2);
        assert_eq!(clock.elapsed_ms("sort", 64), 1);
        assert!(!clock.cancelled("before", 0));
        cancel.store(true, Ordering::Relaxed);
        assert!(clock.cancelled("interval", 64));
    }

    #[test]
    fn deadline_is_exclusive_at_the_exact_cap() {
        let value = std::cell::Cell::new(0);
        let clock = SystemClock::new(None, || value.get());
        let mut bounds = ReadBounds::DEFAULT;
        bounds.graph.deadline_ms = 2;
        let meter = Meter::new(bounds, &clock).unwrap();
        value.set(1);
        assert_eq!(meter.check("sort"), Ok(()));
        for elapsed in [2, 3] {
            value.set(elapsed);
            assert_eq!(
                meter.check("sort"),
                Err(GraphError::InvalidQuery("subset deadline exceeded"))
            );
        }
    }
}
