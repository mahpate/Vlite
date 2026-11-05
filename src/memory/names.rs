//! The names the memory core reserves in a graph: the node labels and node
//! keys only the memory core may create. A raw graph write that would create or
//! change a node with such a name is refused, so no raw writer can plant a node
//! that looks like memory (0.7.1 design note, section 2).
//! Nothing here is a public API, wire route or public `Error` variant.

use super::{MEMORY_NODE_KINDS, binding::RECORD_KEY};
use crate::property_graph::GraphOp;

/// Every label of the memory core (`VliteMemory`, `VliteMemoryScope`, ...)
/// starts with this, case sensitive.
const LABEL_PREFIX: &str = "VliteMemory";

/// Whether `label` is one the memory core reserves.
pub(crate) fn reserved_label(label: &str) -> bool {
    label.starts_with(LABEL_PREFIX)
}

/// Whether `key` is a node key the memory core reserves: the prefix of one of
/// its node kinds, or exactly the binding record's key.
pub(crate) fn reserved_key(key: &str) -> bool {
    key == RECORD_KEY
        || MEMORY_NODE_KINDS
            .iter()
            .any(|(_, prefix)| key.starts_with(prefix))
}

/// Whether a raw graph write may not carry `op`: it creates or replaces a node
/// with a reserved key or label, patches a node with a reserved key, or gives a
/// node a reserved label. Everything else stays allowed so a node planted by an
/// earlier release can still be cleaned up: deleting a reserved node, and
/// removing a reserved label from a node that has an ordinary key (a node with
/// a reserved key is deleted, not patched).
pub(crate) fn refuses_raw(op: &GraphOp) -> bool {
    match op {
        GraphOp::CreateNode { key, labels, .. } | GraphOp::UpsertNode { key, labels, .. } => {
            reserved_key(key) || labels.iter().any(|label| reserved_label(label))
        }
        GraphOp::PatchNode { key, patch, .. } => {
            reserved_key(key) || patch.add_labels.iter().any(|label| reserved_label(label))
        }
        _ => false,
    }
}

/// Whether a raw graph write batch carries an operation [`refuses_raw`]
/// refuses. The batch is then refused as a whole, before any operation runs.
pub(crate) fn refuses_raw_batch(ops: &[GraphOp]) -> bool {
    ops.iter().any(refuses_raw)
}

#[cfg(test)]
mod tests;
