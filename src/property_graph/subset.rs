#![cfg_attr(not(test), allow(dead_code))]

use std::sync::atomic::AtomicBool;

use crate::{Error, Result};

use super::engine::GraphLimits;
use super::engine::subset::meter::{ReadBounds, SystemClock};
use super::engine::subset::plan::ParameterEntry;
use super::engine::subset::read::{ReadError, ReadResult};
use super::read::GraphTextError;
use super::{GraphRef, convert, engine};

const DEFAULT_WRITE_DEADLINE_MS: u64 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SubsetLimits {
    pub(crate) graph: GraphLimits,
    pub(crate) max_binding_rows: u64,
    pub(crate) max_frontier_entries: u64,
    pub(crate) max_returned_rows: u64,
    pub(crate) max_work_units: u64,
    pub(crate) max_scratch_bytes: u64,
    pub(crate) write_deadline_ms: u64,
}

impl SubsetLimits {
    pub(crate) const DEFAULT: Self = Self {
        graph: GraphLimits::DEFAULT,
        max_binding_rows: 1_000,
        max_frontier_entries: 1_000,
        max_returned_rows: 100,
        max_work_units: 100_000,
        max_scratch_bytes: 32 * 1024 * 1024,
        write_deadline_ms: DEFAULT_WRITE_DEADLINE_MS,
    };

    fn engine_bounds(self) -> ReadBounds {
        ReadBounds {
            graph: self.graph,
            max_binding_rows: self.max_binding_rows,
            max_frontier_entries: self.max_frontier_entries,
            max_returned_rows: self.max_returned_rows,
            max_work_units: self.max_work_units,
            max_scratch_bytes: self.max_scratch_bytes,
            write_deadline_ms: self.write_deadline_ms,
        }
    }

    pub(crate) fn validate(self) -> Result<Self> {
        self.engine_bounds()
            .validate()
            .map_err(convert::graph_error)?;
        Ok(self)
    }
}

impl Default for SubsetLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct SubsetReadError {
    pub(crate) error: GraphTextError,
    pub(crate) position: Option<usize>,
}

impl<'db> GraphRef<'db> {
    #[allow(dead_code)]
    pub(crate) fn read_subset(
        &self,
        text: &str,
        parameters: &[ParameterEntry],
        limits: SubsetLimits,
        cancel: Option<&AtomicBool>,
    ) -> std::result::Result<Option<ReadResult>, SubsetReadError> {
        let ttl_clock_ms = self.now();
        let temporal_capable = self.db.flags & crate::format::FLAG_TEMPORAL != 0;
        let clock = SystemClock::new(cancel, super::read::clock());
        engine::subset::read::execute(
            self.graph,
            self.view_sequence,
            temporal_capable,
            ttl_clock_ms,
            text,
            parameters,
            limits.engine_bounds(),
            &clock,
            |as_of, ttl_clock_ms, temporal_capable| {
                super::temporal::read::subset_as_of(as_of, ttl_clock_ms, temporal_capable).map_err(
                    |error| match error {
                        super::temporal::read::SubsetTimeError::UnsupportedFeature(feature) => {
                            ReadError::UnsupportedFeature(feature)
                        }
                        super::temporal::read::SubsetTimeError::InvalidQuery(reason) => {
                            ReadError::Runtime(reason)
                        }
                    },
                )
            },
        )
        .map_err(read_error)
    }
}

#[allow(dead_code)]
fn read_error(error: ReadError) -> SubsetReadError {
    match error {
        ReadError::Query(error) => SubsetReadError {
            error: GraphTextError::Query(error),
            position: None,
        },
        ReadError::Graph(error) => SubsetReadError {
            error: GraphTextError::Read(convert::graph_error(error)),
            position: None,
        },
        ReadError::Runtime(reason) => SubsetReadError {
            error: GraphTextError::Read(Error::InvalidQuery(reason)),
            position: None,
        },
        ReadError::UnsupportedFeature(feature) => SubsetReadError {
            error: GraphTextError::Read(Error::UnsupportedFeature(feature)),
            position: None,
        },
        ReadError::At { error, position } => SubsetReadError {
            error: GraphTextError::Read(convert::graph_error(error)),
            position: Some(position),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subset_limit_defaults_and_validation_follow_the_contract() {
        assert_eq!(SubsetLimits::default().max_scratch_bytes, 32 * 1024 * 1024);
        assert!(SubsetLimits::default().validate().is_ok());

        assert!(matches!(
            SubsetLimits {
                max_binding_rows: 0,
                ..SubsetLimits::DEFAULT
            }
            .validate(),
            Err(Error::InvalidQuery("subset work limits must be positive"))
        ));
        assert!(matches!(
            SubsetLimits {
                max_scratch_bytes: 128 * 1024 * 1024 + 1,
                ..SubsetLimits::DEFAULT
            }
            .validate(),
            Err(Error::GraphLimitExceedsCeiling {
                limit: "max_scratch_bytes",
                ..
            })
        ));
    }

    #[test]
    fn adapter_error_mapping_preserves_kind_and_diagnostic_position() {
        let error = read_error(ReadError::At {
            error: engine::GraphError::InvalidQuery("subset hop bound exceeds max_hops"),
            position: 23,
        });
        assert_eq!(error.position, Some(23));
        assert!(matches!(
            error.error,
            GraphTextError::Read(Error::InvalidQuery("subset hop bound exceeds max_hops"))
        ));
        let error = read_error(ReadError::Runtime("default time overflow"));
        assert!(matches!(
            error.error,
            GraphTextError::Read(Error::InvalidQuery("default time overflow"))
        ));
        let error = read_error(ReadError::Query(engine::language::LanguageError {
            kind: engine::language::LanguageErrorKind::Syntax,
            position: 7,
            message: "expected token".into(),
        }));
        assert!(matches!(error.error, GraphTextError::Query(error) if error.position == 7));
    }
}

#[cfg(test)]
#[path = "subset_read_tests.rs"]
mod graph_subset_read;

#[cfg(test)]
#[path = "subset_write_tests.rs"]
mod graph_subset_write;
