//! Named collections: persisted settings, per-collection state and typed handles.
//!
//! Collection 0 is the implicit `default` collection and is what every legacy
//! `Database` method operates on. Named collections require format v4.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    Condition, Database, DistanceMetric, Expected, HnswConfig, HnswMaintenancePolicy,
    HnswMaintenanceReport, HnswMaintenanceStatus, HnswRebuildLimits, HnswRebuildStatus, Metadata,
    Mutation, Record, Result, SearchHit, SearchStats, format, hnsw,
    metadata_index::{MAX_METADATA_INDEXES, MetadataIndexSpec, MetadataIndexes},
    planner::PlannerPolicy,
    quantize::Quantization,
    text_index::{MAX_TEXT_FIELDS, TextIndex},
};

const MAX_FIELD_NAME_BYTES: usize = 256;

/// Persisted per-collection defaults and derived-index declarations.
///
/// Wave 0 stores and validates these settings. Existing explicit-parameter
/// search methods keep their semantics; declared `metadata_indexes` narrow
/// exact filtered scans (identical results), `text_fields` feed hybrid BM25,
/// `planner` drives `search_auto`, and `search_auto_with_settings` also uses
/// `metric`, `hnsw`, `quantization` and `rerank_multiplier`.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionSettings {
    pub metric: DistanceMetric,
    pub hnsw: HnswConfig,
    pub quantization: Quantization,
    /// Valid in `1..=16`.
    pub rerank_multiplier: u8,
    pub planner: PlannerPolicy,
    /// At most 16; field names are 1..=256 UTF-8 bytes and unique.
    pub metadata_indexes: Vec<MetadataIndexSpec>,
    /// At most 8; field names are 1..=256 UTF-8 bytes and unique.
    pub text_fields: Vec<String>,
}

impl Default for CollectionSettings {
    fn default() -> Self {
        Self {
            metric: DistanceMetric::SquaredL2,
            hnsw: HnswConfig::default(),
            quantization: Quantization::None,
            rerank_multiplier: 4,
            planner: PlannerPolicy::default(),
            metadata_indexes: Vec::new(),
            text_fields: Vec::new(),
        }
    }
}

fn valid_field(field: &str) -> bool {
    (1..=MAX_FIELD_NAME_BYTES).contains(&field.len())
}

impl CollectionSettings {
    pub(crate) fn validate(&self) -> std::result::Result<(), &'static str> {
        if self.hnsw.validate().is_err() {
            return Err("HNSW m must be 2..=64 and ef_construction must be m..=4096");
        }
        if !(1..=16).contains(&self.rerank_multiplier) {
            return Err("rerank_multiplier must be 1..=16");
        }
        self.planner.validate()?;
        if self.metadata_indexes.len() > MAX_METADATA_INDEXES {
            return Err("at most 16 metadata indexes may be declared");
        }
        if self.text_fields.len() > MAX_TEXT_FIELDS {
            return Err("at most 8 text fields may be declared");
        }
        for (position, spec) in self.metadata_indexes.iter().enumerate() {
            if !valid_field(&spec.field) {
                return Err("metadata index field names must be 1..=256 bytes");
            }
            if self.metadata_indexes[..position]
                .iter()
                .any(|other| other.field == spec.field)
            {
                return Err("duplicate metadata index field");
            }
        }
        for (position, field) in self.text_fields.iter().enumerate() {
            if !valid_field(field) {
                return Err("text field names must be 1..=256 bytes");
            }
            if self.text_fields[..position].contains(field) {
                return Err("duplicate text field");
            }
        }
        Ok(())
    }
}

/// Parameters for `Database::create_collection`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionSpec {
    /// Vector dimensions, `1..=MAX_DIMENSIONS`.
    pub dimensions: usize,
    pub settings: CollectionSettings,
}

/// A snapshot description of one live collection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionInfo {
    /// Stable on-disk collection ID; `default` is 0.
    pub id: u16,
    pub name: String,
    pub dimensions: usize,
    pub live_records: usize,
    pub settings: CollectionSettings,
    /// False when the settings are the implicit defaults or memory-only (v1–v3).
    pub settings_persisted: bool,
}

/// Outcome of `Database::set_collection_settings`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsPersistence {
    /// A synced v4 settings frame was committed.
    Persisted,
    /// v1–v3 default collection: applied to this handle only, lost on reopen.
    MemoryOnly,
}

/// In-memory state of one collection. Derived indexes are never written to the
/// datafile; the HNSW index may only be cached in an explicit, revalidated
/// ANN snapshot sidecar (`Database::save_ann_snapshot`).
pub(crate) struct Collection {
    pub name: String,
    pub dimensions: usize,
    pub settings: CollectionSettings,
    pub settings_persisted: bool,
    pub records: BTreeMap<u64, Record>,
    /// Commit sequence that last put each live record (its revision).
    pub revisions: BTreeMap<u64, u64>,
    pub live_body_bytes: u64,
    pub index: Option<hnsw::Index>,
    pub metadata_indexes: MetadataIndexes,
    pub text_index: TextIndex,
}

