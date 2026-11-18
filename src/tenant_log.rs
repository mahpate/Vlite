//! Tenant-only projection of a shared VLite log (HA EPIC-007, ITEM-027).
//!
//! Crate-internal; implemented test-first against
//! `docs/projects/feature-aca-ha/feature-aca-ha.prd.md` ITEM-027.

use crate::format::{self, RawFrame};
use crate::property_graph::engine::codec::{GraphEntry, OP_GRAPH};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Limits {
    /// Cumulative logical bytes read, including replay and output verification.
    pub max_scan_bytes: u64,
    /// Cumulative bytes written to the private stage and publication stage.
    pub max_write_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum Profile {
    Version(u32),
    Encrypted,
    Timestamped,
    Compact,
    Precision,
    TemporalUnavailable,
    UnknownFlags(u16),
    UnknownOperation(u8),
    IncompleteTail,
}

#[derive(Debug)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum ProjectionError {
    InvalidTenant,
    Profile(Profile),
    Storage(crate::Error),
    BudgetExceeded {
        resource: &'static str,
        limit: u64,
    },
    CrossTenantReference {
        sequence: u64,
    },
    InventoryMismatch,
    Cleanup {
        original: Box<ProjectionError>,
        source: std::io::Error,
    },
}

impl std::fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTenant => {
                f.write_str("tenant ID must be exactly 32 hexadecimal characters")
            }
            Self::Profile(reason) => write!(f, "unsupported tenant projection profile: {reason:?}"),
            Self::Storage(error) => error.fmt(f),
            Self::BudgetExceeded { resource, limit } => {
                write!(
                    f,
                    "tenant projection {resource} budget exceeded ({limit} bytes)"
                )
            }
            Self::CrossTenantReference { sequence } => {
                write!(f, "cross-tenant graph reference at sequence {sequence}")
            }
            Self::InventoryMismatch => f.write_str("tenant projection logical inventory mismatch"),
            Self::Cleanup { original, source } => {
                write!(
                    f,
                    "{original}; removing the failed projection also failed: {source}"
                )
            }
        }
    }
}

impl std::error::Error for ProjectionError {}

impl From<crate::Error> for ProjectionError {
    fn from(error: crate::Error) -> Self {
        Self::Storage(error)
    }
}

impl From<std::io::Error> for ProjectionError {
    fn from(error: std::io::Error) -> Self {
        Self::Storage(crate::Error::Io(error))
    }
}

type Result<T> = std::result::Result<T, ProjectionError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum Resource {
    Collection(u16),
    Settings(u16),
    Records(u16),
    Vectors(u16),
    Graph(u16),
    Imports(u16),
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Inventory {
    pub resources: BTreeMap<Resource, [u8; 32]>,
    pub root: [u8; 32],
}

#[derive(Debug)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Projection {
    pub source_lineage: u64,
    pub lineage: u64,
    pub sequence: u64,
    pub inventory: Inventory,
}

struct Budget {
    limits: Limits,
    scanned: u64,
    written: u64,
}

impl Budget {
    fn charge(used: &mut u64, bytes: u64, limit: u64, resource: &'static str) -> Result<()> {
        *used = used
            .checked_add(bytes)
            .filter(|n| *n <= limit)
            .ok_or(ProjectionError::BudgetExceeded { resource, limit })?;
        Ok(())
    }

    fn scan(&mut self, bytes: u64) -> Result<()> {
        Self::charge(&mut self.scanned, bytes, self.limits.max_scan_bytes, "scan")
    }

    fn write(&mut self, file: &mut File, bytes: &[u8]) -> Result<()> {
        Self::charge(
            &mut self.written,
            bytes.len() as u64,
            self.limits.max_write_bytes,
            "write",
        )?;
        file.write_all(bytes)?;
        Ok(())
    }
}

