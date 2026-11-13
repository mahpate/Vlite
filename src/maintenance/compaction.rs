//! Internal preservation scanner and two-hop transform for safe single-file
//! compaction (feature-dashboard-compaction EPIC-001).
//!
//! The scanner certifies the persisted state a replayed v5 single file
//! currently exposes; it never mutates its input and never infers erased
//! history. The transform converts a *frozen copy* into a fresh directory
//! store and packs that store into a fresh single-file candidate using only
//! the public `convert_to_directory` and `backup` paths, then proves the
//! candidate preserves every supported logical field.
//!
//! Exact preservation: lineage, global sequence `S`, collection IDs, names,
//! settings, record bodies, record revisions, allocator high-waters, graph
//! IDs/names/keys/labels/properties/endpoints/attachments/schema/indexes.
//! Monotonic preservation: graph node/edge revisions may rise to at most `S`
//! and never fall, so stale CAS tokens fail and the next write exceeds `S`.

use super::{require_complete, source};
use crate::{Error, Result, format};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

/// Leave room for at least this many commits after `S`.
const SEQUENCE_HEADROOM: u64 = 1 << 32;

/// A stable refusal: the exact API code plus the evidence that produced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub code: &'static str,
    pub evidence: &'static str,
    pub detail: String,
}

fn refuse(code: &'static str, detail: impl Into<String>) -> Refusal {
    Refusal {
        code,
        evidence: "persisted_source",
        detail: detail.into(),
    }
}

/// Per-collection preserved state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CollectionManifest {
    pub name: String,
    pub dimensions: usize,
    pub settings: String,
    pub records: usize,
    /// Digest of every live record body (ID, vector bits, payload, metadata).
    pub body_digest: [u8; 32],
    /// Exact record revisions.
    pub revisions: BTreeMap<u64, u64>,
}

/// Per-graph preserved state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GraphManifest {
    pub name: String,
    pub next_node_id: u64,
    pub next_edge_id: u64,
    pub nodes: usize,
    pub edges: usize,
    /// Digest of node/edge content, attachments, schema and index keys,
    /// excluding revisions.
    pub content_digest: [u8; 32],
    pub node_revisions: BTreeMap<u64, u64>,
    pub edge_revisions: BTreeMap<u64, u64>,
}

/// Canonical logical manifest of a supported plain v5 single file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Manifest {
    pub lineage: u64,
    pub sequence: u64,
    pub dimensions: usize,
    pub collection_high: u16,
    pub collections: BTreeMap<u16, CollectionManifest>,
    pub graphs: Option<BTreeMap<u16, GraphManifest>>,
    pub file_bytes: u64,
}

fn digest_debug(hasher: &mut Sha256, value: &impl std::fmt::Debug) {
    let text = format!("{value:?}");
    hasher.update((text.len() as u64).to_le_bytes());
    hasher.update(text.as_bytes());
}

