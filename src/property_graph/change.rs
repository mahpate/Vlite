//! Graph operations reported by the change feed (v0.6.0).

use crate::Metadata;

/// One graph operation inside a [`ChangeKind::GraphBatch`](crate::changes::ChangeKind::GraphBatch). IDs are the
/// engine's node and edge IDs; keys are resolved from the log.
#[derive(Clone, Debug, PartialEq)]
pub enum GraphChange {
    /// A node was created (`created`) or its labels and properties replaced.
    NodeUpserted {
        id: u64,
        key: String,
        labels: Vec<String>,
        properties: Metadata,
        created: bool,
    },
    NodePatched {
        id: u64,
        key: String,
        add_labels: Vec<String>,
        remove_labels: Vec<String>,
        set: Metadata,
        unset: Vec<String>,
    },
    NodeDeleted {
        id: u64,
        key: String,
    },
    /// An edge was created (`created`) or its properties replaced.
    EdgeUpserted {
        id: u64,
        key: Option<String>,
        source: u64,
        source_key: String,
        relationship: String,
        destination: u64,
        destination_key: String,
        properties: Metadata,
        created: bool,
    },
    EdgePatched {
        id: u64,
        key: Option<String>,
        set: Metadata,
        unset: Vec<String>,
    },
    EdgeDeleted {
        id: u64,
        key: Option<String>,
    },
    /// A node was attached to a record of `collection`.
    Attached {
        node: u64,
        node_key: String,
        collection: String,
        record: u64,
    },
    /// A node was detached from `record` of `collection`.
    Detached {
        node: u64,
        node_key: String,
        collection: String,
        record: u64,
    },
    /// The declared schema was replaced (an empty schema removes it).
    SchemaUpdated {
        schema: super::GraphSchema,
    },
    /// Gap 4 (unreleased): a change to a temporal edge's validity series.
    /// Only [`Database::temporal_changes_after`](crate::Database::temporal_changes_after)
    /// reports it; every other feed refuses temporal changes.
    Temporal(TemporalChange),
}

/// Gap 4 (unreleased): one change to a temporal edge's validity series, in
/// commit order (never at a validity boundary). Corrections name the old
/// segment by its exact bounds and carry only the new values; the old
/// properties are not repeated.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum TemporalChange {
    /// A temporal edge was created with this series (`created`), or an
    /// existing edge was made temporal with one all-time segment.
    EdgePut {
        id: u64,
        key: Option<String>,
        source: u64,
        source_key: String,
        relationship: String,
        destination: u64,
        destination_key: String,
        segments: Vec<super::TemporalSegment>,
        created: bool,
    },
    SegmentAppended {
        id: u64,
        key: Option<String>,
        segment: super::TemporalSegment,
    },
    /// The segment with exactly the old bounds was replaced by `segment`.
    SegmentReplaced {
        id: u64,
        key: Option<String>,
        valid_from: Option<i64>,
        valid_to: Option<i64>,
        segment: super::TemporalSegment,
    },
    /// The segment with exactly the old bounds now ends at `at`.
    SegmentClosed {
        id: u64,
        key: Option<String>,
        valid_from: Option<i64>,
        valid_to: Option<i64>,
        at: i64,
    },
    SegmentRemoved {
        id: u64,
        key: Option<String>,
        valid_from: Option<i64>,
        valid_to: Option<i64>,
    },
}
