use crate::{
    CollectionMut, CollectionRef, Condition, Database, DistanceMetric, Error, MetadataValue,
    Record, Result, SearchHit, current_time_ms, metadata, validate_vector,
};
use std::collections::HashMap;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

/// A generation shares immutable records; a writer copies only the ID tree
/// and the modified record, never the unmodified vectors or payloads.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RecordMap(Arc<BTreeMap<u64, Arc<Record>>>);

type RecordIter<'a> = std::iter::Map<
    std::collections::btree_map::Iter<'a, u64, Arc<Record>>,
    fn((&'a u64, &'a Arc<Record>)) -> (&'a u64, &'a Record),
>;

impl RecordMap {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn contains_key(&self, id: &u64) -> bool {
        self.0.contains_key(id)
    }
    pub fn get(&self, id: &u64) -> Option<&Record> {
        self.0.get(id).map(Arc::as_ref)
    }
    pub fn get_key_value(&self, id: &u64) -> Option<(&u64, &Record)> {
        self.0
            .get_key_value(id)
            .map(|(id, record)| (id, record.as_ref()))
    }
    pub fn get_mut(&mut self, id: &u64) -> Option<&mut Record> {
        Arc::make_mut(&mut self.0).get_mut(id).map(Arc::make_mut)
    }
    pub fn insert(&mut self, id: u64, record: Record) -> Option<Arc<Record>> {
        Arc::make_mut(&mut self.0).insert(id, Arc::new(record))
    }
    pub fn remove(&mut self, id: &u64) -> Option<Arc<Record>> {
        Arc::make_mut(&mut self.0).remove(id)
    }
    pub fn iter(&self) -> RecordIter<'_> {
        self.0.iter().map(|(id, record)| (id, record.as_ref()))
    }
    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &u64> + ExactSizeIterator {
        self.0.keys()
    }
    pub fn values(&self) -> impl DoubleEndedIterator<Item = &Record> + ExactSizeIterator {
        self.0.values().map(Arc::as_ref)
    }
    pub fn range(
        &self,
        range: impl std::ops::RangeBounds<u64>,
    ) -> impl DoubleEndedIterator<Item = (&u64, &Record)> {
        self.0
            .range(range)
            .map(|(id, record)| (id, record.as_ref()))
    }
}

impl<'a> IntoIterator for &'a RecordMap {
    type Item = (&'a u64, &'a Record);
    type IntoIter = RecordIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl std::ops::Index<&u64> for RecordMap {
    type Output = Record;
    fn index(&self, id: &u64) -> &Record {
        &self.0[id]
    }
}

impl FromIterator<(u64, Record)> for RecordMap {
    fn from_iter<T: IntoIterator<Item = (u64, Record)>>(iter: T) -> Self {
        Self(Arc::new(
            iter.into_iter()
                .map(|(id, record)| (id, Arc::new(record)))
                .collect(),
        ))
    }
}

impl<const N: usize> From<[(u64, Record); N]> for RecordMap {
    fn from(records: [(u64, Record); N]) -> Self {
        records.into_iter().collect()
    }
}

/// Maximum distinct live read captures per database. Clones share one capture.
pub const MAX_READ_SNAPSHOTS: usize = 8;
/// Bounds retained logical record bodies per capture (not total allocator RSS).
pub const MAX_READ_SNAPSHOT_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_READ_SNAPSHOT_RECORDS: usize = 100_000;

/// Current read-snapshot retention counters for one database.
///
/// `retained_logical_bytes` sums the logical record-body size at capture time
/// for every live capture. It is a retention-budget measure, not an estimate
/// of allocator usage or process RSS. Counters are sampled independently, so a
/// concurrent capture or final drop may briefly make them reflect adjacent
/// states.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SnapshotRetentionStats {
    pub active_captures: usize,
    pub retained_logical_bytes: u64,
}

struct SnapshotLease {
    readers: Arc<std::sync::atomic::AtomicUsize>,
    retained_logical_bytes_counter: Arc<AtomicU64>,
    retained_logical_bytes: u64,
}

impl Drop for SnapshotLease {
    fn drop(&mut self) {
        self.readers.fetch_sub(1, Ordering::Relaxed);
        self.retained_logical_bytes_counter
            .fetch_sub(self.retained_logical_bytes, Ordering::Relaxed);
    }
}

/// An immutable, committed collection generation. It owns no file handle or
/// database lock and can outlive its database. Capturing is O(1); the first
/// subsequent write copies the ID tree, sharing unchanged record bodies.
///
/// Exact scans do not use derived indexes. Default TTL visibility is frozen
/// at capture time; `_at` methods allow an explicit clock. Drop captures
/// promptly: at most eight captures, each <=100k records/128 MiB of logical
/// record bodies, may be retained per database. Clones share the same budget.
#[derive(Clone)]
pub struct ReadSnapshot {
    records: RecordMap,
    expiries: Arc<BTreeMap<u64, u64>>,
    dimensions: usize,
    sequence: u64,
    lineage: Option<u64>,
    captured_at_ms: u64,
    _lease: Arc<SnapshotLease>,
}

