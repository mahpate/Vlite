//! Model, batch and read limits.
//!
//! Values marked PD-xxx are proposed defaults from the PRD (Section 4.9) and
//! are not user-approved guarantees. Values mirroring `vlite` constants are
//! duplicated here because this crate must not depend on `vlite`; the adapter
//! is expected to assert their equality once it exists.

use super::error::GraphError;

/// PD-002: maximum named graphs per file. Graph IDs are `0..MAX_GRAPHS`
/// (DO-003 body header: `graph_id < 64`).
pub const MAX_GRAPHS: u16 = 64;
/// REQ-005: graph names follow collection name rules, `^[a-z][a-z0-9_-]{0,62}$`.
pub const MAX_GRAPH_NAME_BYTES: usize = 63;
/// PD-001: node keys and edge keys are 1..=256 UTF-8 bytes.
pub const MAX_KEY_BYTES: usize = 256;
/// PD-003: labels per node.
pub const MAX_LABELS: usize = 8;
/// PD-003: label length, `[A-Za-z0-9_]{1,63}`.
pub const MAX_LABEL_BYTES: usize = 63;
/// REQ-007: relationship type length, `[A-Za-z0-9_]{1,63}`.
pub const MAX_RELATIONSHIP_TYPE_BYTES: usize = 63;
/// Mirrors `vlite::MAX_METADATA_FIELDS`.
pub const MAX_PROPERTY_FIELDS: usize = 64;
/// Mirrors `vlite::MAX_METADATA_BYTES` (encoded size, including the count).
pub const MAX_PROPERTY_BYTES: usize = 65_536;
/// Mirrors the metadata field-name rule (1..=256 UTF-8 bytes).
pub const MAX_FIELD_NAME_BYTES: usize = 256;
/// Mirrors the metadata string-value rule (at most 4096 UTF-8 bytes).
pub const MAX_STRING_VALUE_BYTES: usize = 4_096;
/// REQ-010: mirrors `vlite::MAX_BATCH_OPERATIONS`; counts every entry of one
/// graph frame, including vector mutations.
pub const MAX_BATCH_ENTRIES: usize = 4_096;
/// REQ-010: mirrors `vlite::MAX_BATCH_BYTES`; bounds the encoded graph body.
pub const MAX_BATCH_BYTES: usize = 16_777_216;
/// Mirrors `vlite::MAX_FILTER_CONDITIONS`.
pub const MAX_CONDITIONS: usize = 64;
/// PD-010: deadline and cancellation are checked once before a read does any
/// work and again after every this many budget units (adjacency entries
/// examined, or nodes scanned by `find_nodes`).
pub const CHECK_INTERVAL: u64 = 64;
/// At most this many node IDs are listed by one `DegreeLimit` reason.
pub const MAX_DEGREE_LIMIT_NODE_IDS: usize = 64;

/// REQ-024: limits of one read (PD-004..PD-009).
///
/// Values above [`GraphLimits::CEILING`] are rejected with
/// `GraphError::LimitExceedsCeiling`, never clamped. `max_hops` and the
/// output caps may be 0 (start nodes only; return no items of that kind).
/// The work budgets (`max_visited_nodes`, `max_edges_examined`,
/// `max_edges_per_node`, `deadline_ms`) must be at least 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphLimits {
    /// PD-004: hops from the start nodes. A query parameter, never a truncation.
    pub max_hops: u64,
    /// PD-005: distinct nodes a traversal may visit, start nodes included.
    pub max_visited_nodes: u64,
    /// PD-006: adjacency entries examined, or nodes scanned by `find_nodes`.
    pub max_edges_examined: u64,
    /// PD-009: adjacency entries examined per node expansion.
    pub max_edges_per_node: u64,
    /// PD-008: milliseconds of the injected clock.
    pub deadline_ms: u64,
    /// PD-007: output caps. They never stop exploration.
    pub max_output_nodes: u64,
    pub max_output_edges: u64,
    pub max_output_paths: u64,
}

impl GraphLimits {
    pub const DEFAULT: Self = Self {
        max_hops: 2,
        max_visited_nodes: 1_000,
        max_edges_examined: 10_000,
        max_edges_per_node: 1_000,
        deadline_ms: 250,
        max_output_nodes: 100,
        max_output_edges: 200,
        max_output_paths: 20,
    };

    pub const CEILING: Self = Self {
        max_hops: 4,
        max_visited_nodes: 10_000,
        max_edges_examined: 100_000,
        max_edges_per_node: 10_000,
        deadline_ms: 5_000,
        max_output_nodes: 1_000,
        max_output_edges: 2_000,
        max_output_paths: 100,
    };

