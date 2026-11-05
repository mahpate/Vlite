//! Startup checks for bound memory graphs (mapping section 6, B2) and the
//! binding record that makes them survive a restart.
//! Design: `docs/projects/roadmap/m71-memory-followups.md` section 1.

use super::binding::{
    Binding, BindingError, BindingSet, Corrupt, RECORD_KEY, RECORD_LABEL, Recorded, validate_all,
};
use super::{MEMORY_NODE_KINDS, MemoryError, MemoryMut, MemoryRef, MemoryResult};
use crate::property_graph::{
    FindNodes, GraphLimits, GraphOp, NodeView, PropertyProjection, ReadOptions,
};
use crate::{Database, Error, Expected};

/// Why startup refuses. The server owns the fixed message of each class; none
/// names a binding, a graph, a collection or a tenant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// A stored record is not exactly a record, or the records contradict
    /// each other.
    Records,
    /// The file breaks a rotation rule against the stored records.
    Rotation(BindingError),
    /// A named graph or evidence collection holds ordinary data.
    InUse,
    /// A stored resource could not be read.
    Unreadable,
}

/// How a named binding came to own its graph (design note 1.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Adoption {
    /// (a) No live node and an empty evidence collection.
    Fresh,
    /// (b) The graph already holds this binding's record.
    Recorded,
    /// (c) A graph adopted before records existed: it holds only memory.
    Legacy,
}

/// What a start must still do once every check passed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Indexes into the file's set whose stored record is absent or differs.
    pub writes: Vec<usize>,
    /// Records of entries the file does not name. Their graph and evidence
    /// collection stay reserved with no grants (design note 1.4).
    pub orphans: Vec<Recorded>,
}

/// The records of every live graph, each with the stored name of the graph
/// that holds it. A node with the record key but not the record label is
/// ordinary data, not a record; a node with the label that is not exactly a
/// record is corrupt.
fn collect(db: &Database) -> Result<Vec<(String, Recorded)>, Refusal> {
    let mut found = Vec::new();
    for info in db.list_graphs().map_err(|_| Refusal::Unreadable)? {
        let graph = db.graph(&info.name).map_err(|_| Refusal::Unreadable)?;
        let Some(node) = graph
            .get_node(RECORD_KEY)
            .map_err(|_| Refusal::Unreadable)?
            .output
        else {
            continue;
        };
        if !node.labels.contains(RECORD_LABEL) {
            continue;
        }
        let record = Recorded::decode(&node, &info.name).map_err(|Corrupt| Refusal::Records)?;
        found.push((info.name, record));
    }
    Ok(found)
}

/// Runs the startup checks without writing anything: the stored records are
/// read and must be consistent; when the file names bindings the rotation
/// rules run against them and every named graph must be adoptable. With no
/// bindings in the file nothing is refused except a corrupt record, and every
/// record becomes an orphan (design note 1.4).
pub(crate) fn plan(db: &Database, set: &BindingSet, now_ms: u64) -> Result<Plan, Refusal> {
    let found = collect(db)?;
    let previous: Vec<Recorded> = found.iter().map(|(_, record)| record.clone()).collect();
    validate_all(&previous).map_err(|_| Refusal::Records)?;
    let mut writes = Vec::new();
    if !set.bindings.is_empty() {
        set.validate_against(&previous, now_ms)
            .map_err(Refusal::Rotation)?;
        for (index, binding) in set.bindings.iter().enumerate() {
            let graph = binding.stored(&binding.graph);
            let evidence = binding
                .evidence_collection
                .as_deref()
                .map(|name| binding.stored(name));
            let held = found
                .iter()
                .find(|(holder, _)| *holder == graph)
                .map(|(_, record)| record);
            let memory = MemoryRef::bind(db, &graph, evidence.as_deref())
                .map_err(|_| Refusal::Unreadable)?;
            if memory
                .adoption(binding, held)
                .map_err(|_| Refusal::Unreadable)?
                .is_none()
            {
                return Err(Refusal::InUse);
            }
            if held != Some(&Recorded::of(binding)) {
                writes.push(index);
            }
        }
    }
    let orphans = previous
        .into_iter()
        .filter(|record| set.find(&record.tenant, &record.binding).is_none())
        .collect();
    Ok(Plan { writes, orphans })
}