/// Projects a closed ordinary V5 log, preserving tenant-qualified names and
/// retained revisions. Checkpoint restatements are filtered, not turned into
/// commits; only omitted suffix commits receive default-collection no-ops.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn project(
    source: &Path,
    tenant: &str,
    out: &Path,
    limits: Limits,
) -> Result<Projection> {
    if tenant.len() != 32 || !tenant.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ProjectionError::InvalidTenant);
    }
    // symlink_metadata also refuses dangling symlinks, without following them.
    match std::fs::symlink_metadata(out) {
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "tenant projection destination exists",
            )
            .into());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let prefix = format!("t{tenant}__");
    let mut budget = Budget {
        limits,
        scanned: 0,
        written: 0,
    };
    let mut input = crate::maintenance::source(source)?;
    let length = input.seek(SeekFrom::End(0))?;
    // Reserve the entire filtering pass before allocating any frame body.
    budget.scan(length)?;
    input.seek(SeekFrom::Start(0))?;
    let mut raw = [0; 32];
    input.read_exact(&mut raw)?;
    check_profile(&raw)?;
    // Header preflight reads the header once more than the filtering pass.
    budget.scan(32)?;
    let header = format::read_log_header(&mut input, None)?;
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut stage = tempfile::NamedTempFile::new_in(parent)?;
    let mut lineage = format::new_lineage()?;
    while Some(lineage) == header.lineage {
        lineage = format::new_lineage()?;
    }
    budget.write(
        stage.as_file_mut(),
        &format::header_v5_flags(1, lineage, 0, header.flags, None),
    )?;
    let mut ownership = Ownership::default();
    let mut offset = header.start;
    let mut floor = 0;
    while let Some(frame) = format::next_checked_frame(&mut input, &header, offset, None)? {
        let at = offset;
        offset += frame.disk.len() as u64;
        if frame.operation == format::OP_TIME_ANCHOR {
            return Err(ProjectionError::Profile(Profile::Timestamped));
        }
        if frame.operation == format::OP_ADVANCE {
            if at != header.start || header.flags & format::FLAG_HISTORY == 0 {
                return Err(format::corrupt(at, "invalid advance frame").into());
            }
            floor = frame.sequence;
            budget.write(stage.as_file_mut(), &frame.disk)?;
            continue;
        }
        if !format::frame_allowed(header.version, header.flags, &frame.plain[..36]) {
            return Err(ProjectionError::Profile(Profile::UnknownOperation(
                frame.operation,
            )));
        }
        if ownership.select(&frame, header.flags, at, &prefix)? {
            budget.write(stage.as_file_mut(), &frame.disk)?;
        } else if frame.sequence > floor {
            budget.write(
                stage.as_file_mut(),
                &format::record_frame(frame.sequence, 0, 0, None),
            )?;
        }
    }
    if offset != length {
        return Err(ProjectionError::Profile(Profile::IncompleteTail));
    }
    // Full replay validates sequence/image order, batch contents, references,
    // and semantic constraints that the raw frame walker deliberately does not.
    budget.scan(length)?;
    let loaded = format::load(&mut input)?;
    if loaded.incomplete_bytes != 0 {
        return Err(ProjectionError::Profile(Profile::IncompleteTail));
    }
    let expected = inventory(&loaded, &prefix)?;
    let sequence = loaded.sequence;
    let source_lineage = header.lineage.expect("V5 header has lineage");
    drop(loaded);
    let stage_len = stage.as_file().metadata()?.len();
    budget.scan(stage_len)?;
    stage.as_file_mut().seek(SeekFrom::Start(0))?;
    verify(&mut File::open(stage.path())?, &prefix, sequence, &expected)?;
    // Reserve the publication copy, its replay, and the final read-only reopen
    // before making the destination visible.
    Budget::charge(
        &mut budget.written,
        stage_len,
        limits.max_write_bytes,
        "write",
    )?;
    budget.scan(stage_len)?;
    budget.scan(stage_len)?;
    budget.scan(stage_len)?;
    let published = crate::maintenance::publish_with(out, None, |output| {
        std::io::copy(stage.as_file_mut(), output)?;
        Ok(())
    });
    match published {
        Ok(published) => published,
        Err(error @ crate::Error::PublicationIndeterminate { .. }) => {
            return Err(remove_failed(out, error.into()));
        }
        Err(error) => return Err(error.into()),
    };
    let verification = crate::maintenance::source(out)
        .map_err(ProjectionError::from)
        .and_then(|mut file| verify(&mut file, &prefix, sequence, &expected));
    if let Err(error) = verification {
        return Err(remove_failed(out, error));
    }
    Ok(Projection {
        source_lineage,
        lineage,
        sequence,
        inventory: expected,
    })
}

fn remove_failed(out: &Path, original: ProjectionError) -> ProjectionError {
    match std::fs::remove_file(out) {
        Ok(()) => original,
        Err(source) => ProjectionError::Cleanup {
            original: Box::new(original),
            source,
        },
    }
}

fn check_profile(raw: &[u8; 32]) -> Result<()> {
    if &raw[..8] != format::MAGIC
        || format::crc32(&raw[..28]) != u32::from_le_bytes(raw[28..32].try_into().unwrap())
    {
        // The format reader supplies the standard corruption diagnostic.
        format::read_log_header(&mut std::io::Cursor::new(raw), None)?;
    }
    let version = u32::from_le_bytes(raw[8..12].try_into().unwrap());
    if version != format::V5 {
        return Err(ProjectionError::Profile(Profile::Version(version)));
    }
    let flags = u16::from_le_bytes(raw[26..28].try_into().unwrap());
    let unsupported = if flags & format::FLAG_ENCRYPTED != 0 {
        Some(Profile::Encrypted)
    } else if flags & format::FLAG_TIMESTAMPS != 0 {
        Some(Profile::Timestamped)
    } else if flags & format::FLAG_PRECISION != 0 {
        Some(Profile::Precision)
    } else if flags & format::FLAG_COMPACT != 0 {
        Some(Profile::Compact)
    } else if flags & format::FLAG_TEMPORAL != 0 && !crate::property_graph::temporal::supported() {
        Some(Profile::TemporalUnavailable)
    } else {
        let unknown = flags & !(format::FLAG_GRAPH | format::FLAG_HISTORY | format::FLAG_TEMPORAL);
        (unknown != 0).then_some(Profile::UnknownFlags(unknown))
    };
    if let Some(reason) = unsupported {
        return Err(ProjectionError::Profile(reason));
    }
    Ok(())
}

fn owned(name: &str, prefix: &str) -> bool {
    name.starts_with(prefix)
}

#[derive(Default)]
struct Ownership {
    collections: BTreeMap<u16, bool>,
    graphs: BTreeMap<crate::property_graph::engine::model::GraphId, bool>,
}

