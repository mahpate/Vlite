//! Per-collection metadata indexes.
//!
//! Declarations are persisted by the collection settings; postings are derived
//! in memory, rebuilt on open or declaration change and maintained only after
//! successful commits. Lookups return a candidate *superset* that callers must
//! re-verify with `metadata::matches`, so indexed and unindexed searches return
//! identical results. Numeric typing follows the filter semantics exactly:
//! `Integer` and `Float` are distinct (`Integer(1)` never matches `Float(1.0)`)
//! and `-0.0` equals `0.0`.

#[cfg(test)]
use crate::Record;
use crate::{Condition, Metadata, MetadataValue};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Maximum declared metadata indexes per collection.
pub const MAX_METADATA_INDEXES: usize = 16;

/// Kind of a declared metadata index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MetadataIndexKind {
    /// Equality postings.
    Eq,
    /// Ordered numeric postings.
    Range,
    /// Both equality and ordered numeric postings.
    EqRange,
}

impl MetadataIndexKind {
    pub(crate) fn code(self) -> u8 {
        match self {
            Self::Eq => 1,
            Self::Range => 2,
            Self::EqRange => 3,
        }
    }

    pub(crate) fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Eq),
            2 => Some(Self::Range),
            3 => Some(Self::EqRange),
            _ => None,
        }
    }
}

/// One declared index: a metadata field name (1..=256 UTF-8 bytes) and kind.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MetadataIndexSpec {
    pub field: String,
    pub kind: MetadataIndexKind,
}

/// Derived, never-persisted metadata postings for one collection.
///
/// Maintained only after successful commits (PAT-002) and rebuilt from live
/// records on open or declaration change.
#[derive(Debug, Default)]
pub(crate) struct MetadataIndexes {
    fields: BTreeMap<String, FieldIndex>,
    /// P5 block summaries over the declared fields: narrow exact filtered
    /// scans that postings cannot, never changing their results.
    summaries: crate::compact::stats::Summaries,
}

/// Result of a metadata-index lookup over a conjunction of conditions.
#[derive(Debug)]
pub(crate) struct IndexLookup {
    /// Upper bound on matching records: exact posting sizes for equality,
    /// and range sums that stop (saturating at `cap + 1`) once over the cap.
    /// `ids` is `None` exactly when the bound exceeds the cap.
    pub estimate: u64,
    /// Field of the most selective indexed condition.
    pub field: String,
    /// Candidate superset (all conditions must still be re-verified);
    /// materialized only when `estimate <= cap`.
    pub ids: Option<BTreeSet<u64>>,
}

type Postings = BTreeSet<u64>;

/// Finite f64 key ordered by value with `-0.0` normalized to `0.0`, so the
/// key order equals `partial_cmp` on the finite values metadata permits.
#[derive(Clone, Copy, Debug)]
struct OrdF64(f64);

impl OrdF64 {
    fn new(value: f64) -> Self {
        Self(if value == 0.0 { 0.0 } else { value })
    }
}

impl PartialEq for OrdF64 {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for OrdF64 {}

impl PartialOrd for OrdF64 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrdF64 {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Equality key with the same identity as `MetadataValue::eq` on valid
/// (finite) metadata: types are distinct and `-0.0 == 0.0`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum EqKey {
    String(String),
    Integer(i64),
    Float(u64),
    Boolean(bool),
    GeoPoint(u64, u64),
}

impl EqKey {
    fn of(value: &MetadataValue) -> Self {
        match value {
            MetadataValue::String(value) => Self::String(value.clone()),
            MetadataValue::Integer(value) => Self::Integer(*value),
            MetadataValue::Float(value) => Self::Float(OrdF64::new(*value).0.to_bits()),
            MetadataValue::Boolean(value) => Self::Boolean(*value),
            MetadataValue::GeoPoint {
                latitude,
                longitude,
            } => Self::GeoPoint(
                OrdF64::new(*latitude).0.to_bits(),
                OrdF64::new(*longitude).0.to_bits(),
            ),
        }
    }
}

#[derive(Debug, Default)]
struct Numeric {
    ints: BTreeMap<i64, Postings>,
    floats: BTreeMap<OrdF64, Postings>,
}

#[derive(Debug, Default)]
struct FieldIndex {
    eq: Option<HashMap<EqKey, Postings>>,
    range: Option<Numeric>,
}

fn add<K: Ord>(map: &mut BTreeMap<K, Postings>, key: K, id: u64) {
    map.entry(key).or_default().insert(id);
}

fn remove<K: Ord>(map: &mut BTreeMap<K, Postings>, key: &K, id: u64) {
    if let Some(ids) = map.get_mut(key) {
        ids.remove(&id);
        if ids.is_empty() {
            map.remove(key);
        }
    }
}

/// Visits posting sets in `[min, max]` until `visit` returns false.
fn visit_range<K: Ord + Copy>(
    map: &BTreeMap<K, Postings>,
    min: Option<K>,
    max: Option<K>,
    mut visit: impl FnMut(&Postings) -> bool,
) {
    use std::ops::Bound::{Included, Unbounded};
    if let (Some(min), Some(max)) = (min, max)
        && min > max
    {
        return;
    }
    let lower = min.map_or(Unbounded, Included);
    let upper = max.map_or(Unbounded, Included);
    for ids in map.range((lower, upper)).map(|(_, ids)| ids) {
        if !visit(ids) {
            return;
        }
    }
}

impl FieldIndex {
    fn insert(&mut self, id: u64, value: &MetadataValue) {
        if let Some(eq) = &mut self.eq {
            eq.entry(EqKey::of(value)).or_default().insert(id);
        }
        if let Some(range) = &mut self.range {
            match value {
                MetadataValue::Integer(value) => add(&mut range.ints, *value, id),
                MetadataValue::Float(value) => add(&mut range.floats, OrdF64::new(*value), id),
                _ => {}
            }
        }
    }

