//! Graph-aware maintenance (EPIC-006, ITEM-022): graph counts and invariant
//! checks for closed files, and the `enable_graph` / `strip_graph` copy-outs.
//! Re-exported from [`crate::maintenance`].
//!
//! The existing maintenance functions handle graph-enabled files themselves
//! (graph state is replayed and validated by every load, copied byte for
//! byte by `backup`/`restore`/`reencrypt`/`restore_to` and rewritten by
//! `compact*`); `upgrade` and the change feeds reject them explicitly.

use std::path::Path;

use crate::crypto::Cipher;
use crate::format::{self, Loaded};
use crate::maintenance::{
    self, CompactionReport, Inspection, Verification, inspection, publish_with, require_complete,
};
use crate::{EncryptionKey, Error, Result};

use super::engine::GraphState;

/// Graph objects in a file: live graphs and, across all of them, nodes,
/// edges and node-to-record attachments.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct GraphCounts {
    pub graphs: usize,
    pub nodes: usize,
    pub edges: usize,
    pub attachments: usize,
}

impl GraphCounts {
    fn of(state: Option<&GraphState>) -> Self {
        let mut counts = Self::default();
        for graph in state.into_iter().flat_map(GraphState::graphs) {
            counts.graphs += 1;
            counts.nodes += graph.node_count();
            counts.edges += graph.edge_count();
            counts.attachments += graph
                .nodes()
                .map(|node| graph.attachments(node.id).count())
                .sum::<usize>();
        }
        counts
    }
}

/// Result of [`inspect_graph`]: the unchanged [`Inspection`] plus graph data.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct GraphInspection {
    pub inspection: Inspection,
    /// The header carries `FLAG_GRAPH`.
    pub graph_enabled: bool,
    /// All zero for files that are not graph-enabled.
    pub counts: GraphCounts,
}

/// Result of [`verify_graph`]: the unchanged [`Verification`] plus graph data.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct GraphVerification {
    pub verification: Verification,
    pub graph_enabled: bool,
    pub counts: GraphCounts,
}

/// Result of [`strip_graph`]: the copy-out report and what was dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct StripGraphReport {
    pub compaction: CompactionReport,
    pub removed: GraphCounts,
}

/// INV-008 and INV-009 on a fully replayed file: graph-side invariants and
/// derived indexes, and every attached record live without expiry.
fn check_graph(loaded: &Loaded) -> Result<GraphCounts> {
    let Some(state) = &loaded.graph else {
        return Ok(GraphCounts::default());
    };
    let corrupt = |reason| Error::Corrupt { offset: 0, reason };
    state
        .check_invariants()
        .map_err(|_| corrupt("graph state violates its invariants"))?;
    for graph in state.graphs() {
        for node in graph.nodes() {
            for (collection, record) in graph.attachments(node.id) {
                if !loaded.collections.get(&collection).is_some_and(|target| {
                    target.records.contains_key(&record) && !target.expiries.contains_key(&record)
                }) {
                    return Err(corrupt(
                        "graph attachment references a missing or expiring record",
                    ));
                }
            }
        }
    }
    Ok(GraphCounts::of(Some(state)))
}

/// `maintenance::inspect` / `inspect_with_key` (`key` is required exactly
/// for encrypted files) plus the graph flag and counts. Graph invariants
/// (INV-008, INV-009) are checked; a violation is `Corrupt` at offset 0.
pub fn inspect_graph(
    path: impl AsRef<Path>,
    key: Option<&EncryptionKey>,
) -> Result<GraphInspection> {
    let mut file = maintenance::source(path.as_ref())?;
    let loaded = format::load_with(&mut file, key)?;
    let counts = check_graph(&loaded)?;
    Ok(GraphInspection {
        inspection: inspection(&loaded),
        graph_enabled: loaded.graph.is_some(),
        counts,
    })
}