/// Whether `node` is one the memory core writes: exactly one label, and the
/// key prefix that label's nodes carry.
fn memory_node(node: &NodeView) -> bool {
    let mut labels = node.labels.iter();
    let (Some(label), None) = (labels.next(), labels.next()) else {
        return false;
    };
    MEMORY_NODE_KINDS
        .iter()
        .any(|(kind, prefix)| label == kind && node.key.starts_with(prefix))
}

impl MemoryRef<'_> {
    /// How `binding` may adopt this graph, or `None` when the graph or its
    /// evidence collection already holds ordinary data. `held` is the record
    /// this graph stores, if any.
    pub(crate) fn adoption(
        &self,
        binding: &Binding,
        held: Option<&Recorded>,
    ) -> MemoryResult<Option<Adoption>> {
        if let Some(record) = held {
            return Ok(record
                .is(&binding.tenant, &binding.binding)
                .then_some(Adoption::Recorded));
        }
        let graph = self.graph()?;
        if graph.info().node_count == 0 {
            let empty = match self.evidence_collection.as_deref() {
                Some(collection) => self.db.collection(collection)?.is_empty(),
                None => true,
            };
            return Ok(empty.then_some(Adoption::Fresh));
        }
        Ok(self.holds_only_memory()?.then_some(Adoption::Legacy))
    }

    /// Whether every node of the graph is one the memory core writes. The
    /// scan pages through the whole graph; it runs once, for a graph that
    /// 0.7.0 adopted and that has no record yet. The evidence collection is
    /// not inspected: 0.7.0 let an operator load evidence before attaching it.
    fn holds_only_memory(&self) -> MemoryResult<bool> {
        let graph = self.graph()?;
        let mut cursor = None;
        loop {
            let page = graph.find_nodes(
                &FindNodes {
                    label: None,
                    conditions: Vec::new(),
                    limit: Some(GraphLimits::CEILING.max_output_nodes),
                    cursor,
                },
                &ReadOptions {
                    limits: GraphLimits::CEILING,
                    projection: PropertyProjection::None,
                    cancel: None,
                },
            )?;
            if !page.output.iter().all(memory_node) {
                return Ok(false);
            }
            match page.cursor {
                Some(next) => cursor = Some(next),
                None if page.complete => return Ok(true),
                None => {
                    return Err(MemoryError::Storage(Error::InvalidQuery(
                        "memory adoption scan did not complete",
                    )));
                }
            }
        }
    }

    /// Design note 1.5: this graph's record must still be the binding the
    /// caller serves (tenant, name and generation).
    pub(crate) fn check_binding(
        &self,
        tenant: &str,
        binding: &str,
        generation: u64,
    ) -> MemoryResult<()> {
        let node = self.graph()?.get_node(RECORD_KEY)?.output;
        let current = node
            .and_then(|node| Recorded::decode(&node, &self.graph).ok())
            .is_some_and(|record| record.is(tenant, binding) && record.generation == generation);
        if current {
            Ok(())
        } else {
            Err(MemoryError::BindingGeneration)
        }
    }
}

/// What `write_binding_record` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordWrite {
    Created,
    Updated,
    Unchanged,
}

impl MemoryMut<'_> {
    /// Writes or updates this graph's binding record for `binding`. An
    /// identical record is left alone, so a restart with the same file adds
    /// no commit; a record that is not exactly a record is never repaired.
    pub(crate) fn write_binding_record(&mut self, binding: &Binding) -> MemoryResult<RecordWrite> {
        let wanted = Recorded::of(binding);
        let existing = self.db.graph(&self.graph)?.get_node(RECORD_KEY)?.output;
        let op = match existing {
            None => GraphOp::CreateNode {
                key: RECORD_KEY.into(),
                labels: vec![RECORD_LABEL.into()],
                properties: wanted.properties(),
            },
            Some(node) => {
                let stored = Recorded::decode(&node, &self.graph)
                    .map_err(|Corrupt| MemoryError::InvalidBundle)?;
                if stored == wanted {
                    return Ok(RecordWrite::Unchanged);
                }
                GraphOp::UpsertNode {
                    key: RECORD_KEY.into(),
                    labels: vec![RECORD_LABEL.into()],
                    properties: wanted.properties(),
                    expected: Expected::Revision(node.revision),
                }
            }
        };
        let created = matches!(op, GraphOp::CreateNode { .. });
        self.db.graph_mut(&self.graph)?.write_batch(&[op])?;
        Ok(if created {
            RecordWrite::Created
        } else {
            RecordWrite::Updated
        })
    }
}

#[cfg(test)]
mod tests;