    fn remove(&mut self, id: u64, value: &MetadataValue) {
        if let Some(eq) = &mut self.eq {
            let key = EqKey::of(value);
            if let Some(ids) = eq.get_mut(&key) {
                ids.remove(&id);
                if ids.is_empty() {
                    eq.remove(&key);
                }
            }
        }
        if let Some(range) = &mut self.range {
            match value {
                MetadataValue::Integer(value) => remove(&mut range.ints, value, id),
                MetadataValue::Float(value) => remove(&mut range.floats, &OrdF64::new(*value), id),
                _ => {}
            }
        }
    }

    /// Posting set for an equality value, or `None` when not indexable here.
    fn eq_postings(&self, value: &MetadataValue) -> Option<Option<&Postings>> {
        if let Some(eq) = &self.eq {
            return Some(eq.get(&EqKey::of(value)));
        }
        let range = self.range.as_ref()?;
        match value {
            MetadataValue::Integer(value) => Some(range.ints.get(value)),
            MetadataValue::Float(value) => Some(range.floats.get(&OrdF64::new(*value))),
            _ => None,
        }
    }

    /// Visits range postings; returns false when ranges are not indexed here.
    fn visit(
        &self,
        min: &Option<MetadataValue>,
        max: &Option<MetadataValue>,
        visit: impl FnMut(&Postings) -> bool,
    ) -> bool {
        let Some(range) = &self.range else {
            return false;
        };
        let int = |value: &Option<MetadataValue>| match value {
            Some(MetadataValue::Integer(value)) => Some(*value),
            _ => None,
        };
        let float = |value: &Option<MetadataValue>| match value {
            Some(MetadataValue::Float(value)) => Some(OrdF64::new(*value)),
            _ => None,
        };
        // Validated ranges have numeric bounds of one type; an integer range
        // can only match integers and a float range only floats.
        let integer = matches!(min, Some(MetadataValue::Integer(_)))
            || matches!(max, Some(MetadataValue::Integer(_)));
        if integer {
            visit_range(&range.ints, int(min), int(max), visit);
        } else {
            visit_range(&range.floats, float(min), float(max), visit);
        }
        true
    }

    /// Upper bound on matches saturating at `cap + 1`; `None` = not indexed.
    fn count(&self, condition: &Condition, cap: usize) -> Option<usize> {
        match condition {
            Condition::GeoRadius { .. } => None,
            Condition::Eq { value, .. } => self
                .eq_postings(value)
                .map(|ids| ids.map_or(0, BTreeSet::len)),
            Condition::Range { min, max, .. } => {
                let mut total = 0usize;
                self.visit(min, max, |ids| {
                    total = total.saturating_add(ids.len());
                    total <= cap
                })
                .then_some(total.min(cap.saturating_add(1)))
            }
        }
    }