impl Database {
    /// Captures the default collection at one committed version.
    pub fn read_snapshot(&self) -> Result<ReadSnapshot> {
        self.collection("default")?.read_snapshot()
    }

    /// Returns per-database snapshot retention counters.
    pub fn snapshot_retention_stats(&self) -> SnapshotRetentionStats {
        SnapshotRetentionStats {
            active_captures: self.snapshot_readers.load(Ordering::Relaxed),
            retained_logical_bytes: self.snapshot_retained_bytes.load(Ordering::Relaxed),
        }
    }
}

impl CollectionRef<'_> {
    /// Captures this collection without retaining the database owner lock.
    /// Returns `InvalidQuery` when the retention budget is exhausted.
    pub fn read_snapshot(&self) -> Result<ReadSnapshot> {
        use std::sync::atomic::Ordering;
        self.db.ensure_healthy()?;
        if self.db.group_active {
            return Err(Error::InvalidQuery(
                "cannot capture an uncommitted durability group",
            ));
        }
        let state = self.db.state(self.id);
        if state.records.len() > MAX_READ_SNAPSHOT_RECORDS
            || state.live_body_bytes > MAX_READ_SNAPSHOT_BYTES
        {
            return Err(Error::InvalidQuery(
                "collection exceeds read snapshot retention limits",
            ));
        }
        #[allow(deprecated)] // Keep the Rust 1.89 minimum supported version.
        self.db
            .snapshot_readers
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                (count < MAX_READ_SNAPSHOTS).then_some(count + 1)
            })
            .map_err(|_| Error::InvalidQuery("read snapshot retention budget exhausted"))?;
        let retained_logical_bytes = state.live_body_bytes;
        self.db
            .snapshot_retained_bytes
            .fetch_add(retained_logical_bytes, Ordering::Relaxed);
        let lease = Arc::new(SnapshotLease {
            readers: Arc::clone(&self.db.snapshot_readers),
            retained_logical_bytes_counter: Arc::clone(&self.db.snapshot_retained_bytes),
            retained_logical_bytes,
        });
        Ok(ReadSnapshot {
            records: state.records.clone(),
            expiries: Arc::clone(&state.expiries),
            dimensions: state.dimensions,
            sequence: self.db.sequence,
            lineage: self.db.lineage,
            captured_at_ms: current_time_ms(),
            _lease: lease,
        })
    }
}