impl Ownership {
    fn select(&mut self, frame: &RawFrame, flags: u16, at: u64, prefix: &str) -> Result<bool> {
        match frame.operation {
            5 => {
                let (name, _, _) = format::decode_create(frame.body(), at)?;
                let selected = owned(&name, prefix);
                self.collections.insert(frame.id as u16, selected);
                Ok(selected)
            }
            6 => Ok(self.collections.remove(&(frame.id as u16)).unwrap_or(false)),
            OP_GRAPH | format::OP_GRAPH_SNAPSHOT => {
                let body = crate::property_graph::temporal::decode_logged(flags, frame, at)?;
                let mut selected = None;
                for entry in &body.entries {
                    if let GraphEntry::CreateGraph { name } = entry {
                        self.graphs.insert(body.graph_id, owned(name, prefix));
                    }
                    let owner = *self
                        .graphs
                        .get(&body.graph_id)
                        .ok_or_else(|| format::corrupt(at, "unknown graph in projection"))?;
                    if selected.is_some_and(|previous| previous != owner) {
                        return Err(ProjectionError::CrossTenantReference {
                            sequence: frame.sequence,
                        });
                    }
                    selected = Some(owner);
                    match entry {
                        GraphEntry::Attach { collection, .. }
                        | GraphEntry::Detach { collection, .. }
                        | GraphEntry::VectorMutation { collection, .. } => {
                            let resource_owner =
                                self.collections.get(collection).copied().unwrap_or(false);
                            if resource_owner != owner {
                                return Err(ProjectionError::CrossTenantReference {
                                    sequence: frame.sequence,
                                });
                            }
                        }
                        GraphEntry::DropGraph => {
                            self.graphs.remove(&body.graph_id);
                        }
                        _ => {}
                    }
                }
                selected.ok_or_else(|| format::corrupt(at, "empty graph body").into())
            }
            1..=4 | 7..=10 | format::OP_RECORD_BLOCK => Ok(self
                .collections
                .get(&frame.collection)
                .copied()
                .unwrap_or(false)),
            operation => Err(ProjectionError::Profile(Profile::UnknownOperation(
                operation,
            ))),
        }
    }
}

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn inventory(loaded: &format::Loaded, prefix: &str) -> Result<Inventory> {
    let mut resources = BTreeMap::<Resource, [u8; 32]>::new();
    for (&id, collection) in &loaded.collections {
        if !owned(&collection.name, prefix) {
            continue;
        }
        let mut identity = Sha256::new();
        field(&mut identity, collection.name.as_bytes());
        field(&mut identity, &(collection.dimensions as u64).to_le_bytes());
        resources.insert(Resource::Collection(id), identity.finalize().into());
        let mut settings = Sha256::new();
        field(
            &mut settings,
            &format::encode_settings(&collection.settings),
        );
        resources.insert(Resource::Settings(id), settings.finalize().into());
        let mut records = Sha256::new();
        for (&record_id, record) in &collection.records {
            field(&mut records, &record_id.to_le_bytes());
            field(
                &mut records,
                &collection.revisions[&record_id].to_le_bytes(),
            );
            field(&mut records, &(record.vector.len() as u64).to_le_bytes());
            for value in &record.vector {
                field(&mut records, &value.to_bits().to_le_bytes());
            }
            field(&mut records, &record.payload);
            field(&mut records, &crate::metadata::encode(&record.metadata));
            let expiry = collection.expiries.get(&record_id);
            field(&mut records, &[u8::from(expiry.is_some())]);
            field(&mut records, &expiry.copied().unwrap_or(0).to_le_bytes());
        }
        resources.insert(Resource::Records(id), records.finalize().into());
        let mut vectors = Sha256::new();
        field(&mut vectors, &collection.vectors.encode()?);
        resources.insert(Resource::Vectors(id), vectors.finalize().into());
    }
    if let Some(graphs) = &loaded.graph {
        let mut hashes = BTreeMap::new();
        for graph in graphs.graphs().filter(|g| owned(g.name(), prefix)) {
            let mut hash = Sha256::new();
            field(&mut hash, &graph.graph_sequence().to_le_bytes());
            let (nodes, edges) = crate::property_graph::frame::kept_revisions(graph, None);
            for pairs in [nodes, edges] {
                field(&mut hash, &(pairs.len() as u64).to_le_bytes());
                for (id, revision) in pairs {
                    field(&mut hash, &id.to_le_bytes());
                    field(&mut hash, &revision.to_le_bytes());
                }
            }
            hashes.insert(graph.id(), hash);
        }
        // The existing canonical binary encoder sorts graph/node/edge IDs,
        // labels and property keys and stores floats as IEEE bits.
        crate::property_graph::frame::compacted_bodies(
            graphs,
            None,
            loaded.sequence,
            |id, count, bytes| {
                if let Some(hash) = hashes.get_mut(&id) {
                    field(hash, &count.to_le_bytes());
                    field(hash, bytes);
                }
                Ok(())
            },
        )?;
        for (id, hash) in hashes {
            resources.insert(Resource::Graph(id.0), hash.finalize().into());
        }
    }
    let selected_imports: BTreeSet<_> = loaded
        .imports
        .iter()
        .filter_map(|event| match event {
            format::LoadedImportMarker::Marker {
                collection, name, ..
            } if owned(name, prefix) => Some(*collection),
            _ => None,
        })
        .collect();
    let mut imports = BTreeMap::<u16, Sha256>::new();
    for event in &loaded.imports {
        match event {
            format::LoadedImportMarker::Marker {
                collection,
                name,
                marker,
            } if owned(name, prefix) => {
                let hash = imports.entry(*collection).or_default();
                field(hash, &[0]);
                field(hash, name.as_bytes());
                for value in [
                    marker.job,
                    marker.chunk,
                    marker.rows_committed,
                    marker.rows_skipped,
                ] {
                    field(hash, &value.to_le_bytes());
                }
                field(hash, &marker.flags.to_le_bytes());
                field(hash, &marker.digest);
            }
            format::LoadedImportMarker::CollectionDropped(id) if selected_imports.contains(id) => {
                field(imports.entry(*id).or_default(), &[1]);
            }
            format::LoadedImportMarker::Image(_) => {
                return Err(ProjectionError::Profile(Profile::Precision));
            }
            _ => {}
        }
    }
    for (id, hash) in imports {
        resources.insert(Resource::Imports(id), hash.finalize().into());
    }
    let mut root = Sha256::new();
    field(&mut root, b"vlite.tenant-inventory/1");
    for (resource, digest) in &resources {
        let (tag, id): (u8, u16) = match resource {
            Resource::Collection(id) => (0, *id),
            Resource::Settings(id) => (1, *id),
            Resource::Records(id) => (2, *id),
            Resource::Vectors(id) => (3, *id),
            Resource::Graph(id) => (4, *id),
            Resource::Imports(id) => (5, *id),
        };
        field(&mut root, &[tag]);
        field(&mut root, &id.to_le_bytes());
        field(&mut root, digest);
    }
    Ok(Inventory {
        resources,
        root: root.finalize().into(),
    })
}