    fn ids(&self, condition: &Condition) -> Postings {
        match condition {
            Condition::GeoRadius { .. } => Postings::new(),
            Condition::Eq { value, .. } => self
                .eq_postings(value)
                .flatten()
                .cloned()
                .unwrap_or_default(),
            Condition::Range { min, max, .. } => {
                let mut all = Postings::new();
                self.visit(min, max, |ids| {
                    all.extend(ids);
                    true
                });
                all
            }
        }
    }
}

fn field_of(condition: &Condition) -> &str {
    match condition {
        Condition::Eq { field, .. }
        | Condition::Range { field, .. }
        | Condition::GeoRadius { field, .. } => field,
    }
}

impl MetadataIndexes {
    #[cfg(test)]
    pub(crate) fn rebuilt_effective(
        &self,
        specs: &[MetadataIndexSpec],
        records: &crate::records::CollectionRecords,
        workspace: &mut crate::compact::source::ReadWorkspace,
    ) -> crate::Result<Self> {
        let mut rebuilt = Self::default();
        rebuilt.rebuild(specs, &crate::records::RecordMap::default());
        if !rebuilt.fields.is_empty() {
            for id in records.effective_ids() {
                if let Some(metadata) = records.effective_metadata(id, workspace)? {
                    rebuilt.track(id, None, Some(&metadata));
                }
            }
        }
        Ok(rebuilt)
    }

    /// Rebuilds from all live records after open or a declaration change.
    pub(crate) fn rebuild(
        &mut self,
        specs: &[MetadataIndexSpec],
        records: &crate::records::RecordMap,
    ) {
        self.fields.clear();
        self.summaries =
            crate::compact::stats::Summaries::new(specs.iter().map(|spec| spec.field.clone()));
        for spec in specs {
            let index = self.fields.entry(spec.field.clone()).or_default();
            if matches!(
                spec.kind,
                MetadataIndexKind::Eq | MetadataIndexKind::EqRange
            ) {
                index.eq.get_or_insert_with(HashMap::new);
            }
            if matches!(
                spec.kind,
                MetadataIndexKind::Range | MetadataIndexKind::EqRange
            ) {
                index.range.get_or_insert_with(Numeric::default);
            }
        }
        if self.fields.is_empty() {
            return;
        }
        for (&id, record) in records {
            self.track(id, None, Some(&record.metadata));
        }
    }

    /// Applies one committed change (`current` None means delete).
    pub(crate) fn track(
        &mut self,
        id: u64,
        previous: Option<&Metadata>,
        current: Option<&Metadata>,
    ) {
        self.summaries.track(id, previous, current);
        for (field, index) in &mut self.fields {
            let old = previous.and_then(|metadata| metadata.get(field));
            let new = current.and_then(|metadata| metadata.get(field));
            if old.is_some() && old == new {
                continue;
            }
            if let Some(old) = old {
                index.remove(id, old);
            }
            if let Some(new) = new {
                index.insert(id, new);
            }
        }
    }

    /// Looks up the most selective indexed condition. Returns `None` when no
    /// condition can use an index. Candidate IDs are materialized (and
    /// intersected with every other indexed condition within `cap`) only when
    /// the best bound is `<= cap`.
    pub(crate) fn lookup(&self, conditions: &[Condition], cap: usize) -> Option<IndexLookup> {
        let counted: Vec<(usize, &FieldIndex, &Condition)> = conditions
            .iter()
            .filter_map(|condition| {
                let index = self.fields.get(field_of(condition))?;
                Some((index.count(condition, cap)?, index, condition))
            })
            .collect();
        let best = counted
            .iter()
            .enumerate()
            .min_by_key(|(_, (count, ..))| *count)?
            .0;
        let (count, index, condition) = counted[best];
        let field = field_of(condition).to_owned();
        if count > cap {
            return Some(IndexLookup {
                estimate: count as u64,
                field,
                ids: None,
            });
        }
        let mut ids = index.ids(condition);
        for (position, &(other_count, other, other_condition)) in counted.iter().enumerate() {
            if position == best || other_count > cap || ids.is_empty() {
                continue;
            }
            let other_ids = other.ids(other_condition);
            ids.retain(|id| other_ids.contains(id));
        }
        Some(IndexLookup {
            estimate: ids.len() as u64,
            field,
            ids: Some(ids),
        })
    }