impl Collection {
    pub(crate) fn from_loaded(loaded: format::LoadedCollection) -> Self {
        let mut collection = Self {
            name: loaded.name,
            dimensions: loaded.dimensions,
            settings: loaded.settings,
            settings_persisted: loaded.settings_persisted,
            records: loaded.records,
            revisions: loaded.revisions,
            live_body_bytes: loaded.live_body_bytes,
            index: None,
            metadata_indexes: MetadataIndexes::default(),
            text_index: TextIndex::default(),
        };
        collection.rebuild_derived();
        collection
    }

    pub(crate) fn empty(name: String, dimensions: usize, settings: CollectionSettings) -> Self {
        Self::from_loaded(format::LoadedCollection::new(name, dimensions, settings))
    }

    /// Exact top-k restricted to `candidates` when given (planner input for
    /// EPIC-005). Query, filter and metric must already be validated.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn exact_search_candidates(
        &self,
        query: &[f32],
        query_norm: f64,
        k: usize,
        conditions: &[Condition],
        metric: DistanceMetric,
        candidates: Option<&BTreeSet<u64>>,
        stats: &mut SearchStats,
    ) -> Vec<SearchHit> {
        crate::exact_search(
            &self.records,
            query,
            query_norm,
            k,
            conditions,
            metric,
            candidates,
            stats,
        )
    }

    pub(crate) fn rebuild_derived(&mut self) {
        self.metadata_indexes
            .rebuild(&self.settings.metadata_indexes, &self.records);
        self.text_index
            .rebuild(&self.settings.text_fields, &self.records);
    }

    /// Applies one durably committed mutation to all in-memory state.
    ///
    /// Derived structures observe the previous record before replacement.
    pub(crate) fn apply_committed(
        &mut self,
        id: u64,
        record: Option<Record>,
        sequence: u64,
        vector_bytes: usize,
    ) {
        if let Some(index) = &mut self.index
            && !index.track(id, self.records.get(&id), vector_bytes)
        {
            self.index = None;
        }
        let previous = self.records.get(&id).map(|record| &record.metadata);
        let current = record.as_ref().map(|record| &record.metadata);
        self.metadata_indexes.track(id, previous, current);
        self.text_index.track(id, previous, current);
        match record {
            Some(record) => {
                self.live_body_bytes += format::record_body_len(&record);
                if let Some(previous) = self.records.insert(id, record) {
                    self.live_body_bytes -= format::record_body_len(&previous);
                }
                self.revisions.insert(id, sequence);
            }
            None => {
                if let Some(previous) = self.records.remove(&id) {
                    self.live_body_bytes -= format::record_body_len(&previous);
                }
                self.revisions.remove(&id);
            }
        }
    }
}

/// Read-only handle to one collection. Obtain with `Database::collection`.
#[derive(Clone, Copy)]
pub struct CollectionRef<'a> {
    pub(crate) db: &'a Database,
    pub(crate) id: u16,
}

impl<'a> CollectionRef<'a> {
    fn state(&self) -> &'a Collection {
        self.db.state(self.id)
    }

    pub fn id(&self) -> u16 {
        self.id
    }

    pub fn name(&self) -> &'a str {
        &self.state().name
    }

    pub fn dimensions(&self) -> usize {
        self.state().dimensions
    }

    pub fn settings(&self) -> &'a CollectionSettings {
        &self.state().settings
    }

    pub fn len(&self) -> usize {
        self.state().records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.state().records.is_empty()
    }

    pub(crate) fn first_id(&self) -> Option<u64> {
        self.state().records.keys().next().copied()
    }

    pub fn get(&self, id: u64) -> Result<Option<&'a Record>> {
        self.db.get_in(self.id, id)
    }

    /// See `Database::revision`.
    pub fn revision(&self, id: u64) -> Result<Option<u64>> {
        self.db.revision_in(self.id, id)
    }

    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<SearchHit>> {
        self.search_filtered_with_metric(query, k, &[], DistanceMetric::SquaredL2)
    }

    pub fn search_filtered(
        &self,
        query: &[f32],
        k: usize,
        conditions: &[Condition],
    ) -> Result<Vec<SearchHit>> {
        self.search_filtered_with_metric(query, k, conditions, DistanceMetric::SquaredL2)
    }

    pub fn search_with_metric(
        &self,
        query: &[f32],
        k: usize,
        metric: DistanceMetric,
    ) -> Result<Vec<SearchHit>> {
        self.search_filtered_with_metric(query, k, &[], metric)
    }

    /// Same semantics as `Database::search_filtered_with_metric`.
    pub fn search_filtered_with_metric(
        &self,
        query: &[f32],
        k: usize,
        conditions: &[Condition],
        metric: DistanceMetric,
    ) -> Result<Vec<SearchHit>> {
        self.db.search_in(
            self.id,
            query,
            k,
            conditions,
            metric,
            &mut SearchStats::default(),
        )
    }

    /// Exact search that also reports work counters.
    pub fn search_with_stats(
        &self,
        query: &[f32],
        k: usize,
        conditions: &[Condition],
        metric: DistanceMetric,
    ) -> Result<(Vec<SearchHit>, SearchStats)> {
        let mut stats = SearchStats::default();
        let hits = self
            .db
            .search_in(self.id, query, k, conditions, metric, &mut stats)?;
        Ok((hits, stats))
    }

    /// Same semantics as `Database::search_approximate_ready_filtered_with_metric`.
    pub fn search_approximate_ready_filtered_with_metric(
        &self,
        query: &[f32],
        k: usize,
        ef_search: usize,
        config: HnswConfig,
        conditions: &[Condition],
        metric: DistanceMetric,
    ) -> Result<Vec<SearchHit>> {
        self.db.search_ready_in(
            self.id,
            query,
            k,
            ef_search,
            config,
            conditions,
            metric,
            &mut SearchStats::default(),
        )
    }
}