/// Scans a replayed source. Returns the canonical manifest or the first
/// refusal from the persisted-state evidence source.
pub(crate) fn scan(loaded: &format::Loaded) -> std::result::Result<Manifest, Refusal> {
    if loaded.version != format::V5 {
        return Err(refuse(
            "unsupported_source",
            format!("format version {} is not plain v5", loaded.version),
        ));
    }
    if loaded.incomplete_bytes != 0 {
        return Err(refuse(
            "unsupported_source",
            "the file has an incomplete tail",
        ));
    }
    let flags = loaded.flags;
    if flags & format::FLAG_ENCRYPTED != 0 || loaded.cipher.is_some() {
        return Err(refuse(
            "encrypted_compaction_unqualified",
            "encrypted files are not qualified for compaction",
        ));
    }
    if flags & format::FLAG_TIMESTAMPS != 0 || !loaded.commit_times.is_empty() {
        return Err(refuse(
            "unsupported_source",
            "commit timestamps cannot be preserved",
        ));
    }
    if flags & format::FLAG_TEMPORAL != 0 {
        return Err(refuse(
            "unsupported_source",
            "temporal graph history is not supported",
        ));
    }
    if flags & (format::FLAG_COMPACT | format::FLAG_PRECISION) != 0
        || loaded.compact_skipped.blocks != 0
    {
        return Err(refuse(
            "unsupported_source",
            "compact or stored-precision images are not supported",
        ));
    }
    let known = format::FLAG_GRAPH | format::FLAG_HISTORY;
    if flags & !known != 0 {
        return Err(refuse(
            "unsupported_source",
            format!("unrecognized feature flags {:#x}", flags & !known),
        ));
    }
    let Some(lineage) = loaded.lineage else {
        return Err(refuse("unsupported_source", "missing lineage"));
    };
    if loaded.sequence > u64::MAX - SEQUENCE_HEADROOM {
        return Err(refuse(
            "unsupported_source",
            "commit sequence space is nearly exhausted",
        ));
    }
    if loaded
        .imports
        .iter()
        .any(|marker| !matches!(marker, format::LoadedImportMarker::CollectionDropped(_)))
    {
        return Err(refuse(
            "import_state_unsupported",
            "the file retains import job state",
        ));
    }
    let mut collections = BTreeMap::new();
    for (&id, collection) in &loaded.collections {
        if !collection.expiries.is_empty() {
            return Err(refuse(
                "ttl_compaction_unsupported",
                format!("collection {} stores record deadlines", collection.name),
            ));
        }
        if collection.precision.is_some() {
            return Err(refuse(
                "unsupported_source",
                "stored-precision collections are not supported",
            ));
        }
        let mut hasher = Sha256::new();
        for (record_id, record) in collection.records.iter() {
            hasher.update(record_id.to_le_bytes());
            hasher.update((record.vector.len() as u64).to_le_bytes());
            for value in record.vector.iter() {
                hasher.update(value.to_bits().to_le_bytes());
            }
            hasher.update((record.payload.len() as u64).to_le_bytes());
            hasher.update(&record.payload);
            digest_debug(&mut hasher, &record.metadata);
        }
        collections.insert(
            id,
            CollectionManifest {
                name: collection.name.clone(),
                dimensions: collection.dimensions,
                settings: format!("{:?}", collection.settings),
                records: collection.records.len(),
                body_digest: hasher.finalize().into(),
                revisions: collection.revisions.clone(),
            },
        );
    }
    let graphs = match &loaded.graph {
        None => None,
        Some(state) => {
            let mut graphs = BTreeMap::new();
            for graph in state.graphs() {
                if graph.has_stored_expiry() {
                    return Err(refuse(
                        "ttl_compaction_unsupported",
                        format!("graph {} stores node or edge deadlines", graph.name()),
                    ));
                }
                if graph.has_temporal_segments() {
                    return Err(refuse(
                        "unsupported_source",
                        "temporal graph edges are not supported",
                    ));
                }
                let mut hasher = Sha256::new();
                let mut node_revisions = BTreeMap::new();
                let mut edge_revisions = BTreeMap::new();
                for node in graph.nodes() {
                    hasher.update(b"n");
                    digest_debug(
                        &mut hasher,
                        &(node.id, &node.key, &node.labels, &node.properties),
                    );
                    for attachment in graph.attachments(node.id) {
                        digest_debug(&mut hasher, &attachment);
                    }
                    node_revisions.insert(node.id.0, node.revision);
                }
                for edge in graph.edges() {
                    hasher.update(b"e");
                    digest_debug(
                        &mut hasher,
                        &(
                            edge.id,
                            &edge.key,
                            edge.source,
                            &edge.relationship,
                            edge.destination,
                            &edge.properties,
                        ),
                    );
                    edge_revisions.insert(edge.id.0, edge.revision);
                }
                digest_debug(&mut hasher, graph.schema());
                for key in graph.property_index_keys() {
                    digest_debug(&mut hasher, key);
                }
                graphs.insert(
                    graph.id().0,
                    GraphManifest {
                        name: graph.name().to_owned(),
                        next_node_id: graph.next_node_id(),
                        next_edge_id: graph.next_edge_id(),
                        nodes: graph.node_count(),
                        edges: graph.edge_count(),
                        content_digest: hasher.finalize().into(),
                        node_revisions,
                        edge_revisions,
                    },
                );
            }
            Some(graphs)
        }
    };
    Ok(Manifest {
        lineage,
        sequence: loaded.sequence,
        dimensions: loaded.dimensions,
        collection_high: loaded.collection_high,
        collections,
        graphs,
        file_bytes: loaded.end + loaded.incomplete_bytes,
    })
}

