//! Startup adoption check for a bound graph (mapping section 6, B2).

use super::{LABEL_SCOPE, MemoryRef, MemoryResult};
use crate::property_graph::{FindNodes, ReadOptions};

impl MemoryRef<'_> {
    /// Whether startup may adopt the bound resources: a fresh graph and
    /// evidence collection with no live entries, or a graph that already
    /// holds a memory scope. Anything else is an in-use ordinary resource.
    /// **PROVISIONAL**: a graph mixing memory scopes with ordinary nodes
    /// written before the bindings existed is adopted; the reserved-resource
    /// guards prevent such writes once bindings are configured.
    pub(crate) fn adoptable(&self) -> MemoryResult<bool> {
        let graph = self.graph()?;
        if graph.info().node_count == 0 {
            return Ok(match self.evidence_collection.as_deref() {
                Some(collection) => self.db.collection(collection)?.is_empty(),
                None => true,
            });
        }
        let scopes = graph.find_nodes(
            &FindNodes {
                label: Some(LABEL_SCOPE.into()),
                limit: Some(1),
                ..FindNodes::default()
            },
            &ReadOptions::default(),
        )?;
        Ok(!scopes.output.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{MemoryMut, MemoryScope};
    use super::*;
    use crate::{Database, Metadata, property_graph::GraphOp};

    fn scope() -> MemoryScope {
        MemoryScope {
            tenant: "@root".into(),
            agent: "a".into(),
            user: None,
            session: None,
        }
    }

    fn database(dir: &tempfile::TempDir) -> Database {
        let mut db = Database::create(dir.path().join("startup.vlite"), 2).unwrap();
        db.create_graph("memory").unwrap();
        db.create_collection(
            "evidence",
            crate::CollectionSpec {
                dimensions: 2,
                settings: crate::CollectionSettings::default(),
            },
        )
        .unwrap();
        db
    }

    fn adoptable(db: &Database, evidence: Option<&str>) -> bool {
        MemoryRef::bind(db, "memory", evidence)
            .unwrap()
            .adoptable()
            .unwrap()
    }

    #[test]
    fn fresh_resources_are_adoptable() {
        let dir = tempfile::tempdir().unwrap();
        let db = database(&dir);
        assert!(adoptable(&db, None));
        assert!(adoptable(&db, Some("evidence")));
    }

    #[test]
    fn a_graph_with_ordinary_nodes_is_not_adoptable() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir);
        db.graph_mut("memory")
            .unwrap()
            .write_batch(&[GraphOp::CreateNode {
                key: "ordinary".into(),
                labels: vec!["Person".into()],
                properties: Metadata::new(),
            }])
            .unwrap();
        assert!(!adoptable(&db, None));
    }

    #[test]
    fn a_fresh_graph_with_ordinary_evidence_records_is_not_adoptable() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir);
        db.collection_mut("evidence")
            .unwrap()
            .put(1, &[1.0, 0.0], b"")
            .unwrap();
        assert!(adoptable(&db, None));
        assert!(!adoptable(&db, Some("evidence")));
    }

    #[test]
    fn a_memory_provisioned_graph_stays_adoptable() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = database(&dir);
        MemoryMut::bind(&mut db, "memory", None)
            .unwrap()
            .provision_scope(&scope())
            .unwrap();
        assert!(adoptable(&db, None));
        db.collection_mut("evidence")
            .unwrap()
            .put(1, &[1.0, 0.0], b"")
            .unwrap();
        assert!(adoptable(&db, Some("evidence")));
    }
}