/// `maintenance::verify` / `verify_with_key` plus the graph flag and counts,
/// with the same invariant checks as [`inspect_graph`].
pub fn verify_graph(
    path: impl AsRef<Path>,
    key: Option<&EncryptionKey>,
) -> Result<GraphVerification> {
    let (verification, loaded) = maintenance::verify_loaded(path.as_ref(), key)?;
    let counts = check_graph(&loaded)?;
    Ok(GraphVerification {
        verification,
        graph_enabled: loaded.graph.is_some(),
        counts,
    })
}

/// Rewrites a complete source as `compact*` does (live records, original
/// collection IDs, sequences restarting at one, no commit timestamps, the
/// same key with a fresh salt), with `extra_flags` in the new v5 header and,
/// when that includes `FLAG_GRAPH`, the source's graph state.
fn rewrite(
    loaded: &Loaded,
    destination: &Path,
    key: Option<&EncryptionKey>,
    extra_flags: u16,
) -> Result<CompactionReport> {
    let source = inspection(loaded);
    let cipher = key.map(Cipher::create).transpose()?;
    let lineage = format::new_lineage()?;
    let output = publish_with(destination, key, |output| {
        maintenance::write_v5(
            output,
            loaded,
            lineage,
            crate::current_time_ms(),
            cipher.as_ref(),
            extra_flags,
        )
    })?;
    Ok(CompactionReport { source, output })
}

fn load_complete(path: &Path, key: Option<&EncryptionKey>) -> Result<Loaded> {
    let mut file = maintenance::source(path)?;
    let loaded = format::load_with(&mut file, key)?;
    require_complete(&loaded)?;
    Ok(loaded)
}

/// Copies a closed v4 or v5 file that is not graph-enabled to a new,
/// nonexistent `destination` as a graph-enabled v5 file with no graphs
/// (REQ-003). The copy is a compaction (see `maintenance::compact`): live
/// records only, sequences restart, commit timestamps are not carried, and
/// encrypted sources stay encrypted under `key` with a fresh salt. The output
/// is verified before it is published; the source is never modified.
///
/// v1–v3 sources need `maintenance::upgrade` first, and graph-enabled sources
/// are refused; both are `GraphInvalidOptions`.
pub fn enable_graph(
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    key: Option<&EncryptionKey>,
) -> Result<CompactionReport> {
    let loaded = load_complete(source.as_ref(), key)?;
    if loaded.version < format::V4 {
        return Err(Error::GraphInvalidOptions(
            "enable_graph requires a format v4 or v5 source; run maintenance::upgrade first",
        ));
    }
    if loaded.graph.is_some() {
        return Err(Error::GraphInvalidOptions(
            "the source is already graph-enabled",
        ));
    }
    rewrite(&loaded, destination.as_ref(), key, format::FLAG_GRAPH)
}

/// Rollback path for graph-enabled files (REQ-004): copies the vector data of
/// a closed graph-enabled file to a new, nonexistent `destination` as a v5
/// file without `FLAG_GRAPH`, dropping every graph, node, edge and
/// attachment (the attached records themselves are kept). The copy is a
/// compaction like [`enable_graph`]'s. `confirm_graph_data_loss` must be
/// `true` (otherwise `GraphInvalidOptions`, checked first); sources that are
/// not graph-enabled are `GraphsNotEnabled`. The source is never modified.
pub fn strip_graph(
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
    key: Option<&EncryptionKey>,
    confirm_graph_data_loss: bool,
) -> Result<StripGraphReport> {
    if !confirm_graph_data_loss {
        return Err(Error::GraphInvalidOptions(
            "strip_graph deletes all graph data and requires confirm_graph_data_loss = true",
        ));
    }
    let loaded = load_complete(source.as_ref(), key)?;
    if loaded.graph.is_none() {
        return Err(Error::GraphsNotEnabled);
    }
    let removed = GraphCounts::of(loaded.graph.as_ref());
    let compaction = rewrite(&loaded, destination.as_ref(), key, 0)?;
    Ok(StripGraphReport {
        compaction,
        removed,
    })
}