    /// A re-verified candidate superset from block summaries of the declared
    /// fields, when they prune at least one block and leave at most `cap`
    /// IDs; `None` means scan every record.
    pub(crate) fn summary_candidates(
        &self,
        conditions: &[Condition],
        records: &crate::records::RecordMap,
        cap: usize,
    ) -> Option<BTreeSet<u64>> {
        self.summaries.candidates(conditions, records, cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_ann_paths_remain_refused_with_unchanged_state() {
        let _precision = crate::compact::Activation::on();
        let _records = crate::compact::catalog::payload::Activation::on();
        let _source = crate::compact::source::Activation::on();
        let mut original = crate::Database::create_in_memory(2, Default::default()).unwrap();
        original.put(1, &[1.0, 2.0], b"p").unwrap();
        let mut loaded = crate::format::load_with(
            &mut std::io::Cursor::new(original.to_bytes().unwrap()),
            None,
        )
        .unwrap();
        loaded.flags |= crate::format::FLAG_PRECISION;
        loaded.collections.get_mut(&0).unwrap().precision =
            Some(crate::compact::catalog::Precision::Float32);
        let mut bytes = Vec::new();
        crate::maintenance::write_v5_image(
            &mut bytes,
            &loaded,
            loaded.lineage.unwrap(),
            None,
            None,
            None,
            loaded.flags,
            loaded.sequence,
        )
        .unwrap();
        let mut db = crate::Database::open_from_bytes(bytes.clone(), None).unwrap();
        let oracle = {
            let _off = crate::compact::source::Activation::set(false);
            crate::Database::open_from_bytes(bytes, None).unwrap()
        };
        let sequence = db.sequence;
        let pin = db.default_state().records.pin_effective();
        let config = crate::HnswConfig::default();
        let metric = crate::DistanceMetric::SquaredL2;
        let limits = crate::HnswRebuildLimits::default();
        let options = crate::AnnOptions::default();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("refused.ann");
        macro_rules! refused {
            ($expression:expr) => {{
                assert!(
                    matches!(
                        $expression,
                        Err(crate::Error::UnsupportedFeature(
                            crate::format::FLAG_COMPACT
                        ))
                    ),
                    stringify!($expression)
                );
                assert_eq!(db.sequence, sequence);
                assert_eq!(
                    db.default_state().records.source_identity(),
                    pin.source_identity()
                );
                assert_eq!(
                    db.default_state().records.effective_ids(),
                    pin.effective_ids()
                );
                assert!(db.default_state().index.is_none());
                assert!(db.default_state().background_hnsw.is_none());
                assert!(db.rebuild.is_none());
            }};
        }
        refused!(db.build_hnsw(config));
        refused!(db.build_hnsw_with_options(config, metric, options));
        refused!(db.set_background_hnsw_rebuild(Some(limits)));
        refused!(db.start_hnsw_rebuild(config, metric, limits));
        refused!(db.poll_hnsw_rebuild());
        refused!(db.search_approximate(&[1.0, 2.0], 1, 1, config));
        refused!(db.search_approximate_ready_with_options(
            &[1.0, 2.0],
            1,
            1,
            config,
            options,
            &[],
            metric
        ));
        refused!(db.save_ann_snapshot(&path));
        refused!(db.load_ann_snapshot(&path));
        refused!(db.build_disk_ann("default", &path, config, metric));
        assert!(!path.exists());
        let sidecar = directory.path().join("oracle.disk");
        oracle
            .build_disk_ann("default", &sidecar, config, metric)
            .unwrap();
        let disk = crate::disk_ann::DiskAnnIndex::open(&sidecar, Default::default()).unwrap();
        refused!(db.search_disk_ann(&disk, &[1.0, 2.0], 1, 1));
        #[cfg(feature = "gpu")]
        {
            refused!(crate::gpu::resident_preflight(&db));
            // Structural wiring covers the early gate even on adapter-free CI.
            let source: String = include_str!("gpu.rs")
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            let search = source.split("pubfnsearch(").nth(1).unwrap();
            let start = search.find('{').unwrap();
            let mut depth = 0usize;
            let end = search[start..]
                .char_indices()
                .find_map(|(offset, c)| {
                    match c {
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        _ => {}
                    }
                    (depth == 0).then_some(start + offset)
                })
                .unwrap();
            let body = &search[start..end];
            assert!(body.contains("resident_preflight("));
            assert!(
                body.find("resident_preflight(").unwrap() < body.find("database.state(").unwrap()
            );
            if let Some(gpu) = crate::gpu::GpuExactSearcher::new() {
                refused!(gpu.search(&db, &[1.0, 2.0], 1));
            }
        }
        refused!(db.checkpoint());
    }

    #[test]
    fn compact_settings_validate_and_persist_named_collection() {
        let _precision = crate::compact::Activation::on();
        let _records = crate::compact::catalog::payload::Activation::on();
        let _source = crate::compact::source::Activation::on();
        for encrypted in [false, true] {
            let key = encrypted.then(|| crate::EncryptionKey::from_bytes([9; 32]));
            let mut original = crate::Database::create_in_memory(
                2,
                crate::CreateOptions {
                    encryption: key.clone(),
                    ..Default::default()
                },
            )
            .unwrap();
            original
                .create_collection(
                    "indexed",
                    crate::CollectionSpec {
                        dimensions: 2,
                        settings: Default::default(),
                    },
                )
                .unwrap();
            let old = crate::Metadata::from([("tag".into(), MetadataValue::String("old".into()))]);
            original
                .collection_mut("indexed")
                .unwrap()
                .put_with_metadata(1, &[1.0, 2.0], b"a", &old)
                .unwrap();
            let mut loaded = crate::format::load_with(
                &mut std::io::Cursor::new(original.to_bytes().unwrap()),
                key.as_ref(),
            )
            .unwrap();
            loaded.flags |= crate::format::FLAG_PRECISION;
            let id = *loaded.collections.keys().find(|&&id| id != 0).unwrap();
            loaded.collections.get_mut(&0).unwrap().precision =
                Some(crate::compact::catalog::Precision::Float32);
            loaded.collections.get_mut(&id).unwrap().precision =
                Some(crate::compact::catalog::Precision::Float16);
            let mut image = Vec::new();
            crate::maintenance::write_v5_image(
                &mut image,
                &loaded,
                loaded.lineage.unwrap(),
                None,
                None,
                loaded.cipher.as_ref(),
                loaded.flags,
                loaded.sequence,
            )
            .unwrap();
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("compact-settings.vlite");
            std::fs::write(&path, image).unwrap();
            let mut db = match key.as_ref() {
                Some(key) => crate::Database::open_encrypted(&path, key).unwrap(),
                None => crate::Database::open(&path).unwrap(),
            };
            assert!(db.state(id).records.is_compact());
            let before = db.sequence;
            let invalid = crate::CollectionSettings {
                rerank_multiplier: 0,
                ..Default::default()
            };
            assert!(matches!(
                db.set_collection_settings("indexed", invalid),
                Err(crate::Error::InvalidCollectionSettings(_))
            ));
            assert_eq!(db.sequence, before);
            let settings = crate::CollectionSettings {
                metadata_indexes: vec![spec("tag", MetadataIndexKind::Eq)],
                text_fields: vec!["tag".into()],
                ..Default::default()
            };
            db.set_collection_settings("indexed", settings.clone())
                .unwrap();
            let filter = [Condition::Eq {
                field: "tag".into(),
                value: MetadataValue::String("old".into()),
            }];
            assert_eq!(
                db.state(id)
                    .metadata_indexes
                    .lookup(&filter, 10)
                    .unwrap()
                    .ids,
                Some([1].into())
            );
            drop(db);
            let reopened = match key.as_ref() {
                Some(key) => crate::Database::open_encrypted(&path, key).unwrap(),
                None => crate::Database::open(&path).unwrap(),
            };
            assert_eq!(reopened.state(id).settings, settings);
            assert_eq!(
                reopened.default_state().settings,
                crate::CollectionSettings::default()
            );
        }
    }

    #[test]
    fn compact_metadata_text_indexes_verify_overlay() {
        let _precision = crate::compact::Activation::on();
        let _records = crate::compact::catalog::payload::Activation::on();
        let _source = crate::compact::source::Activation::on();
        for encrypted in [false, true] {
            let key = encrypted.then(|| crate::EncryptionKey::from_bytes([9; 32]));
            let mut original = crate::Database::create_in_memory(
                2,
                crate::CreateOptions {
                    encryption: key.clone(),
                    ..crate::CreateOptions::default()
                },
            )
            .unwrap();
            let old = crate::Metadata::from([
                ("tag".into(), MetadataValue::String("old".into())),
                ("body".into(), MetadataValue::String("old text".into())),
            ]);
            original
                .put_with_metadata(10, &[0.25, -0.0], b"a", &old)
                .unwrap();
            original
                .put_with_metadata(20, &[0.5, 1.0], b"b", &old)
                .unwrap();
            original
                .put_with_metadata(40, &[1.0, 0.5], b"d", &old)
                .unwrap();
            original
                .put_with_metadata(50, &[1.0, 2.0], b"e", &old)
                .unwrap();
            original
                .put_with_expiry(
                    60,
                    &[3.0, 4.0],
                    b"future",
                    &crate::Metadata::new(),
                    Some(u64::MAX),
                )
                .unwrap();
            let mut loaded = crate::format::load_with(
                &mut std::io::Cursor::new(original.to_bytes().unwrap()),
                key.as_ref(),
            )
            .unwrap();
            loaded.flags |= crate::format::FLAG_PRECISION;
            loaded
                .collections
                .get_mut(&0)
                .unwrap()
                .expiries
                .insert(40, 100);
            loaded.collections.get_mut(&0).unwrap().precision =
                Some(crate::compact::catalog::Precision::Float16);
            let mut image = Vec::new();
            crate::maintenance::write_v5_image(
                &mut image,
                &loaded,
                loaded.lineage.unwrap(),
                None,
                None,
                loaded.cipher.as_ref(),
                loaded.flags,
                loaded.sequence,
            )
            .unwrap();
            let mut db = crate::Database::open_from_bytes(image.clone(), key.as_ref()).unwrap();
            let image_bytes = image.len();
            let mut oracle = {
                let _off = crate::compact::source::Activation::set(false);
                crate::Database::open_from_bytes(image, key.as_ref()).unwrap()
            };
            let current = crate::Metadata::from([
                ("tag".into(), MetadataValue::String("new".into())),
                ("body".into(), MetadataValue::String("new text".into())),
            ]);
            for db in [&mut db, &mut oracle] {
                db.delete(10).unwrap();
                db.update_metadata(20, &current).unwrap();
                db.put_with_metadata(30, &[2.0, 3.0], b"c", &current)
                    .unwrap();
            }
            let pin = db.default_state().records.pin_effective();
            let mut workspace = crate::compact::source::ReadWorkspace::new(4 << 20).unwrap();
            let initial_accounting = pin.source_accounting(&mut workspace).unwrap().unwrap();
            let cells = initial_accounting.cells_live;
            let source_bytes = initial_accounting.source_live;
            assert!(source_bytes > 0 && source_bytes <= image_bytes);
            let indexes = MetadataIndexes::default()
                .rebuilt_effective(&[spec("tag", MetadataIndexKind::Eq)], &pin, &mut workspace)
                .unwrap();
            let text = crate::text_index::TextIndex::default()
                .rebuilt_effective(&["body".into()], &pin, &mut workspace)
                .unwrap();
            let filter = [Condition::Eq {
                field: "tag".into(),
                value: MetadataValue::String("new".into()),
            }];
            assert_eq!(
                indexes.lookup(&filter, 10).unwrap().ids.unwrap(),
                [20, 30].into()
            );
            let old_filter = [Condition::Eq {
                field: "tag".into(),
                value: MetadataValue::String("old".into()),
            }];
            assert_eq!(
                indexes.lookup(&old_filter, 10).unwrap().ids.unwrap(),
                [40, 50].into()
            );
            let terms = crate::text_index::tokenize("new").into_iter().collect();
            assert_eq!(
                text.search(&terms, 10, |_| true)
                    .iter()
                    .map(|(id, _)| *id)
                    .collect::<Vec<_>>(),
                vec![20, 30]
            );
            let old_terms = crate::text_index::tokenize("old").into_iter().collect();
            assert_eq!(
                text.search(&old_terms, 10, |_| true)
                    .iter()
                    .map(|(id, _)| *id)
                    .collect::<Vec<_>>(),
                vec![40, 50]
            );
            assert_eq!(
                pin.effective_ids(),
                oracle.default_state().records.effective_ids()
            );
            let mut expected = MetadataIndexes::default();
            expected.rebuild(
                &[spec("tag", MetadataIndexKind::Eq)],
                oracle.default_state().records.resident().unwrap(),
            );
            assert_eq!(
                indexes.lookup(&filter, 10).unwrap().ids,
                expected.lookup(&filter, 10).unwrap().ids
            );
            assert_eq!(
                pin.source_accounting(&mut workspace)
                    .unwrap()
                    .unwrap()
                    .cells_live,
                cells
            );
            let mut settings = db.default_state().settings.clone();
            settings.metadata_indexes = vec![spec("tag", MetadataIndexKind::Eq)];
            settings.text_fields = vec!["body".into()];
            db.set_collection_settings("default", settings.clone())
                .unwrap();
            oracle.set_collection_settings("default", settings).unwrap();
            assert_eq!(
                db.default_state()
                    .metadata_indexes
                    .lookup(&filter, 10)
                    .unwrap()
                    .ids,
                Some([20, 30].into())
            );
            assert_eq!(
                db.default_state()
                    .text_index
                    .search(&terms, 10, |_| true)
                    .len(),
                2
            );
            db.state_mut(0).metadata_indexes = MetadataIndexes::default();
            db.state_mut(0).text_index = crate::text_index::TextIndex::default();
            db.state_mut(0).rebuild_derived().unwrap();
            let state = db.default_state();
            assert_eq!(
                state.metadata_indexes.lookup(&filter, 10).unwrap().ids,
                Some([20, 30].into())
            );
            assert_eq!(state.text_index.search(&terms, 10, |_| true).len(), 2);
            let hits = [20, 30, 40, 10, 50, 60].map(|id| crate::SearchHit {
                id,
                distance: id as f64 / 10.0,
            });
            let reranker = crate::rerank::ExactReranker {
                metric: crate::DistanceMetric::SquaredL2,
            };
            let expected = crate::CollectionRef { db: &oracle, id: 0 }
                .rerank(&[0.0, 0.0], &hits, 10, &reranker)
                .unwrap();
            struct Verify(std::collections::BTreeMap<u64, crate::Record>);
            impl crate::rerank::Reranker for Verify {
                fn rerank(
                    &self,
                    query: &[f32],
                    candidates: &[crate::rerank::RerankCandidate],
                    limit: usize,
                ) -> crate::Result<Vec<crate::SearchHit>> {
                    assert_eq!(candidates.len(), self.0.len());
                    for candidate in candidates {
                        let expected = &self.0[&candidate.id];
                        assert_eq!(candidate.payload, expected.payload);
                        assert_eq!(
                            candidate
                                .vector
                                .iter()
                                .map(|value| value.to_bits())
                                .collect::<Vec<_>>(),
                            expected
                                .vector
                                .iter()
                                .map(|value| value.to_bits())
                                .collect::<Vec<_>>()
                        );
                        assert_eq!(candidate.distance, candidate.id as f64 / 10.0);
                    }
                    crate::rerank::Reranker::rerank(
                        &crate::rerank::ExactReranker {
                            metric: crate::DistanceMetric::SquaredL2,
                        },
                        query,
                        candidates,
                        limit,
                    )
                }
            }
            let verifier = Verify(
                hits.iter()
                    .filter_map(|hit| oracle.get(hit.id).unwrap().map(|row| (hit.id, row.clone())))
                    .collect(),
            );
            let actual = crate::CollectionRef { db: &db, id: 0 }
                .rerank(&[0.0, 0.0], &hits, 10, &verifier)
                .unwrap();
            assert_eq!(actual, expected);
            struct Count(usize);
            impl crate::rerank::Reranker for Count {
                fn rerank(
                    &self,
                    _: &[f32],
                    candidates: &[crate::rerank::RerankCandidate],
                    _: usize,
                ) -> crate::Result<Vec<crate::SearchHit>> {
                    assert_eq!(candidates.len(), self.0);
                    Ok(Vec::new())
                }
            }
            let boundary = vec![
                crate::SearchHit {
                    id: 50,
                    distance: 5.0
                };
                crate::MAX_BATCH_OPERATIONS
            ];
            let oversized = vec![boundary[0].clone(); crate::MAX_BATCH_OPERATIONS + 1];
            crate::CollectionRef { db: &db, id: 0 }
                .rerank(
                    &[0.0, 0.0],
                    &boundary,
                    1,
                    &Count(crate::MAX_BATCH_OPERATIONS),
                )
                .unwrap();
            db.poisoned = true;
            {
                let view = crate::CollectionRef { db: &db, id: 0 };
                assert!(matches!(
                    view.rerank(&[], &[], 0, &Count(0)),
                    Err(crate::Error::Poisoned)
                ));
                assert!(matches!(
                    view.rerank(&[0.0, 0.0], &oversized, 1, &Count(0)),
                    Err(crate::Error::Poisoned)
                ));
            }
            db.poisoned = false;
            let view = crate::CollectionRef { db: &db, id: 0 };
            assert!(matches!(
                view.rerank(&[0.0, 0.0], &oversized, 1, &Count(0)),
                Err(crate::Error::BatchTooLarge)
            ));
            let memory = crate::quality::effective_memory(&pin, &mut workspace)
                .unwrap()
                .unwrap();
            assert_eq!(memory.source.source_live, source_bytes);
            assert_eq!(memory.source.mapped_live, 0);
            assert_eq!(memory.source.cells_live, cells);
            assert!(memory.overlay_bytes > 0);
            assert_eq!(memory.source.vectors_live, 0);
            db.delete(50).unwrap();
            drop(db);
            let pinned = crate::rerank::rerank_effective(
                &pin,
                &Default::default(),
                crate::current_time_ms(),
                &[0.0, 0.0],
                &hits,
                10,
                &verifier,
            )
            .unwrap();
            assert_eq!(pinned, expected);
        }
    }

    fn record(fields: &[(&str, MetadataValue)]) -> Record {
        Record {
            vector: vec![0.0],
            payload: vec![],
            metadata: fields
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
        }
    }

    fn spec(field: &str, kind: MetadataIndexKind) -> MetadataIndexSpec {
        MetadataIndexSpec {
            field: field.into(),
            kind,
        }
    }

    #[test]
    fn typed_equality_ranges_and_negative_zero_follow_filter_semantics() {
        use MetadataValue::{Float, Integer, String as Text};
        let records = crate::records::RecordMap::from([
            (1, record(&[("n", Integer(1))])),
            (2, record(&[("n", Float(1.0))])),
            (3, record(&[("n", Float(-0.0))])),
            (4, record(&[("n", Float(0.0))])),
            (5, record(&[("n", Integer(i64::MIN))])),
            (6, record(&[("n", Integer(i64::MAX))])),
            (7, record(&[("n", Text("1".into()))])),
        ]);
        for kind in [
            MetadataIndexKind::Eq,
            MetadataIndexKind::Range,
            MetadataIndexKind::EqRange,
        ] {
            let mut indexes = MetadataIndexes::default();
            indexes.rebuild(&[spec("n", kind)], &records);
            let ids = |condition: Condition| {
                indexes
                    .lookup(&[condition], usize::MAX - 1)
                    .and_then(|lookup| lookup.ids)
                    .map(|ids| ids.into_iter().collect::<Vec<_>>())
            };
            let eq = |value| Condition::Eq {
                field: "n".into(),
                value,
            };
            assert_eq!(ids(eq(Integer(1))), Some(vec![1]));
            assert_eq!(ids(eq(Float(1.0))), Some(vec![2]));
            assert_eq!(ids(eq(Float(0.0))), Some(vec![3, 4]));
            assert_eq!(ids(eq(Float(-0.0))), Some(vec![3, 4]));
            let text = ids(eq(Text("1".into())));
            let range = |min, max| Condition::Range {
                field: "n".into(),
                min,
                max,
            };
            let ints = ids(range(Some(Integer(i64::MIN)), Some(Integer(i64::MAX))));
            let floats = ids(range(Some(Float(-0.0)), None));
            if kind == MetadataIndexKind::Eq {
                assert_eq!(text, Some(vec![7]));
                assert_eq!(ints, None);
                assert_eq!(floats, None);
            } else {
                assert_eq!(
                    text,
                    if kind == MetadataIndexKind::Range {
                        None
                    } else {
                        Some(vec![7])
                    }
                );
                assert_eq!(ints, Some(vec![1, 5, 6]));
                assert_eq!(floats, Some(vec![2, 3, 4]));
            }
        }
    }

    #[test]
    fn tracking_updates_and_lookup_caps_and_intersects() {
        let mut indexes = MetadataIndexes::default();
        indexes.rebuild(
            &[
                spec("a", MetadataIndexKind::Eq),
                spec("b", MetadataIndexKind::Range),
            ],
            &crate::records::RecordMap::new(),
        );
        for id in 0..10 {
            let metadata = record(&[
                ("a", MetadataValue::Integer(id as i64 % 2)),
                ("b", MetadataValue::Integer(id as i64)),
            ])
            .metadata;
            indexes.track(id, None, Some(&metadata));
        }
        let a0 = Condition::Eq {
            field: "a".into(),
            value: MetadataValue::Integer(0),
        };
        let b = Condition::Range {
            field: "b".into(),
            min: Some(MetadataValue::Integer(2)),
            max: Some(MetadataValue::Integer(6)),
        };
        let lookup = indexes.lookup(&[a0.clone(), b.clone()], 100).unwrap();
        assert_eq!(
            lookup.ids.unwrap().into_iter().collect::<Vec<_>>(),
            vec![2, 4, 6]
        );
        let capped = indexes.lookup(&[a0.clone(), b], 3).unwrap();
        assert_eq!((capped.estimate, capped.ids), (4, None));
        let old = record(&[("a", MetadataValue::Integer(0))]).metadata;
        indexes.track(0, Some(&old), None);
        let new = record(&[("a", MetadataValue::Integer(1))]).metadata;
        indexes.track(2, Some(&old), Some(&new));
        let lookup = indexes.lookup(&[a0], 100).unwrap();
        assert_eq!(
            lookup.ids.unwrap().into_iter().collect::<Vec<_>>(),
            vec![4, 6, 8]
        );
        assert!(
            indexes
                .lookup(
                    &[Condition::Eq {
                        field: "z".into(),
                        value: MetadataValue::Boolean(true)
                    }],
                    100
                )
                .is_none()
        );
    }
}