fn verify(
    file: &mut (impl Read + Seek),
    prefix: &str,
    sequence: u64,
    expected: &Inventory,
) -> Result<()> {
    let loaded = format::load(file)?;
    crate::maintenance::require_complete(&loaded)?;
    let default = &loaded.collections[&0];
    if loaded.sequence != sequence
        || !default.records.is_empty()
        || default.dimensions != 1
        || default.settings != crate::CollectionSettings::default()
        || loaded
            .collections
            .iter()
            .any(|(&id, c)| id != 0 && !owned(&c.name, prefix))
        || loaded
            .graph
            .as_ref()
            .is_some_and(|state| state.graphs().any(|g| !owned(g.name(), prefix)))
        || inventory(&loaded, prefix)? != *expected
    {
        return Err(ProjectionError::InventoryMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CollectionSpec, CreateOptions, Database, Metadata};
    use std::fs::{self, File, OpenOptions};

    const TENANT: &str = "11111111111111111111111111111111";
    const FOREIGN: &str = "22222222222222222222222222222222";
    const CANARY: &[u8] = b"FOREIGN-PAYLOAD-MUST-NOT-TRAVEL";

    fn name(tenant: &str, local: &str) -> String {
        format!("t{tenant}__{local}")
    }

    fn limits() -> Limits {
        Limits {
            max_scan_bytes: 16 << 20,
            max_write_bytes: 16 << 20,
        }
    }

    fn collection(db: &mut Database, name: &str) -> u16 {
        db.create_collection(
            name,
            CollectionSpec {
                dimensions: 2,
                settings: Default::default(),
            },
        )
        .unwrap()
        .id
    }

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.vlite");
        let out = dir.path().join("tenant.vlite");
        let mut db = Database::create(&source, 7).unwrap();
        db.put(0, &[1.0; 7], CANARY).unwrap();
        collection(&mut db, &name(FOREIGN, "other"));
        collection(&mut db, &name(TENANT, "data"));
        db.collection_mut(&name(FOREIGN, "other"))
            .unwrap()
            .put(9, &[1.0, 0.0], CANARY)
            .unwrap();
        db.collection_mut(&name(TENANT, "data"))
            .unwrap()
            .put(42, &[-0.0, 1.0], b"selected")
            .unwrap();
        collection(&mut db, &name(TENANT, "empty"));
        db.close().unwrap();
        (dir, source, out)
    }

    fn loaded(path: &Path) -> format::Loaded {
        format::load(&mut File::open(path).unwrap()).unwrap()
    }

    #[test]
    fn two_tenants_project_only_selected() {
        let (_dir, source, out) = fixture();
        let before = fs::read(&source).unwrap();
        project(&source, TENANT, &out, limits()).unwrap();
        let db = Database::open(&out).unwrap();
        assert!(db.is_empty());
        assert!(db.collection(&name(FOREIGN, "other")).is_err());
        assert_eq!(
            db.collection(&name(TENANT, "data"))
                .unwrap()
                .get(42)
                .unwrap()
                .unwrap()
                .payload,
            b"selected"
        );
        db.close().unwrap();
        assert_eq!(fs::read(source).unwrap(), before);
    }

    #[test]
    fn near_prefix_collision_is_not_selected() {
        assert!(owned("t1__data", "t1__"));
        assert!(!owned("t10__data", "t1__"));
        let (_dir, source, out) = fixture();
        let near = format!("{TENANT}0");
        let mut db = Database::open(&source).unwrap();
        collection(&mut db, &name(&near, "data"));
        db.close().unwrap();
        project(&source, TENANT, &out, limits()).unwrap();
        assert!(
            loaded(&out)
                .collections
                .values()
                .all(|c| c.name != name(&near, "data"))
        );
    }

    #[test]
    fn foreign_canary_absent_from_output_bytes() {
        let (_dir, source, out) = fixture();
        project(&source, TENANT, &out, limits()).unwrap();
        let bytes = fs::read(out).unwrap();
        assert!(!bytes.windows(CANARY.len()).any(|w| w == CANARY));
        assert!(
            !bytes
                .windows(FOREIGN.len())
                .any(|w| w == FOREIGN.as_bytes())
        );
    }

    #[test]
    fn selected_empty_collection_kept() {
        let (_dir, source, out) = fixture();
        project(&source, TENANT, &out, limits()).unwrap();
        assert!(
            loaded(&out)
                .collections
                .values()
                .any(|c| c.name == name(TENANT, "empty") && c.records.is_empty())
        );
    }

    #[test]
    fn sparse_ids_preserved() {
        let (_dir, source, out) = fixture();
        project(&source, TENANT, &out, limits()).unwrap();
        let source = loaded(&source);
        let output = loaded(&out);
        assert_eq!(
            output.collections.keys().copied().collect::<Vec<_>>(),
            [0, 2, 3]
        );
        assert_eq!(
            output.collections[&2].revisions,
            source.collections[&2].revisions
        );
        assert_eq!(output.collection_floor, 0);
    }

    #[test]
    fn dropped_and_recreated_name_tracks_lifetime() {
        let (_dir, source, out) = fixture();
        let mut db = Database::open(&source).unwrap();
        let local = name(TENANT, "again");
        let old = collection(&mut db, &local);
        db.drop_collection(&local).unwrap();
        let new = collection(&mut db, &local);
        db.close().unwrap();
        project(&source, TENANT, &out, limits()).unwrap();
        let output = loaded(&out);
        assert!(!output.collections.contains_key(&old));
        assert_eq!(output.collections[&new].name, local);
    }

    #[test]
    fn graph_id_reuse_across_tenant_lifetimes() {
        let (_dir, source, out) = fixture();
        let mut db = Database::open(&source).unwrap();
        let selected = name(TENANT, "g");
        let foreign = name(FOREIGN, "g");
        db.create_graph(&foreign).unwrap();
        let old = db
            .graph
            .as_ref()
            .unwrap()
            .graph_by_name(&foreign)
            .unwrap()
            .id();
        db.drop_graph(&foreign).unwrap();
        db.create_graph(&selected).unwrap();
        let new = db
            .graph
            .as_ref()
            .unwrap()
            .graph_by_name(&selected)
            .unwrap()
            .id();
        assert_eq!(old, new);
        db.graph_mut(&selected)
            .unwrap()
            .create_node("kept", &["Node"], &Metadata::new())
            .unwrap();
        db.close().unwrap();
        project(&source, TENANT, &out, limits()).unwrap();
        let db = Database::open(&out).unwrap();
        assert!(db.graph(&selected).is_ok());
        assert!(db.graph(&foreign).is_err());
        db.close().unwrap();
    }

    #[test]
    fn cross_tenant_graph_reference_refused() {
        for incoming in [false, true] {
            let (_dir, source, out) = fixture();
            let mut db = Database::open(&source).unwrap();
            let graph = name(if incoming { FOREIGN } else { TENANT }, "g");
            let resource = name(
                if incoming { TENANT } else { FOREIGN },
                if incoming { "data" } else { "other" },
            );
            db.create_graph(&graph).unwrap();
            let mut graph = db.graph_mut(&graph).unwrap();
            graph.create_node("n", &[], &Metadata::new()).unwrap();
            graph
                .attach("n", &resource, if incoming { 42 } else { 9 })
                .unwrap();
            db.close().unwrap();
            assert!(matches!(
                project(&source, TENANT, &out, limits()),
                Err(ProjectionError::CrossTenantReference { .. })
            ));
            assert!(!out.exists());
        }
    }

    #[test]
    fn leading_advance_kept_and_sequence_continuous() {
        let (dir, source, out) = fixture();
        let state = loaded(&source);
        let image = dir.path().join("image.vlite");
        let mut file = File::create_new(&image).unwrap();
        crate::maintenance::write_v5_image(
            &mut file,
            &state,
            state.lineage.unwrap(),
            Some(0),
            Some(0),
            None,
            state.flags & format::FLAG_GRAPH,
            state.sequence,
        )
        .unwrap();
        drop(file);
        let mut db = Database::open(&image).unwrap();
        db.put(100, &[1.0; 7], CANARY).unwrap();
        db.close().unwrap();
        let report = project(&image, TENANT, &out, limits()).unwrap();
        let output = loaded(&out);
        assert_eq!(output.history_floor, state.sequence);
        assert_eq!(output.sequence, state.sequence + 1);
        assert_eq!(report.sequence, output.sequence);
        assert!(output.collections[&0].records.is_empty());
        let mut input = File::open(&image).unwrap();
        let mut projected = File::open(&out).unwrap();
        let a = format::read_log_header(&mut input, None).unwrap();
        let b = format::read_log_header(&mut projected, None).unwrap();
        assert_eq!(
            format::next_frame(&mut input, &a, a.start)
                .unwrap()
                .unwrap()
                .disk,
            format::next_frame(&mut projected, &b, b.start)
                .unwrap()
                .unwrap()
                .disk,
        );
    }

    #[test]
    fn placeholder_is_a_valid_noop() {
        let mut file = std::io::Cursor::new(format::header_v5_flags(1, 1, 0, 0, None));
        file.get_mut().extend(format::record_frame(1, 0, 0, None));
        file.get_mut().extend(format::record_frame(2, 0, 0, None));
        let state = format::load(&mut file).unwrap();
        assert_eq!(state.sequence, 2);
        assert!(state.collections[&0].records.is_empty());
        assert!(state.collections[&0].revisions.is_empty());
    }

    #[test]
    fn fresh_lineage_differs_from_source() {
        let (_dir, source, out) = fixture();
        let report = project(&source, TENANT, &out, limits()).unwrap();
        assert_ne!(loaded(&source).lineage, loaded(&out).lineage);
        assert_ne!(report.source_lineage, report.lineage);
    }

    #[test]
    fn output_reopens_and_inventory_matches() {
        let (_dir, source, out) = fixture();
        let report = project(&source, TENANT, &out, limits()).unwrap();
        let prefix = format!("t{TENANT}__");
        assert_eq!(
            inventory(&loaded(&source), &prefix).unwrap(),
            report.inventory
        );
        assert_eq!(inventory(&loaded(&out), &prefix).unwrap(), report.inventory);
    }

    #[test]
    fn corrupt_source_refused_nothing_written() {
        let (_dir, source, out) = fixture();
        let mut bytes = fs::read(&source).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(&source, bytes).unwrap();
        assert!(project(&source, TENANT, &out, limits()).is_err());
        assert!(!out.exists());
    }

    #[test]
    fn incomplete_tail_refused() {
        let (_dir, source, out) = fixture();
        OpenOptions::new()
            .append(true)
            .open(&source)
            .unwrap()
            .write_all(b"VRE")
            .unwrap();
        assert!(matches!(
            project(&source, TENANT, &out, limits()),
            Err(ProjectionError::Profile(Profile::IncompleteTail))
        ));
        assert!(!out.exists());
    }

    #[test]
    fn encrypted_source_refused() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("encrypted.vlite");
        let out = dir.path().join("out.vlite");
        Database::create_with_options(
            &source,
            2,
            CreateOptions {
                encryption: Some(crate::EncryptionKey::from_bytes([7; 32])),
                ..Default::default()
            },
        )
        .unwrap()
        .close()
        .unwrap();
        assert!(matches!(
            project(&source, TENANT, &out, limits()),
            Err(ProjectionError::Profile(Profile::Encrypted))
        ));
        assert!(!out.exists());
    }

    #[test]
    fn timestamped_source_refused() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("timestamps.vlite");
        let out = dir.path().join("out.vlite");
        Database::create_with_options(
            &source,
            2,
            CreateOptions {
                commit_timestamps: true,
                ..Default::default()
            },
        )
        .unwrap()
        .close()
        .unwrap();
        assert!(matches!(
            project(&source, TENANT, &out, limits()),
            Err(ProjectionError::Profile(Profile::Timestamped))
        ));
        assert!(!out.exists());
    }

    #[test]
    fn existing_output_refused() {
        let (_dir, source, out) = fixture();
        fs::write(&out, b"existing").unwrap();
        assert!(project(&source, TENANT, &out, limits()).is_err());
        assert_eq!(fs::read(out).unwrap(), b"existing");
    }

    #[test]
    fn budget_exceeded_refused() {
        let (_dir, source, _out) = fixture();
        let source_length = fs::metadata(source).unwrap().len();
        for budget in [
            Limits {
                max_scan_bytes: 1,
                ..limits()
            },
            Limits {
                max_write_bytes: 1,
                ..limits()
            },
            Limits {
                max_scan_bytes: source_length,
                ..limits()
            },
        ] {
            let (_dir, source, out) = fixture();
            assert!(matches!(
                project(&source, TENANT, &out, budget),
                Err(ProjectionError::BudgetExceeded { .. })
            ));
            assert!(!out.exists());
        }
    }

    #[test]
    fn invalid_tenant_refused() {
        let (_dir, source, out) = fixture();
        for tenant in ["", "1", "../tenant", "1111111111111111111111111111111g"] {
            assert!(matches!(
                project(&source, tenant, &out, limits()),
                Err(ProjectionError::InvalidTenant)
            ));
            assert!(!out.exists());
        }
    }

    #[test]
    fn unsupported_versions_flags_and_images_refused() {
        for (version, flags, expected) in [
            (4, 0, Profile::Version(4)),
            (
                5,
                format::FLAG_HISTORY | format::FLAG_COMPACT,
                Profile::Compact,
            ),
            (
                5,
                format::FLAG_HISTORY | format::FLAG_COMPACT | format::FLAG_PRECISION,
                Profile::Precision,
            ),
            (5, 0x8000, Profile::UnknownFlags(0x8000)),
        ] {
            let (_dir, source, out) = fixture();
            let mut bytes = fs::read(&source).unwrap();
            bytes[8..12].copy_from_slice(&u32::to_le_bytes(version));
            bytes[26..28].copy_from_slice(&u16::to_le_bytes(flags));
            let crc = format::crc32(&bytes[..28]);
            bytes[28..32].copy_from_slice(&crc.to_le_bytes());
            fs::write(&source, bytes).unwrap();
            assert!(matches!(
                project(&source, TENANT, &out, limits()),
                Err(ProjectionError::Profile(reason)) if reason == expected
            ));
            assert!(!out.exists());
        }
    }

    #[test]
    fn unknown_operation_and_unflagged_time_anchor_refused() {
        for (operation, expected) in [
            (99, Profile::UnknownOperation(99)),
            (11, Profile::Timestamped),
        ] {
            let (_dir, source, out) = fixture();
            let mut frame = format::record_frame(7, 0, 0, None);
            frame[4] = operation;
            let crc = format::crc32(&frame[..32]);
            frame[32..36].copy_from_slice(&crc.to_le_bytes());
            let footer = frame.len() - 16;
            let crc = format::crc32(&frame[..footer]);
            frame[footer + 4..footer + 8].copy_from_slice(&crc.to_le_bytes());
            OpenOptions::new()
                .append(true)
                .open(&source)
                .unwrap()
                .write_all(&frame)
                .unwrap();
            assert!(matches!(
                project(&source, TENANT, &out, limits()),
                Err(ProjectionError::Profile(reason)) if reason == expected
            ));
            assert!(!out.exists());
        }
    }

    #[test]
    fn selected_frames_byte_identical_and_suffix_contiguous() {
        let (_dir, source, out) = fixture();
        project(&source, TENANT, &out, limits()).unwrap();
        let frames = |path: &Path| {
            let mut file = File::open(path).unwrap();
            let header = format::read_log_header(&mut file, None).unwrap();
            let mut offset = header.start;
            let mut frames = BTreeMap::new();
            while let Some(frame) = format::next_frame(&mut file, &header, offset).unwrap() {
                offset += frame.disk.len() as u64;
                frames.insert(frame.sequence, frame);
            }
            frames
        };
        let original = frames(&source);
        let projected = frames(&out);
        assert_eq!(
            projected.keys().copied().collect::<Vec<_>>(),
            (1..=6).collect::<Vec<_>>()
        );
        for (sequence, frame) in original {
            if frame.collection == 2 || (frame.operation == 5 && matches!(frame.id, 2 | 3)) {
                assert_eq!(projected[&sequence].disk, frame.disk);
            } else {
                assert_eq!(
                    projected[&sequence].disk,
                    format::record_frame(sequence, 0, 0, None)
                );
            }
        }
    }

    #[test]
    fn metadata_expiry_extensions_settings_and_import_history_preserved() {
        use crate::vectors::{RecordVectors, VectorSpec, VersionedVector};
        use crate::{DistanceMetric, InvalidRowPolicy, MetadataValue, Mutation};
        let (_dir, source, out) = fixture();
        let mut db = Database::open(&source).unwrap();
        let data = name(TENANT, "data");
        let mut c = db.collection_mut(&data).unwrap();
        c.put_with_expiry(
            77,
            &[-0.0, 0.125],
            b"retained-even-if-expired",
            &Metadata::from([("float".into(), MetadataValue::Float(-0.0))]),
            Some(1),
        )
        .unwrap();
        c.configure_vector(
            "embedding",
            VectorSpec {
                dimensions: 2,
                metric: DistanceMetric::Cosine,
                active_version: "v1".into(),
                history: 1,
            },
        )
        .unwrap();
        c.put_vectors(
            42,
            RecordVectors {
                dense: BTreeMap::from([(
                    "embedding".into(),
                    vec![VersionedVector {
                        version: "v1".into(),
                        vector: vec![-0.0, 0.75],
                    }],
                )]),
                sparse: BTreeMap::from([("terms".into(), vec![(3, 0.5), (500, -0.0)])]),
            },
        )
        .unwrap();
        c.import_chunk(
            100,
            1,
            &[Mutation::Put {
                id: 100,
                vector: vec![0.5, 0.25],
                payload: b"imported".to_vec(),
            }],
            InvalidRowPolicy::RejectChunk,
        )
        .unwrap();
        c.finish_import(100).unwrap();
        db.create_collection(
            &name(TENANT, "configured"),
            CollectionSpec {
                dimensions: 3,
                settings: crate::CollectionSettings {
                    metric: DistanceMetric::Cosine,
                    ..Default::default()
                },
            },
        )
        .unwrap();
        db.close().unwrap();
        let expected = loaded(&source);
        let report = project(&source, TENANT, &out, limits()).unwrap();
        let output = loaded(&out);
        assert_eq!(
            output.collections[&2].expiries,
            expected.collections[&2].expiries
        );
        assert_eq!(
            output.collections[&2].revisions,
            expected.collections[&2].revisions
        );
        assert_eq!(
            output.collections[&2].vectors.encode().unwrap(),
            expected.collections[&2].vectors.encode().unwrap()
        );
        assert_eq!(output.imports, expected.imports);
        assert!(
            report
                .inventory
                .resources
                .contains_key(&Resource::Imports(2))
        );
    }

    #[test]
    fn selected_graph_snapshot_and_atomic_vectors_preserved() {
        use crate::Mutation;
        use crate::property_graph::GraphOp;
        let (dir, source, out) = fixture();
        let mut db = Database::open(&source).unwrap();
        let graph = name(TENANT, "g");
        let data = name(TENANT, "data");
        db.create_graph(&name(FOREIGN, "g")).unwrap();
        db.create_graph(&graph).unwrap();
        db.graph_mut(&graph)
            .unwrap()
            .write_batch(&[
                GraphOp::CreateNode {
                    key: "a".into(),
                    labels: vec!["Node".into()],
                    properties: Metadata::new(),
                },
                GraphOp::CreateNode {
                    key: "b".into(),
                    labels: vec![],
                    properties: Metadata::new(),
                },
                GraphOp::CreateEdge {
                    key: Some("edge".into()),
                    source: "a".into(),
                    relationship: "LINK".into(),
                    destination: "b".into(),
                    properties: Metadata::new(),
                },
                GraphOp::Vector {
                    collection: data.clone(),
                    mutation: Mutation::Put {
                        id: 200,
                        vector: vec![0.5, 0.5],
                        payload: b"atomic".to_vec(),
                    },
                },
                GraphOp::Attach {
                    node: "a".into(),
                    collection: data.clone(),
                    record: 200,
                },
            ])
            .unwrap();
        db.close().unwrap();
        project(&source, TENANT, &out, limits()).unwrap();
        let state = loaded(&source);
        let image = dir.path().join("graph-image.vlite");
        let mut file = File::create_new(&image).unwrap();
        crate::maintenance::write_v5_image(
            &mut file,
            &state,
            state.lineage.unwrap(),
            Some(0),
            Some(0),
            None,
            state.flags & format::FLAG_GRAPH,
            state.sequence,
        )
        .unwrap();
        drop(file);
        let image_out = dir.path().join("graph-projected.vlite");
        let report = project(&image, TENANT, &image_out, limits()).unwrap();
        let graph_id = state
            .graph
            .as_ref()
            .unwrap()
            .graph_by_name(&graph)
            .unwrap()
            .id()
            .0;
        assert!(
            report
                .inventory
                .resources
                .contains_key(&Resource::Graph(graph_id))
        );
        assert_eq!(
            inventory(&loaded(&image), &format!("t{TENANT}__")).unwrap(),
            inventory(&loaded(&image_out), &format!("t{TENANT}__")).unwrap(),
        );
    }

    #[test]
    fn cross_tenant_atomic_vector_refused_even_without_attachment() {
        use crate::Mutation;
        use crate::property_graph::GraphOp;
        for incoming in [false, true] {
            let (_dir, source, out) = fixture();
            let mut db = Database::open(&source).unwrap();
            let graph = name(if incoming { FOREIGN } else { TENANT }, "g");
            let data = name(
                if incoming { TENANT } else { FOREIGN },
                if incoming { "data" } else { "other" },
            );
            db.create_graph(&graph).unwrap();
            db.graph_mut(&graph)
                .unwrap()
                .write_batch(&[GraphOp::Vector {
                    collection: data,
                    mutation: Mutation::Put {
                        id: 200,
                        vector: vec![1.0, 0.0],
                        payload: b"cross".to_vec(),
                    },
                }])
                .unwrap();
            db.close().unwrap();
            assert!(matches!(
                project(&source, TENANT, &out, limits()),
                Err(ProjectionError::CrossTenantReference { .. })
            ));
            assert!(!out.exists());
        }
    }

    #[test]
    fn publication_race_does_not_replace_destination() {
        let (_dir, source, out) = fixture();
        let racer = out.clone();
        let _armed = crate::maintenance::failpoint::arm(move |stage| {
            if stage == crate::maintenance::failpoint::Stage::Staged {
                fs::write(&racer, b"raced")?;
            }
            Ok(())
        });
        assert!(project(&source, TENANT, &out, limits()).is_err());
        assert_eq!(fs::read(out).unwrap(), b"raced");
    }

    #[test]
    fn publication_failure_removes_visible_output() {
        let (_dir, source, out) = fixture();
        let _armed = crate::maintenance::failpoint::arm(|stage| {
            if stage == crate::maintenance::failpoint::Stage::Visible {
                return Err(std::io::Error::other("injected post-publication failure"));
            }
            Ok(())
        });
        assert!(project(&source, TENANT, &out, limits()).is_err());
        assert!(!out.exists());
    }

    #[test]
    fn kv_namespace_is_not_an_ownership_signal() {
        let (_dir, source, out) = fixture();
        let mut db = Database::open(&source).unwrap();
        let selected = name(TENANT, "kv");
        let foreign = name(FOREIGN, "kv");
        for collection in [&selected, &foreign] {
            db.create_collection(
                collection,
                CollectionSpec {
                    dimensions: 1,
                    settings: Default::default(),
                },
            )
            .unwrap();
        }
        let revision = db
            .kv_store(&selected)
            .unwrap()
            .put("application", "key", b"kept", Default::default())
            .unwrap();
        db.kv_store(&foreign)
            .unwrap()
            .put(TENANT, "key", CANARY, Default::default())
            .unwrap();
        db.close().unwrap();
        project(&source, TENANT, &out, limits()).unwrap();
        let db = Database::open(&out).unwrap();
        let entry = db
            .kv_view(&selected)
            .unwrap()
            .get("application", "key")
            .unwrap()
            .unwrap();
        assert_eq!(entry.value, b"kept");
        assert_eq!(entry.version, revision);
        assert!(db.kv_view(&foreign).is_err());
        db.close().unwrap();
        assert!(
            !fs::read(out)
                .unwrap()
                .windows(CANARY.len())
                .any(|bytes| bytes == CANARY)
        );
    }

    #[test]
    fn temporal_history_preserved_only_when_capability_supported() {
        use crate::property_graph::{GraphOp, TemporalOp, TemporalSegment};
        let activation = crate::property_graph::temporal::Activation::on();
        let (dir, source, out) = fixture();
        let temporal = dir.path().join("temporal.vlite");
        crate::property_graph::temporal::enable_temporal(&source, &temporal, None).unwrap();
        let graph = name(TENANT, "temporal");
        let mut db = Database::open(&temporal).unwrap();
        db.create_graph(&graph).unwrap();
        db.graph_mut(&graph)
            .unwrap()
            .write_batch(&[
                GraphOp::CreateNode {
                    key: "a".into(),
                    labels: vec![],
                    properties: Metadata::new(),
                },
                GraphOp::CreateNode {
                    key: "b".into(),
                    labels: vec![],
                    properties: Metadata::new(),
                },
                GraphOp::Temporal(TemporalOp::CreateEdge {
                    key: Some("edge".into()),
                    source: "a".into(),
                    relationship: "LINK".into(),
                    destination: "b".into(),
                    segments: vec![TemporalSegment {
                        valid_from: Some(-1),
                        valid_to: Some(50),
                        properties: Metadata::new(),
                    }],
                }),
            ])
            .unwrap();
        db.close().unwrap();
        project(&temporal, TENANT, &out, limits()).unwrap();
        assert_eq!(
            loaded(&out).flags & format::FLAG_TEMPORAL,
            format::FLAG_TEMPORAL
        );
        drop(activation);
        let refused = dir.path().join("refused.vlite");
        assert!(matches!(
            project(&temporal, TENANT, &refused, limits()),
            Err(ProjectionError::Profile(Profile::TemporalUnavailable))
        ));
        assert!(!refused.exists());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_output_refused() {
        let (dir, source, out) = fixture();
        let absent = dir.path().join("absent.vlite");
        std::os::unix::fs::symlink(&absent, &out).unwrap();
        assert!(project(&source, TENANT, &out, limits()).is_err());
        assert!(fs::symlink_metadata(out).unwrap().file_type().is_symlink());
        assert!(!absent.exists());
    }
}