/// Proves `after` preserves `before`. Graph revisions may rise to at most
/// `S` but never fall; everything else must match exactly.
pub(crate) fn compare(before: &Manifest, after: &Manifest) -> std::result::Result<(), String> {
    let s = before.sequence;
    let fail = |what: &str| Err(format!("candidate does not preserve {what}"));
    if after.lineage != before.lineage {
        return fail("lineage");
    }
    if after.sequence != s {
        return fail("the global commit sequence");
    }
    if after.dimensions != before.dimensions {
        return fail("default dimensions");
    }
    if after.collection_high != before.collection_high {
        return fail("the collection ID high-water");
    }
    if after.collections != before.collections {
        return fail("collection records, settings or revisions");
    }
    match (&before.graphs, &after.graphs) {
        (None, None) => {}
        (Some(old_graphs), Some(new_graphs)) => {
            if old_graphs.len() != new_graphs.len() {
                return fail("the graph set");
            }
            for (id, old) in old_graphs {
                let Some(new) = new_graphs.get(id) else {
                    return fail("the graph set");
                };
                if new.name != old.name
                    || new.next_node_id != old.next_node_id
                    || new.next_edge_id != old.next_edge_id
                {
                    return fail("graph names or node/edge ID high-waters");
                }
                if new.nodes != old.nodes
                    || new.edges != old.edges
                    || new.content_digest != old.content_digest
                {
                    return fail("graph content, attachments, schema or indexes");
                }
                for (revisions_old, revisions_new) in [
                    (&old.node_revisions, &new.node_revisions),
                    (&old.edge_revisions, &new.edge_revisions),
                ] {
                    if revisions_old.len() != revisions_new.len() {
                        return fail("graph entity sets");
                    }
                    for (entity, &revision) in revisions_old {
                        match revisions_new.get(entity) {
                            Some(&next) if next >= revision && next <= s => {}
                            _ => return fail("monotonic graph revisions"),
                        }
                    }
                }
            }
        }
        _ => return fail("graph enablement"),
    }
    Ok(())
}

/// Scans a closed source path (exclusive lock) without transforming it.
pub(crate) fn scan_path(path: &Path) -> Result<std::result::Result<Manifest, Refusal>> {
    let mut file = source(path)?;
    let loaded = format::load(&mut file)?;
    require_complete(&loaded)?;
    Ok(scan(&loaded))
}

/// Failure of a two-hop transform.
#[derive(Debug)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum TransformError {
    /// The frozen source is outside the supported preservation matrix.
    Refused(Refusal),
    /// The candidate failed logical comparison (`validation_failed`).
    Mismatch(String),
    /// I/O or format failure.
    Failed(Error),
}

impl From<Error> for TransformError {
    fn from(error: Error) -> Self {
        Self::Failed(error)
    }
}

/// A validated candidate: manifests before and after, and measured sizes.
#[derive(Debug)]
pub(crate) struct Transformed {
    pub before: Manifest,
    pub after: Manifest,
}