impl ReadSnapshot {
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn lineage(&self) -> Option<u64> {
        self.lineage
    }
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }
    /// Physical records at capture time, including records hidden by TTL.
    pub fn len(&self) -> usize {
        self.records.len()
    }
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
    pub fn get(&self, id: u64) -> Result<Option<&Record>> {
        self.get_at(id, self.captured_at_ms)
    }
    pub fn get_at(&self, id: u64, now_ms: u64) -> Result<Option<&Record>> {
        Ok(self.records.get(&id).filter(|_| {
            !self
                .expiries
                .get(&id)
                .is_some_and(|expiry| *expiry <= now_ms)
        }))
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    fn database() -> Database {
        Database::create_in_memory(1, crate::CreateOptions::default()).unwrap()
    }

    #[test]
    fn generations_share_unchanged_record_bodies_and_release_obsolete_bodies() {
        let mut db = database();
        db.put(1, &[1.0], b"first").unwrap();
        db.put(2, &[2.0], b"second").unwrap();
        let snapshot = db.read_snapshot().unwrap();
        let old = Arc::downgrade(&snapshot.records.0[&1]);
        let unchanged = snapshot.get(2).unwrap().unwrap().vector.as_ptr();
        db.put(1, &[3.0], b"replacement").unwrap();
        assert!(Arc::ptr_eq(
            &snapshot.records.0[&2],
            &db.default_state().records.0[&2]
        ));
        assert_eq!(unchanged, db.get(2).unwrap().unwrap().vector.as_ptr());
        assert_eq!(old.upgrade().unwrap().payload, b"first");
        drop(snapshot);
        assert!(old.upgrade().is_none());
    }

    #[test]
    fn snapshot_byte_budget_and_poisoned_owner_reject_new_captures() {
        let mut db = database();
        db.state_mut(0).live_body_bytes = MAX_READ_SNAPSHOT_BYTES + 1;
        assert!(db.read_snapshot().is_err());
        assert_eq!(
            db.snapshot_retention_stats(),
            SnapshotRetentionStats::default()
        );
        db.state_mut(0).live_body_bytes = 0;
        let snapshot = db.read_snapshot().unwrap();
        assert_eq!(
            db.snapshot_retention_stats(),
            SnapshotRetentionStats {
                active_captures: 1,
                retained_logical_bytes: 0,
            }
        );
        db.poisoned = true;
        assert!(matches!(db.read_snapshot(), Err(Error::Poisoned)));
        assert!(snapshot.search(&[0.0], 1).unwrap().is_empty());
    }

    #[test]
    fn retention_counters_count_distinct_captures_until_the_last_clone_drops() {
        let mut db = Database::create_in_memory(
            1,
            crate::CreateOptions {
                format: Some(crate::FormatVersion::V5),
                ..Default::default()
            },
        )
        .unwrap();
        db.put_with_expiry(1, &[1.0], b"payload", &crate::Metadata::new(), Some(1))
            .unwrap();
        let captured_bytes = db.default_state().live_body_bytes;
        let first = db.read_snapshot().unwrap();
        let first_clone = first.clone();
        assert!(
            first.get(1).unwrap().is_none(),
            "TTL visibility does not remove the physical record from retention accounting"
        );
        assert_eq!(
            db.snapshot_retention_stats(),
            SnapshotRetentionStats {
                active_captures: 1,
                retained_logical_bytes: captured_bytes,
            }
        );

        let second = db.read_snapshot().unwrap();
        assert_eq!(
            db.snapshot_retention_stats(),
            SnapshotRetentionStats {
                active_captures: 2,
                retained_logical_bytes: captured_bytes * 2,
            }
        );
        db.put(1, &[2.0], b"replacement with a different length")
            .unwrap();
        assert_eq!(
            db.snapshot_retention_stats().retained_logical_bytes,
            captured_bytes * 2,
            "accounting follows captured generations, not the live state"
        );

        drop(first);
        assert_eq!(db.snapshot_retention_stats().active_captures, 2);
        drop(first_clone);
        assert_eq!(
            db.snapshot_retention_stats(),
            SnapshotRetentionStats {
                active_captures: 1,
                retained_logical_bytes: captured_bytes,
            }
        );
        drop(second);
        assert_eq!(
            db.snapshot_retention_stats(),
            SnapshotRetentionStats::default()
        );
    }

    #[test]
    fn record_count_limit_accepts_boundary_and_rejects_larger_captures() {
        let mut db = database();
        db.state_mut(0).records = (0..MAX_READ_SNAPSHOT_RECORDS as u64)
            .map(|id| {
                (
                    id,
                    Record {
                        vector: Vec::new(),
                        payload: Vec::new(),
                        metadata: crate::Metadata::new(),
                    },
                )
            })
            .collect();
        let boundary = db.read_snapshot().unwrap();
        assert_eq!(boundary.len(), MAX_READ_SNAPSHOT_RECORDS);
        drop(boundary);
        db.state_mut(0).records.insert(
            MAX_READ_SNAPSHOT_RECORDS as u64,
            Record {
                vector: Vec::new(),
                payload: Vec::new(),
                metadata: crate::Metadata::new(),
            },
        );
        assert!(db.read_snapshot().is_err());
        assert_eq!(
            db.snapshot_retention_stats(),
            SnapshotRetentionStats::default()
        );
    }

    #[test]
    fn named_collection_drop_and_recreation_keep_each_capture_accounted() {
        let mut db = Database::create_in_memory(
            1,
            crate::CreateOptions {
                format: Some(crate::FormatVersion::V5),
                ..Default::default()
            },
        )
        .unwrap();
        let spec = crate::CollectionSpec {
            dimensions: 1,
            settings: Default::default(),
        };
        db.create_collection("named", spec.clone()).unwrap();
        db.collection_mut("named")
            .unwrap()
            .put(1, &[1.0], b"old")
            .unwrap();
        let old_bytes = db
            .state(db.collection("named").unwrap().id())
            .live_body_bytes;
        let old = db.collection("named").unwrap().read_snapshot().unwrap();
        db.collection_mut("named").unwrap().delete(1).unwrap();
        db.drop_collection("named").unwrap();
        db.create_collection("named", spec).unwrap();
        db.collection_mut("named")
            .unwrap()
            .put(2, &[2.0], b"new generation")
            .unwrap();
        let new_bytes = db
            .state(db.collection("named").unwrap().id())
            .live_body_bytes;
        let new = db.collection("named").unwrap().read_snapshot().unwrap();

        assert_eq!(
            db.snapshot_retention_stats(),
            SnapshotRetentionStats {
                active_captures: 2,
                retained_logical_bytes: old_bytes + new_bytes,
            }
        );
        drop(old);
        assert_eq!(
            db.snapshot_retention_stats(),
            SnapshotRetentionStats {
                active_captures: 1,
                retained_logical_bytes: new_bytes,
            }
        );
        drop(new);
        assert_eq!(
            db.snapshot_retention_stats(),
            SnapshotRetentionStats::default()
        );
    }

    #[test]
    fn concurrent_snapshot_reclamation_never_exceeds_the_captured_budget() {
        let mut db = database();
        db.put(1, &[1.0], b"payload").unwrap();
        let bytes_per_capture = db.default_state().live_body_bytes;
        let captures = (0..MAX_READ_SNAPSHOTS)
            .map(|_| db.read_snapshot().unwrap())
            .collect::<Vec<_>>();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let drop_barrier = Arc::clone(&barrier);
        let drop_done = Arc::clone(&done);
        let dropper = std::thread::spawn(move || {
            drop_barrier.wait();
            for capture in captures {
                drop(capture);
                std::thread::yield_now();
            }
            drop_done.store(true, Ordering::Release);
        });

        barrier.wait();
        while !done.load(Ordering::Acquire) {
            let stats = db.snapshot_retention_stats();
            assert!(stats.active_captures <= MAX_READ_SNAPSHOTS);
            assert!(stats.retained_logical_bytes <= bytes_per_capture * MAX_READ_SNAPSHOTS as u64);
            std::thread::yield_now();
        }
        dropper.join().unwrap();
        assert_eq!(
            db.snapshot_retention_stats(),
            SnapshotRetentionStats::default()
        );
    }

    #[test]
    fn durability_group_syncs_once_rejects_nested_captures_and_preserves_validation_boundaries() {
        let mut db = database();
        let mut syncs = 0;
        let results = db
            .durable_group_with_sync(
                |db| {
                    assert!(db.read_snapshot().is_err());
                    assert!(db.durable_group(|_| ()).is_err());
                    [
                        db.put(1, &[1.0], b"one"),
                        db.put(2, &[f32::NAN], b"invalid"),
                        db.put(3, &[3.0], b"three"),
                    ]
                },
                |_| {
                    syncs += 1;
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(syncs, 1);
        assert!(results[0].is_ok() && results[1].is_err() && results[2].is_ok());
        assert_eq!(db.len(), 2);
        let bytes = db.to_bytes().unwrap();
        let recovered = Database::open_from_bytes(bytes, None).unwrap();
        assert_eq!(recovered.len(), 2);
        assert!(recovered.get(2).unwrap().is_none());
    }

    #[test]
    fn group_final_sync_failure_and_unwind_poison_owner_but_not_prior_snapshots() {
        let mut db = database();
        db.put(1, &[1.0], b"committed").unwrap();
        let before = db.read_snapshot().unwrap();
        db.fail_next_group_sync = true;
        let result = db.durable_group(|db| db.put(1, &[2.0], b"unacknowledged"));
        assert!(matches!(result, Err(Error::CommitIndeterminate(_))));
        assert!(matches!(db.get(1), Err(Error::Poisoned)));
        assert!(matches!(db.read_snapshot(), Err(Error::Poisoned)));
        assert_eq!(before.get(1).unwrap().unwrap().payload, b"committed");
        let mut db = database();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = db.durable_group(|db| {
                db.put(2, &[2.0], b"").unwrap();
                panic!("injected callback unwind");
            });
        }));
        assert!(result.is_err());
        assert!(db.poisoned);
        assert!(!db.group_active);
    }
}
impl ReadSnapshot {
    pub fn browse(&self, after: Option<u64>, limit: usize) -> Result<BrowsePage> {
        self.browse_at(after, limit, self.captured_at_ms)
    }
    pub fn browse_at(&self, after: Option<u64>, limit: usize, now_ms: u64) -> Result<BrowsePage> {
        if !(1..=MAX_BROWSE_PAGE).contains(&limit) {
            return Err(Error::InvalidQuery("browse limit must be 1..=1000"));
        }
        use std::ops::Bound::{Excluded, Unbounded};
        let mut hits: Vec<_> = self
            .records
            .range((after.map_or(Unbounded, Excluded), Unbounded))
            .filter(|(id, _)| {
                !self
                    .expiries
                    .get(id)
                    .is_some_and(|expiry| *expiry <= now_ms)
            })
            .take(limit + 1)
            .map(|(&id, record)| BrowseHit {
                id,
                record: record.clone(),
            })
            .collect();
        let more = hits.len() > limit;
        hits.truncate(limit);
        let next_after = more.then(|| hits.last().expect("nonempty page").id);
        Ok(BrowsePage { hits, next_after })
    }
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<SearchHit>> {
        self.search_filtered_with_metric(query, k, &[], DistanceMetric::SquaredL2)
    }
    pub fn search_filtered_with_metric(
        &self,
        query: &[f32],
        k: usize,
        conditions: &[Condition],
        metric: DistanceMetric,
    ) -> Result<Vec<SearchHit>> {
        self.search_filtered_with_metric_at(query, k, conditions, metric, self.captured_at_ms)
    }
    pub fn search_filtered_with_metric_at(
        &self,
        query: &[f32],
        k: usize,
        conditions: &[Condition],
        metric: DistanceMetric,
        now_ms: u64,
    ) -> Result<Vec<SearchHit>> {
        self.search_with_stats_at(
            query,
            k,
            conditions,
            metric,
            now_ms,
            &mut crate::SearchStats::default(),
        )
    }
    pub(crate) fn search_with_stats_at(
        &self,
        query: &[f32],
        k: usize,
        conditions: &[Condition],
        metric: DistanceMetric,
        now_ms: u64,
        stats: &mut crate::SearchStats,
    ) -> Result<Vec<SearchHit>> {
        validate_vector(self.dimensions, query)?;
        metadata::validate_filter(conditions)?;
        let norm = crate::validate_metric_query(query, metric)?;
        Ok(crate::exact_search(
            &self.records,
            &self.expiries,
            query,
            norm,
            k,
            conditions,
            metric,
            None,
            now_ms,
            stats,
        ))
    }
}