    fn fields(&self) -> [(&'static str, u64); 8] {
        [
            ("max_hops", self.max_hops),
            ("max_visited_nodes", self.max_visited_nodes),
            ("max_edges_examined", self.max_edges_examined),
            ("max_edges_per_node", self.max_edges_per_node),
            ("deadline_ms", self.deadline_ms),
            ("max_output_nodes", self.max_output_nodes),
            ("max_output_edges", self.max_output_edges),
            ("max_output_paths", self.max_output_paths),
        ]
    }

    /// Rejects the first field (in declaration order) above its ceiling, then
    /// zero work budgets. Crate-private: `vlite` re-exports `GraphLimits`
    /// publicly, and every read validates its limits itself.
    pub(crate) fn validate(&self) -> Result<(), GraphError> {
        for ((limit, requested), (_, ceiling)) in
            self.fields().into_iter().zip(Self::CEILING.fields())
        {
            if requested > ceiling {
                return Err(GraphError::LimitExceedsCeiling {
                    limit,
                    requested,
                    ceiling,
                });
            }
        }
        for (value, reason) in [
            (
                self.max_visited_nodes,
                "max_visited_nodes must be at least 1",
            ),
            (
                self.max_edges_examined,
                "max_edges_examined must be at least 1",
            ),
            (
                self.max_edges_per_node,
                "max_edges_per_node must be at least 1",
            ),
            (self.deadline_ms, "deadline_ms must be at least 1"),
        ] {
            if value == 0 {
                return Err(GraphError::InvalidQuery(reason));
            }
        }
        Ok(())
    }
}

impl Default for GraphLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_ceilings_are_the_proposed_values() {
        assert_eq!(GraphLimits::default(), GraphLimits::DEFAULT);
        let defaults = GraphLimits::DEFAULT.fields();
        let ceilings = GraphLimits::CEILING.fields();
        let expected = [
            ("max_hops", 2, 4),
            ("max_visited_nodes", 1_000, 10_000),
            ("max_edges_examined", 10_000, 100_000),
            ("max_edges_per_node", 1_000, 10_000),
            ("deadline_ms", 250, 5_000),
            ("max_output_nodes", 100, 1_000),
            ("max_output_edges", 200, 2_000),
            ("max_output_paths", 20, 100),
        ];
        for (index, (name, default, ceiling)) in expected.into_iter().enumerate() {
            assert_eq!(defaults[index], (name, default));
            assert_eq!(ceilings[index], (name, ceiling));
        }
        assert_eq!(GraphLimits::DEFAULT.validate(), Ok(()));
        assert_eq!(GraphLimits::CEILING.validate(), Ok(()));
    }

    fn with(index: usize, value: u64) -> GraphLimits {
        let mut limits = GraphLimits::DEFAULT;
        let field = [
            &mut limits.max_hops,
            &mut limits.max_visited_nodes,
            &mut limits.max_edges_examined,
            &mut limits.max_edges_per_node,
            &mut limits.deadline_ms,
            &mut limits.max_output_nodes,
            &mut limits.max_output_edges,
            &mut limits.max_output_paths,
        ]
        .into_iter()
        .nth(index)
        .unwrap();
        *field = value;
        limits
    }

    #[test]
    fn every_limit_is_accepted_at_its_ceiling_and_rejected_above_it() {
        for (index, (name, ceiling)) in GraphLimits::CEILING.fields().into_iter().enumerate() {
            assert_eq!(with(index, ceiling).validate(), Ok(()), "{name}");
            for requested in [ceiling + 1, u64::MAX] {
                assert_eq!(
                    with(index, requested).validate(),
                    Err(GraphError::LimitExceedsCeiling {
                        limit: name,
                        requested,
                        ceiling,
                    }),
                    "{name}"
                );
            }
        }
        // The first offending field in declaration order is reported.
        let mut limits = GraphLimits::CEILING;
        limits.max_output_paths = 101;
        limits.max_hops = 5;
        assert!(matches!(
            limits.validate(),
            Err(GraphError::LimitExceedsCeiling {
                limit: "max_hops",
                ..
            })
        ));
        // A value above its ceiling is reported before a zero work budget.
        limits = GraphLimits::DEFAULT;
        limits.deadline_ms = 0;
        limits.max_output_nodes = 1_001;
        assert!(matches!(
            limits.validate(),
            Err(GraphError::LimitExceedsCeiling {
                limit: "max_output_nodes",
                ..
            })
        ));
    }

    #[test]
    fn zero_is_valid_for_hops_and_output_caps_but_not_for_work_budgets() {
        for (index, (name, _)) in GraphLimits::DEFAULT.fields().into_iter().enumerate() {
            let result = with(index, 0).validate();
            match name {
                "max_hops" | "max_output_nodes" | "max_output_edges" | "max_output_paths" => {
                    assert_eq!(result, Ok(()), "{name}")
                }
                _ => assert!(
                    matches!(result, Err(GraphError::InvalidQuery(reason)) if reason.starts_with(name)),
                    "{name}: {result:?}"
                ),
            }
        }
    }
}