/// Mutable handle to one collection. Obtain with `Database::collection_mut`.
///
/// Every mutation has the same durability contract as the `Database` methods.
pub struct CollectionMut<'a> {
    pub(crate) db: &'a mut Database,
    pub(crate) id: u16,
}

impl CollectionMut<'_> {
    /// Read-only view of the same collection.
    pub fn view(&self) -> CollectionRef<'_> {
        CollectionRef {
            db: self.db,
            id: self.id,
        }
    }

    pub fn put(&mut self, id: u64, vector: &[f32], payload: &[u8]) -> Result<u64> {
        self.put_with_metadata_if(id, vector, payload, &Metadata::new(), Expected::Any)
    }

    pub fn put_with_metadata(
        &mut self,
        id: u64,
        vector: &[f32],
        payload: &[u8],
        metadata: &Metadata,
    ) -> Result<u64> {
        self.put_with_metadata_if(id, vector, payload, metadata, Expected::Any)
    }

    /// See `Database::put_with_metadata_if`.
    pub fn put_with_metadata_if(
        &mut self,
        id: u64,
        vector: &[f32],
        payload: &[u8],
        metadata: &Metadata,
        expected: Expected,
    ) -> Result<u64> {
        self.db
            .put_in(self.id, id, vector, payload, metadata, expected)
    }

    pub fn delete(&mut self, id: u64) -> Result<bool> {
        self.db.delete_in(self.id, id, Expected::Any)
    }

    /// See `Database::delete_if`.
    pub fn delete_if(&mut self, id: u64, expected: Expected) -> Result<bool> {
        self.db.delete_in(self.id, id, expected)
    }

    /// See `Database::write_batch`. Named collections always use v4 frames.
    pub fn write_batch(&mut self, operations: &[Mutation]) -> Result<()> {
        self.db.write_batch_in(self.id, operations)
    }

    pub fn build_hnsw_with_metric(
        &mut self,
        config: HnswConfig,
        metric: DistanceMetric,
    ) -> Result<()> {
        self.db.build_hnsw_in(self.id, config, metric)
    }

    /// Lazily (re)builds like `Database::search_approximate_filtered_with_metric`.
    pub fn search_approximate_filtered_with_metric(
        &mut self,
        query: &[f32],
        k: usize,
        ef_search: usize,
        config: HnswConfig,
        conditions: &[Condition],
        metric: DistanceMetric,
    ) -> Result<Vec<SearchHit>> {
        self.db
            .search_approximate_in(self.id, query, k, ef_search, config, conditions, metric)
    }

    /// One background rebuild exists per database; another collection's job
    /// makes this return `HnswRebuildBusy`.
    pub fn start_hnsw_rebuild(
        &mut self,
        config: HnswConfig,
        metric: DistanceMetric,
        limits: HnswRebuildLimits,
    ) -> Result<()> {
        self.db.start_rebuild_in(self.id, config, metric, limits)
    }

    /// Returns `Idle` if no job exists for this collection.
    pub fn poll_hnsw_rebuild(&mut self) -> Result<HnswRebuildStatus> {
        self.db.poll_rebuild_in(self.id)
    }

    /// Returns false if no job exists for this collection.
    pub fn cancel_hnsw_rebuild(&mut self) -> bool {
        self.db.cancel_rebuild_in(self.id)
    }

    pub fn hnsw_maintenance_status(
        &self,
        config: HnswConfig,
        metric: DistanceMetric,
        policy: HnswMaintenancePolicy,
    ) -> Result<HnswMaintenanceStatus> {
        self.db
            .maintenance_status_in(self.id, config, metric, policy)
    }

    pub fn maintain_hnsw(
        &mut self,
        config: HnswConfig,
        metric: DistanceMetric,
        policy: HnswMaintenancePolicy,
    ) -> Result<HnswMaintenanceReport> {
        self.db.maintain_in(self.id, config, metric, policy)
    }
}