/// Scans `frozen` (a closed, verified copy), converts it to the new directory
/// `staging`, packs that into the new single file `candidate`, and proves the
/// candidate preserves the frozen manifest. Neither output may exist; the
/// frozen copy is never modified. The caller owns cleanup of `staging`.
pub(crate) fn transform(
    frozen: &Path,
    staging: &Path,
    candidate: &Path,
) -> std::result::Result<Transformed, TransformError> {
    let before = scan_path(frozen)?.map_err(TransformError::Refused)?;
    super::convert_to_directory(frozen, staging)?;
    super::backup(staging, candidate)?;
    let after = scan_path(candidate)?.map_err(|refusal| {
        TransformError::Mismatch(format!("candidate scan refused: {}", refusal.detail))
    })?;
    compare(&before, &after).map_err(TransformError::Mismatch)?;
    Ok(Transformed { before, after })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::InvalidRowPolicy;
    use crate::property_graph::{GraphSchema, LabelDefinition, NodePatch};
    use crate::{
        CreateOptions, Database, EncryptionKey, Expected, Metadata, MetadataValue, Mutation,
    };
    use tempfile::tempdir;

    fn rich(path: &Path) {
        let mut db = Database::create_with_options(path, 2, CreateOptions::default()).unwrap();
        for id in 1..=5u64 {
            db.put(id, &[id as f32, 0.5], format!("p{id}").as_bytes())
                .unwrap();
        }
        db.delete(5).unwrap();
        db.create_graph("g").unwrap();
        {
            let mut graph = db.graph_mut("g").unwrap();
            graph
                .create_node("a", &["Person"], &Metadata::new())
                .unwrap();
            graph
                .create_node("b", &["Person"], &Metadata::new())
                .unwrap();
            graph.create_node("z", &[], &Metadata::new()).unwrap();
            graph
                .create_edge(Some("ab"), "a", "KNOWS", "b", &Metadata::new())
                .unwrap();
            graph.attach("a", "default", 1).unwrap();
            graph
                .put_schema(&GraphSchema::default().label("Person", LabelDefinition::new("people")))
                .unwrap();
            // Deleting the highest node ID must not lower the allocator.
            graph.delete_node("z", Expected::Any).unwrap();
            graph
                .patch_node(
                    "b",
                    &NodePatch {
                        set: Metadata::from([("n".into(), MetadataValue::Integer(1))]),
                        ..NodePatch::default()
                    },
                    Expected::Any,
                )
                .unwrap();
        }
        db.put(2, &[2.0, 2.0], b"two-again").unwrap();
        db.close().unwrap();
    }

    #[test]
    fn rich_source_round_trips_twice_with_monotonic_graph_revisions() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("src.vlite");
        rich(&file);
        let candidate = dir.path().join("restore-1.vlite");
        let result = transform(&file, &dir.path().join("staging"), &candidate).unwrap();
        assert_eq!(result.before.lineage, result.after.lineage);
        assert_eq!(result.after.sequence, result.before.sequence);
        let graph = result
            .after
            .graphs
            .as_ref()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert_eq!(graph.next_node_id, 3, "deleted highest node ID is retained");
        assert!(
            graph
                .node_revisions
                .values()
                .all(|&revision| revision <= result.after.sequence)
        );
        let second = transform(
            &candidate,
            &dir.path().join("staging2"),
            &dir.path().join("restore-2.vlite"),
        )
        .unwrap();
        assert_eq!(second.after.sequence, result.before.sequence);
        assert_eq!(second.after.lineage, result.before.lineage);
    }

    #[test]
    fn compare_rejects_allocator_lineage_and_revision_regressions() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("src.vlite");
        rich(&file);
        let base = scan_path(&file).unwrap().unwrap();
        compare(&base, &base).unwrap();
        let mutated = |change: &dyn Fn(&mut Manifest)| {
            let mut copy = base.clone();
            change(&mut copy);
            compare(&base, &copy).is_err()
        };
        assert!(mutated(&|m| m.lineage ^= 1));
        assert!(mutated(&|m| m.sequence += 1));
        assert!(mutated(&|m| m.collection_high += 1));
        assert!(mutated(&|m| first_graph(m).next_node_id -= 1));
        assert!(mutated(&|m| first_graph(m).next_edge_id += 1));
        assert!(mutated(&|m| {
            *first_graph(m).node_revisions.values_mut().next().unwrap() -= 1;
        }));
        let s = base.sequence;
        assert!(mutated(&|m| {
            *first_graph(m).edge_revisions.values_mut().next().unwrap() = s + 1;
        }));
        // Raising a graph revision to S is allowed.
        assert!(!mutated(&|m| {
            *first_graph(m).edge_revisions.values_mut().next().unwrap() = s;
        }));
        assert!(mutated(&|m| {
            let collection = m.collections.values_mut().next().unwrap();
            *collection.revisions.values_mut().next().unwrap() += 1;
        }));
        assert!(mutated(&|m| m
            .collections
            .values_mut()
            .next()
            .unwrap()
            .body_digest[0] ^= 1));
    }

    fn first_graph(m: &mut Manifest) -> &mut GraphManifest {
        m.graphs.as_mut().unwrap().values_mut().next().unwrap()
    }

    fn refused(path: &Path) -> Refusal {
        scan_path(path).unwrap().unwrap_err()
    }

    #[test]
    fn ttl_timestamps_and_encryption_are_refused_with_codes() {
        let dir = tempdir().unwrap();
        let ttl = dir.path().join("ttl.vlite");
        let mut db = Database::create(&ttl, 2).unwrap();
        // An already-expired deadline still counts.
        db.put_with_expiry(1, &[1.0, 1.0], b"x", &Metadata::new(), Some(1))
            .unwrap();
        db.close().unwrap();
        let refusal = refused(&ttl);
        assert_eq!(refusal.code, "ttl_compaction_unsupported");
        assert_eq!(refusal.evidence, "persisted_source");

        let graph_ttl = dir.path().join("graph-ttl.vlite");
        let mut db = Database::create(&graph_ttl, 2).unwrap();
        db.create_graph("g").unwrap();
        {
            let mut graph = db.graph_mut("g").unwrap();
            graph.create_node("a", &[], &Metadata::new()).unwrap();
            graph
                .set_node_expiry("a", Some(u64::MAX / 2), Expected::Any)
                .unwrap();
        }
        db.close().unwrap();
        assert_eq!(refused(&graph_ttl).code, "ttl_compaction_unsupported");

        let stamped = dir.path().join("stamped.vlite");
        let options = CreateOptions {
            commit_timestamps: true,
            ..CreateOptions::default()
        };
        Database::create_with_options(&stamped, 2, options)
            .unwrap()
            .close()
            .unwrap();
        assert_eq!(refused(&stamped).code, "unsupported_source");

        let encrypted = dir.path().join("enc.vlite");
        let key = EncryptionKey::from_bytes([7u8; 32]);
        let options = CreateOptions {
            encryption: Some(key.clone()),
            ..CreateOptions::default()
        };
        Database::create_with_options(&encrypted, 2, options)
            .unwrap()
            .close()
            .unwrap();
        let mut file = source(&encrypted).unwrap();
        let loaded = format::load_with(&mut file, Some(&key)).unwrap();
        assert_eq!(
            scan(&loaded).unwrap_err().code,
            "encrypted_compaction_unqualified"
        );
    }

    #[test]
    fn finished_import_markers_are_refused() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("imported.vlite");
        let mut db = Database::create(&path, 2).unwrap();
        let rows = [Mutation::Put {
            id: 1,
            vector: vec![1.0, 1.0],
            payload: Vec::new(),
        }];
        let marker = |chunk: u64, rows: &[Mutation], flags: u32| format::ImportMarker {
            job: 7,
            chunk,
            rows_committed: rows.len() as u64,
            rows_skipped: 0,
            flags,
            digest: if rows.is_empty() {
                [0; 32]
            } else {
                crate::import::chunk_digest(rows, InvalidRowPolicy::RejectChunk)
            },
        };
        db.commit_import_frame(0, &rows, &marker(1, &rows, 0))
            .unwrap();
        db.commit_import_frame(0, &[], &marker(1, &[], format::MARKER_FINISHED))
            .unwrap();
        db.close().unwrap();
        assert_eq!(refused(&path).code, "import_state_unsupported");
    }

    #[test]
    fn torn_tails_old_versions_unknown_flags_and_exhaustion_are_refused() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("torn.vlite");
        let mut db = Database::create(&path, 2).unwrap();
        db.put(1, &[1.0, 1.0], b"x").unwrap();
        db.close().unwrap();
        let length = std::fs::metadata(&path).unwrap().len();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(length - 3).unwrap();
        drop(file);
        let mut file = source(&path).unwrap();
        let mut loaded = format::load(&mut file).unwrap();
        assert_eq!(scan(&loaded).unwrap_err().code, "unsupported_source");
        loaded.incomplete_bytes = 0;
        scan(&loaded).unwrap();
        loaded.sequence = u64::MAX - 1;
        assert_eq!(scan(&loaded).unwrap_err().code, "unsupported_source");
        loaded.sequence = 1;
        loaded.version = 4;
        assert_eq!(scan(&loaded).unwrap_err().code, "unsupported_source");
        loaded.version = format::V5;
        loaded.flags |= 0x4000;
        assert_eq!(scan(&loaded).unwrap_err().code, "unsupported_source");
    }
}