/// Maximum number of records returned by one embedded browse call.
pub const MAX_BROWSE_PAGE: usize = 1_000;

#[derive(Clone, Debug, PartialEq)]
pub struct BrowseHit {
    pub id: u64,
    pub record: Record,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BrowsePage {
    pub hits: Vec<BrowseHit>,
    pub next_after: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchGroup {
    pub value: MetadataValue,
    pub hits: Vec<SearchHit>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FacetCount {
    pub value: MetadataValue,
    pub count: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NumericStats {
    pub count: u64,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub sum: f64,
    pub average: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeoSort {
    VectorDistance,
    GeoDistance,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GeoSearchHit {
    pub id: u64,
    pub vector_distance: f64,
    pub geo_distance_meters: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecommendationQuery {
    pub positive_ids: Vec<u64>,
    pub negative_ids: Vec<u64>,
    pub positive_vectors: Vec<Vec<f32>>,
    pub negative_vectors: Vec<Vec<f32>>,
    pub k: usize,
    pub conditions: Vec<Condition>,
    pub metric: DistanceMetric,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GeoSearchQuery {
    pub vector: Vec<f32>,
    pub k: usize,
    pub field: String,
    pub latitude: f64,
    pub longitude: f64,
    pub radius_meters: f64,
    pub conditions: Vec<Condition>,
    pub metric: DistanceMetric,
    pub sort: GeoSort,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ValueKey {
    String(String),
    Integer(i64),
    Float(u64),
    Boolean(bool),
    Geo(u64, u64),
}

fn value_key(value: &MetadataValue) -> ValueKey {
    match value {
        MetadataValue::String(value) => ValueKey::String(value.clone()),
        MetadataValue::Integer(value) => ValueKey::Integer(*value),
        MetadataValue::Float(value) => {
            ValueKey::Float(if *value == 0.0 { 0.0f64 } else { *value }.to_bits())
        }
        MetadataValue::Boolean(value) => ValueKey::Boolean(*value),
        MetadataValue::GeoPoint {
            latitude,
            longitude,
        } => ValueKey::Geo(
            if *latitude == 0.0 { 0.0f64 } else { *latitude }.to_bits(),
            if *longitude == 0.0 {
                0.0f64
            } else {
                *longitude
            }
            .to_bits(),
        ),
    }
}

fn compare_values(left: &MetadataValue, right: &MetadataValue) -> std::cmp::Ordering {
    use MetadataValue::*;
    let rank = |value: &MetadataValue| match value {
        String(_) => 0,
        Integer(_) => 1,
        Float(_) => 2,
        Boolean(_) => 3,
        GeoPoint { .. } => 4,
    };
    rank(left)
        .cmp(&rank(right))
        .then_with(|| match (left, right) {
            (String(a), String(b)) => a.cmp(b),
            (Integer(a), Integer(b)) => a.cmp(b),
            (Float(a), Float(b)) => a.total_cmp(b),
            (Boolean(a), Boolean(b)) => a.cmp(b),
            (
                GeoPoint {
                    latitude: a1,
                    longitude: a2,
                },
                GeoPoint {
                    latitude: b1,
                    longitude: b2,
                },
            ) => a1.total_cmp(b1).then_with(|| a2.total_cmp(b2)),
            _ => std::cmp::Ordering::Equal,
        })
}

fn valid_field(field: &str) -> bool {
    !field.is_empty() && field.len() <= 256
}

impl CollectionRef<'_> {
    /// Exact filtered retrieval at a caller-provided Unix-millisecond time.
    pub fn search_filtered_with_metric_at(
        &self,
        query: &[f32],
        k: usize,
        conditions: &[Condition],
        metric: DistanceMetric,
        now_ms: u64,
    ) -> Result<Vec<SearchHit>> {
        self.db.search_in_at(
            self.id,
            query,
            k,
            conditions,
            metric,
            now_ms,
            &mut crate::SearchStats::default(),
        )
    }

    /// Returns a bounded page of records by ascending ID, excluding expired records.
    pub fn browse(&self, after: Option<u64>, limit: usize) -> Result<BrowsePage> {
        self.browse_at(after, limit, current_time_ms())
    }

    /// Deterministic form of `browse` for tests and applications with an injected clock.
    pub fn browse_at(&self, after: Option<u64>, limit: usize, now_ms: u64) -> Result<BrowsePage> {
        self.db.ensure_healthy()?;
        if !(1..=MAX_BROWSE_PAGE).contains(&limit) {
            return Err(Error::InvalidQuery("browse limit must be 1..=1000"));
        }
        let state = self.db.state(self.id);
        let mut selected = Vec::with_capacity(limit.saturating_add(1));
        for (&id, record) in state
            .records
            .iter()
            .filter(|(id, _)| after.is_none_or(|after| **id > after))
        {
            if state
                .expiries
                .get(&id)
                .is_some_and(|expiry| *expiry <= now_ms)
            {
                continue;
            }
            selected.push(BrowseHit {
                id,
                record: record.clone(),
            });
            if selected.len() > limit {
                break;
            }
        }
        let has_more = selected.len() > limit;
        selected.truncate(limit);
        let next_after = has_more.then(|| selected.last().expect("nonempty page").id);
        Ok(BrowsePage {
            hits: selected,
            next_after,
        })
    }

    /// Groups exact vector-search hits by a typed metadata value. Records without
    /// the grouping field are omitted; groups follow their nearest hit ordering.
    pub fn search_grouped(
        &self,
        query: &[f32],
        groups: usize,
        hits_per_group: usize,
        field: &str,
        conditions: &[Condition],
        metric: DistanceMetric,
    ) -> Result<Vec<SearchGroup>> {
        self.db.ensure_healthy()?;
        if !valid_field(field) {
            return Err(Error::InvalidQuery("group field must be 1..=256 bytes"));
        }
        let state = self.db.state(self.id);
        let hits = self.db.search_in(
            self.id,
            query,
            state.records.len(),
            conditions,
            metric,
            &mut crate::SearchStats::default(),
        )?;
        if groups == 0 || hits_per_group == 0 {
            return Ok(Vec::new());
        }
        let mut output: Vec<SearchGroup> = Vec::new();
        let mut positions: HashMap<ValueKey, usize> = HashMap::new();
        for hit in hits {
            let Some(value) = state.records[&hit.id].metadata.get(field) else {
                continue;
            };
            let key = value_key(value);
            if let Some(&position) = positions.get(&key) {
                if output[position].hits.len() < hits_per_group {
                    output[position].hits.push(hit);
                }
            } else if output.len() < groups {
                positions.insert(key, output.len());
                output.push(SearchGroup {
                    value: value.clone(),
                    hits: vec![hit],
                });
            }
        }
        Ok(output)
    }

    /// Recommendation from positive and negative example IDs and/or vectors.
    /// The query is the positive mean minus the negative mean. Example IDs are
    /// excluded from results. At least one positive example is required.
    pub fn recommend(&self, request: &RecommendationQuery) -> Result<Vec<SearchHit>> {
        let positive_ids = &request.positive_ids;
        let negative_ids = &request.negative_ids;
        let positive_vectors = &request.positive_vectors;
        let negative_vectors = &request.negative_vectors;
        let k = request.k;
        let conditions = &request.conditions;
        let metric = request.metric;
        self.db.ensure_healthy()?;
        let state = self.db.state(self.id);
        metadata::validate_filter(conditions)?;
        let now_ms = current_time_ms();
        let mut excluded = std::collections::BTreeSet::new();
        let mut positives = Vec::new();
        let mut negatives = Vec::new();
        for &id in positive_ids.iter().chain(negative_ids) {
            if !excluded.insert(id) {
                return Err(Error::InvalidQuery(
                    "recommendation example IDs must be unique",
                ));
            }
            let record = state
                .records
                .get(&id)
                .filter(|_| {
                    !state
                        .expiries
                        .get(&id)
                        .is_some_and(|expiry| *expiry <= now_ms)
                })
                .ok_or(Error::InvalidQuery(
                    "recommendation example ID is missing or expired",
                ))?;
            if positive_ids.contains(&id) {
                positives.push(record.vector.as_slice());
            } else {
                negatives.push(record.vector.as_slice());
            }
        }
        for vector in positive_vectors.iter().chain(negative_vectors) {
            validate_vector(state.dimensions, vector)?;
        }
        if positives.is_empty() && positive_vectors.is_empty() {
            return Err(Error::InvalidQuery(
                "recommendation needs at least one positive example",
            ));
        }
        let positive_count = positives.len() + positive_vectors.len();
        let negative_count = negatives.len() + negative_vectors.len();
        let mut query = vec![0.0f32; state.dimensions];
        for dimension in 0..state.dimensions {
            let positive_sum: f64 = positives
                .iter()
                .map(|vector| f64::from(vector[dimension]))
                .sum::<f64>()
                + positive_vectors
                    .iter()
                    .map(|vector| f64::from(vector[dimension]))
                    .sum::<f64>();
            let negative_sum: f64 = negatives
                .iter()
                .map(|vector| f64::from(vector[dimension]))
                .sum::<f64>()
                + negative_vectors
                    .iter()
                    .map(|vector| f64::from(vector[dimension]))
                    .sum::<f64>();
            let value = positive_sum / positive_count as f64
                - if negative_count == 0 {
                    0.0
                } else {
                    negative_sum / negative_count as f64
                };
            query[dimension] = value as f32;
            if !query[dimension].is_finite() {
                return Err(Error::NonFiniteVector);
            }
        }
        let all = self.db.search_in_at(
            self.id,
            &query,
            state.records.len(),
            conditions,
            metric,
            now_ms,
            &mut crate::SearchStats::default(),
        )?;
        Ok(all
            .into_iter()
            .filter(|hit| !excluded.contains(&hit.id))
            .take(k)
            .collect())
    }

    /// Counts typed values for one metadata field under an optional AND filter.
    pub fn facets(&self, field: &str, conditions: &[Condition]) -> Result<Vec<FacetCount>> {
        self.db.ensure_healthy()?;
        if !valid_field(field) {
            return Err(Error::InvalidQuery("facet field must be 1..=256 bytes"));
        }
        metadata::validate_filter(conditions)?;
        let now_ms = current_time_ms();
        let state = self.db.state(self.id);
        let mut counts: HashMap<ValueKey, (MetadataValue, u64)> = HashMap::new();
        for (&id, record) in &state.records {
            if state
                .expiries
                .get(&id)
                .is_some_and(|expiry| *expiry <= now_ms)
                || !metadata::matches(&record.metadata, conditions)
            {
                continue;
            }
            if let Some(value) = record.metadata.get(field) {
                let entry = counts
                    .entry(value_key(value))
                    .or_insert_with(|| (value.clone(), 0));
                entry.1 += 1;
            }
        }
        let mut facets: Vec<_> = counts
            .into_values()
            .map(|(value, count)| FacetCount { value, count })
            .collect();
        facets.sort_by(|a, b| compare_values(&a.value, &b.value));
        Ok(facets)
    }

    /// Numeric min/max/sum/count/average over integer and float metadata values.
    pub fn numeric_stats(&self, field: &str, conditions: &[Condition]) -> Result<NumericStats> {
        self.db.ensure_healthy()?;
        if !valid_field(field) {
            return Err(Error::InvalidQuery("stats field must be 1..=256 bytes"));
        }
        metadata::validate_filter(conditions)?;
        let now_ms = current_time_ms();
        let state = self.db.state(self.id);
        let mut result = NumericStats::default();
        for (&id, record) in &state.records {
            if state
                .expiries
                .get(&id)
                .is_some_and(|expiry| *expiry <= now_ms)
                || !metadata::matches(&record.metadata, conditions)
            {
                continue;
            }
            let value = match record.metadata.get(field) {
                Some(MetadataValue::Integer(value)) => *value as f64,
                Some(MetadataValue::Float(value)) => *value,
                _ => continue,
            };
            result.count += 1;
            result.min = Some(result.min.map_or(value, |current| current.min(value)));
            result.max = Some(result.max.map_or(value, |current| current.max(value)));
            result.sum += value;
            if !result.sum.is_finite() {
                return Err(Error::InvalidQuery("numeric stats sum overflow"));
            }
        }
        result.average = (result.count != 0).then_some(result.sum / result.count as f64);
        Ok(result)
    }

    /// Vector search constrained to a spherical radius around a geo-point field.
    /// Vector distance remains available; `sort` selects vector or geographic ordering.
    pub fn search_geo(&self, request: &GeoSearchQuery) -> Result<Vec<GeoSearchHit>> {
        let query = &request.vector;
        let k = request.k;
        let field = request.field.as_str();
        let latitude = request.latitude;
        let longitude = request.longitude;
        let radius_meters = request.radius_meters;
        let conditions = &request.conditions;
        let metric = request.metric;
        let sort = request.sort;
        if !valid_field(field) {
            return Err(Error::InvalidQuery("geo field must be 1..=256 bytes"));
        }
        let mut filters = conditions.to_vec();
        filters.push(Condition::GeoRadius {
            field: field.to_owned(),
            latitude,
            longitude,
            radius_meters,
        });
        metadata::validate_filter(&filters)?;
        let state = self.db.state(self.id);
        let limit = if sort == GeoSort::GeoDistance {
            state.records.len()
        } else {
            k
        };
        let hits = self.db.search_in(
            self.id,
            query,
            limit,
            &filters,
            metric,
            &mut crate::SearchStats::default(),
        )?;
        let mut output: Vec<_> = hits
            .into_iter()
            .map(|hit| {
                let (lat, lon) = match state.records[&hit.id]
                    .metadata
                    .get(field)
                    .expect("radius filter matched")
                {
                    MetadataValue::GeoPoint {
                        latitude,
                        longitude,
                    } => (*latitude, *longitude),
                    _ => unreachable!("radius filter only matches geo points"),
                };
                GeoSearchHit {
                    id: hit.id,
                    vector_distance: hit.distance,
                    geo_distance_meters: metadata::geo_distance_meters(
                        latitude, longitude, lat, lon,
                    ),
                }
            })
            .collect();
        output.sort_by(|a, b| match sort {
            GeoSort::VectorDistance => a
                .vector_distance
                .total_cmp(&b.vector_distance)
                .then_with(|| a.id.cmp(&b.id)),
            GeoSort::GeoDistance => a
                .geo_distance_meters
                .total_cmp(&b.geo_distance_meters)
                .then_with(|| a.id.cmp(&b.id)),
        });
        output.truncate(k);
        Ok(output)
    }
}

impl CollectionMut<'_> {
    pub fn browse(&self, after: Option<u64>, limit: usize) -> Result<BrowsePage> {
        self.view().browse(after, limit)
    }
    pub fn search_grouped(
        &self,
        query: &[f32],
        groups: usize,
        hits_per_group: usize,
        field: &str,
        conditions: &[Condition],
        metric: DistanceMetric,
    ) -> Result<Vec<SearchGroup>> {
        self.view()
            .search_grouped(query, groups, hits_per_group, field, conditions, metric)
    }
    pub fn recommend(&self, query: &RecommendationQuery) -> Result<Vec<SearchHit>> {
        self.view().recommend(query)
    }
    pub fn facets(&self, field: &str, conditions: &[Condition]) -> Result<Vec<FacetCount>> {
        self.view().facets(field, conditions)
    }
    pub fn numeric_stats(&self, field: &str, conditions: &[Condition]) -> Result<NumericStats> {
        self.view().numeric_stats(field, conditions)
    }
    pub fn search_geo(&self, query: &GeoSearchQuery) -> Result<Vec<GeoSearchHit>> {
        self.view().search_geo(query)
    }
}

impl Database {
    pub fn browse(&self, after: Option<u64>, limit: usize) -> Result<BrowsePage> {
        CollectionRef { db: self, id: 0 }.browse(after, limit)
    }
    pub fn browse_at(&self, after: Option<u64>, limit: usize, now_ms: u64) -> Result<BrowsePage> {
        CollectionRef { db: self, id: 0 }.browse_at(after, limit, now_ms)
    }
    pub fn search_grouped(
        &self,
        query: &[f32],
        groups: usize,
        hits_per_group: usize,
        field: &str,
        conditions: &[Condition],
        metric: DistanceMetric,
    ) -> Result<Vec<SearchGroup>> {
        CollectionRef { db: self, id: 0 }.search_grouped(
            query,
            groups,
            hits_per_group,
            field,
            conditions,
            metric,
        )
    }
    pub fn recommend(&self, query: &RecommendationQuery) -> Result<Vec<SearchHit>> {
        CollectionRef { db: self, id: 0 }.recommend(query)
    }
    pub fn facets(&self, field: &str, conditions: &[Condition]) -> Result<Vec<FacetCount>> {
        CollectionRef { db: self, id: 0 }.facets(field, conditions)
    }
    pub fn numeric_stats(&self, field: &str, conditions: &[Condition]) -> Result<NumericStats> {
        CollectionRef { db: self, id: 0 }.numeric_stats(field, conditions)
    }
    pub fn search_geo(&self, query: &GeoSearchQuery) -> Result<Vec<GeoSearchHit>> {
        CollectionRef { db: self, id: 0 }.search_geo(query)
    }
}
