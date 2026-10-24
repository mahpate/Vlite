//! Opt-in background graph construction; publication stays on the database owner.
#[cfg(test)]
use std::collections::BTreeMap;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
};

use crate::{
    AnnOptions, DistanceMetric, Error, HnswConfig, MAX_HNSW_BUFFER_IDS,
    MAX_HNSW_BUFFER_VECTOR_BYTES, Quantization, Record, Result,
    hnsw::{Index, QuantizedSnapshot},
};

/// Explicit caller-driven high-water policy. No timers or hidden retries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HnswMaintenancePolicy {
    pub rebuild_at_buffered_ids: usize,
    pub rebuild_at_buffer_vector_bytes: usize,
    pub limits: HnswRebuildLimits,
}

impl Default for HnswMaintenancePolicy {
    fn default() -> Self {
        Self {
            rebuild_at_buffered_ids: MAX_HNSW_BUFFER_IDS / 2,
            rebuild_at_buffer_vector_bytes: MAX_HNSW_BUFFER_VECTOR_BYTES / 2,
            limits: HnswRebuildLimits::default(),
        }
    }
}

impl HnswMaintenancePolicy {
    pub(crate) fn validate(self) -> Result<()> {
        if !(1..=MAX_HNSW_BUFFER_IDS).contains(&self.rebuild_at_buffered_ids)
            || !(1..=MAX_HNSW_BUFFER_VECTOR_BYTES).contains(&self.rebuild_at_buffer_vector_bytes)
        {
            return Err(Error::InvalidHnswMaintenancePolicy);
        }
        self.limits.validate(0, 1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HnswBufferCapacity {
    pub buffered_ids: usize,
    pub buffer_vector_bytes: usize,
    pub remaining_ids: usize,
    pub remaining_vector_bytes: usize,
}

impl HnswBufferCapacity {
    pub(crate) fn of(index: &Index) -> Self {
        let (buffered_ids, buffer_vector_bytes) = index.buffer_usage();
        Self {
            buffered_ids,
            buffer_vector_bytes,
            remaining_ids: MAX_HNSW_BUFFER_IDS - buffered_ids,
            remaining_vector_bytes: MAX_HNSW_BUFFER_VECTOR_BYTES - buffer_vector_bytes,
        }
    }

    pub(crate) fn exhausted(self) -> bool {
        self.remaining_ids == 0 || self.remaining_vector_bytes == 0
    }

    pub(crate) fn high(self, policy: HnswMaintenancePolicy) -> bool {
        self.buffered_ids >= policy.rebuild_at_buffered_ids
            || self.buffer_vector_bytes >= policy.rebuild_at_buffer_vector_bytes
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HnswPressure {
    Healthy,
    RebuildRecommended,
    CapacityExhausted,
    Unavailable,
    CatchUpOverflow,
}

/// Processed records, not percent CPU time or an estimated completion deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HnswRebuildProgress {
    pub config: HnswConfig,
    pub metric: DistanceMetric,
    pub snapshot_records: usize,
    pub processed_records: usize,
    pub worker_finished: bool,
    pub terminal_reason: Option<HnswRebuildStatus>,
    pub catchup_buffer: Option<HnswBufferCapacity>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HnswMaintenanceStatus {
    pub ready: bool,
    pub pressure: HnswPressure,
    pub active_buffer: Option<HnswBufferCapacity>,
    pub rebuild: Option<HnswRebuildProgress>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HnswMaintenanceReport {
    pub outcome: HnswRebuildStatus,
    pub started: bool,
    pub status: HnswMaintenanceStatus,
}

/// Bounds snapshot vectors and graph cardinality, not allocator/total process RSS.
/// Shared unquantized record bodies also obey `MAX_HNSW_RETAINED_RECORD_BYTES`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HnswRebuildLimits {
    pub max_records: usize,
    pub max_snapshot_vector_bytes: usize,
}

/// Shared unquantized generations retain complete immutable record bodies.
/// This additional cap prevents payload-heavy generations being retained
/// without a byte bound while the owner replaces their records.
pub const MAX_HNSW_RETAINED_RECORD_BYTES: u64 = 256 * 1024 * 1024;

impl Default for HnswRebuildLimits {
    fn default() -> Self {
        Self {
            max_records: 100_000,
            max_snapshot_vector_bytes: 128 * 1024 * 1024,
        }
    }
}

impl HnswRebuildLimits {
    /// f32 snapshot: `records × dimensions × 4` bytes count against the limit.
    pub(crate) fn validate(self, records: usize, dimensions: usize) -> Result<()> {
        self.validate_snapshot(records, dimensions.checked_mul(4))
    }

    /// Snapshot for `options`: f32 vectors (4 bytes per component) or, for
    /// ScalarU8, u8 codes (1 byte per component) count against the limit.
    pub(crate) fn validate_for(
        self,
        records: usize,
        dimensions: usize,
        options: AnnOptions,
    ) -> Result<()> {
        match options.quantization {
            Quantization::None => self.validate(records, dimensions),
            Quantization::ScalarU8 => self.validate_snapshot(records, Some(dimensions)),
        }
    }

    fn validate_snapshot(self, records: usize, record_bytes: Option<usize>) -> Result<()> {
        if !(1..=1_000_000).contains(&self.max_records)
            || !(1..=256 * 1024 * 1024).contains(&self.max_snapshot_vector_bytes)
        {
            return Err(Error::InvalidHnswRebuildLimits);
        }
        let bytes = record_bytes.and_then(|n| records.checked_mul(n));
        if records > self.max_records || bytes.is_none_or(|n| n > self.max_snapshot_vector_bytes) {
            return Err(Error::HnswRebuildTooLarge);
        }
        Ok(())
    }
}

/// Terminal states consume the job on poll; the following poll returns Idle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HnswRebuildStatus {
    Idle,
    Running,
    Cancelling,
    Published,
    Cancelled,
    Superseded,
    CatchUpOverflow,
}

pub(crate) struct Job {
    worker: Option<JoinHandle<Option<Index>>>,
    cancel: Arc<AtomicBool>,
    terminal: Option<HnswRebuildStatus>,
    changes: Option<Index>,
    config: HnswConfig,
    metric: DistanceMetric,
    options: AnnOptions,
    snapshot_records: usize,
    processed: Arc<AtomicUsize>,
    vector_sequence: u64,
}

impl Job {
    /// Shares an immutable record generation, then builds on one owned thread.
    ///
    /// Unquantized snapshots retain immutable records without copying vectors.
    /// ScalarU8 snapshots are trained and encoded here and hold only IDs and
    /// u8 codes. A writer copies the ID tree, not unchanged record bodies.
    pub fn start(
        records: &crate::records::RecordMap,
        config: HnswConfig,
        metric: DistanceMetric,
        options: AnnOptions,
    ) -> Result<Self> {
        enum Snapshot {
            Vectors(crate::records::RecordMap),
            Codes(QuantizedSnapshot),
        }
        let snapshot = match options.quantization {
            Quantization::ScalarU8 => Snapshot::Codes(QuantizedSnapshot::encode(records, metric)),
            Quantization::None => Snapshot::Vectors(records.clone()),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancel);
        let processed = Arc::new(AtomicUsize::new(0));
        let progress = Arc::clone(&processed);
        let worker = thread::Builder::new()
            .name("vlite-hnsw-rebuild".into())
            .spawn(move || match snapshot {
                Snapshot::Vectors(records) => {
                    Index::build_with_progress(&records, config, metric, options, &stop, &progress)
                }
                Snapshot::Codes(codes) => {
                    Index::build_quantized(codes, config, metric, options, &stop, &progress)
                }
            })?;
        Ok(Self {
            worker: Some(worker),
            cancel,
            terminal: None,
            changes: Some(Index::build(
                &crate::records::RecordMap::new(),
                config,
                metric,
                options,
            )),
            config,
            metric,
            options,
            snapshot_records: records.len(),
            processed,
            vector_sequence: 0,
        })
    }

    pub fn options(&self) -> AnnOptions {
        self.options
    }

    pub fn set_vector_sequence(&mut self, sequence: u64) {
        self.vector_sequence = sequence;
    }

    pub fn covers_vector_sequence(&self, sequence: u64) -> bool {
        self.vector_sequence == sequence
    }

    pub fn progress(&self) -> HnswRebuildProgress {
        HnswRebuildProgress {
            config: self.config,
            metric: self.metric,
            snapshot_records: self.snapshot_records,
            processed_records: self.processed.load(Ordering::Relaxed),
            worker_finished: self.finished(),
            terminal_reason: self.terminal,
            catchup_buffer: self.changes.as_ref().map(HnswBufferCapacity::of),
        }
    }

    pub fn stop(&mut self, reason: HnswRebuildStatus) {
        if self.terminal.is_none() {
            self.terminal = Some(reason);
            self.changes = None;
        }
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn track(&mut self, id: u64, previous: Option<&Record>, current_bytes: usize) {
        if let Some(changes) = &mut self.changes
            && !changes.track(id, previous, current_bytes)
        {
            self.stop(HnswRebuildStatus::CatchUpOverflow);
        }
    }

    pub fn finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub fn status(&self) -> HnswRebuildStatus {
        if self.terminal.is_some() {
            HnswRebuildStatus::Cancelling
        } else {
            HnswRebuildStatus::Running
        }
    }

    pub fn finish(mut self) -> Result<(HnswRebuildStatus, Option<Index>)> {
        let index = self
            .worker
            .take()
            .expect("worker is joined once")
            .join()
            .map_err(|_| Error::HnswRebuildFailed)?;
        if let Some(reason) = self.terminal {
            return Ok((reason, None));
        }
        let mut index = index.ok_or(Error::HnswRebuildFailed)?;
        index.adopt_buffer(
            self.changes
                .take()
                .expect("uncancelled job has catch-up buffer"),
        );
        Ok((HnswRebuildStatus::Published, Some(index)))
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            // Drop cannot report a panic; poll/close report it through finish().
            let _ = worker.join();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Database, MAX_DIMENSIONS};
    use std::time::{Duration, Instant};

    #[test]
    fn publication_rejects_a_generation_not_covered_by_catchup() {
        let mut db = Database::create_in_memory(1, crate::CreateOptions::default()).unwrap();
        db.put(1, &[1.0], b"").unwrap();
        db.build_hnsw(HnswConfig::default()).unwrap();
        db.start_hnsw_rebuild(
            HnswConfig::default(),
            DistanceMetric::SquaredL2,
            HnswRebuildLimits::default(),
        )
        .unwrap();
        // Simulate a state replacement that bypasses the committed catch-up hook.
        db.state_mut(0).vector_sequence += 1;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match db.poll_hnsw_rebuild().unwrap() {
                HnswRebuildStatus::Running => {
                    assert!(Instant::now() < deadline);
                    thread::yield_now();
                }
                status => {
                    assert_eq!(status, HnswRebuildStatus::Superseded);
                    break;
                }
            }
        }
        assert!(
            db.search_approximate_ready(&[1.0], 1, 1, HnswConfig::default())
                .is_ok()
        );
    }

    #[test]
    fn shared_rebuild_retained_body_budget_includes_payloads() {
        let mut db = Database::create_in_memory(1, crate::CreateOptions::default()).unwrap();
        db.state_mut(0).live_body_bytes = MAX_HNSW_RETAINED_RECORD_BYTES + 1;
        assert!(matches!(
            db.start_hnsw_rebuild(
                HnswConfig::default(),
                DistanceMetric::SquaredL2,
                HnswRebuildLimits::default()
            ),
            Err(Error::HnswRebuildTooLarge)
        ));
        assert!(db.rebuild.is_none());
    }

    fn failed_job() -> Job {
        Job {
            worker: Some(thread::spawn(|| {
                panic!("injected background worker failure")
            })),
            cancel: Arc::new(AtomicBool::new(false)),
            terminal: None,
            changes: Some(Index::build(
                &crate::records::RecordMap::new(),
                HnswConfig::default(),
                DistanceMetric::SquaredL2,
                AnnOptions::default(),
            )),
            config: HnswConfig::default(),
            metric: DistanceMetric::SquaredL2,
            options: AnnOptions::default(),
            snapshot_records: 0,
            processed: Arc::new(AtomicUsize::new(0)),
            vector_sequence: 0,
        }
    }

    #[test]
    fn worker_failure_is_reported_without_replacing_ready_index_or_poisoning_storage() {
        let directory = tempfile::tempdir().unwrap();
        let mut db = Database::create(directory.path().join("data.vlite"), 2).unwrap();
        db.put(1, &[1.0, 2.0], b"").unwrap();
        db.build_hnsw(HnswConfig::default()).unwrap();
        db.rebuild = Some((0, failed_job()));
        let deadline = Instant::now() + Duration::from_secs(600);
        while !db.rebuild.as_ref().unwrap().1.finished() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        assert!(matches!(
            db.poll_hnsw_rebuild(),
            Err(Error::HnswRebuildFailed)
        ));
        assert_eq!(db.poll_hnsw_rebuild().unwrap(), HnswRebuildStatus::Idle);
        assert_eq!(
            db.search_approximate_ready(&[1.0, 2.0], 1, 1, HnswConfig::default())
                .unwrap()[0]
                .id,
            1
        );
        db.put(2, &[2.0, 3.0], b"still writable").unwrap();
        db.rebuild = Some((0, failed_job()));
        assert!(matches!(db.close(), Err(Error::HnswRebuildFailed)));
    }

    #[test]
    fn byte_catchup_limit_and_repeated_ids_reuse_existing_index_accounting() {
        let mut job = Job::start(
            &crate::records::RecordMap::new(),
            HnswConfig::default(),
            DistanceMetric::SquaredL2,
            AnnOptions::default(),
        )
        .unwrap();
        let record = Record {
            vector: vec![1.0; MAX_DIMENSIONS],
            payload: vec![],
            metadata: BTreeMap::new(),
        };
        for id in 0..16 {
            job.track(id, Some(&record), MAX_DIMENSIONS * 4);
        }
        assert_eq!(
            job.changes.as_ref().unwrap().buffer_usage(),
            (16, 8 * 1024 * 1024)
        );
        for _ in 0..1000 {
            job.track(0, Some(&record), MAX_DIMENSIONS * 4);
        }
        assert!(job.terminal.is_none());
        job.track(16, Some(&record), MAX_DIMENSIONS * 4);
        assert_eq!(job.finish().unwrap().0, HnswRebuildStatus::CatchUpOverflow);
    }

    #[test]
    fn cancellation_before_build_and_invalid_limits_are_explicit() {
        assert!(
            Index::build_cancellable(
                &crate::records::RecordMap::new(),
                HnswConfig::default(),
                DistanceMetric::Cosine,
                AnnOptions::default(),
                &AtomicBool::new(true)
            )
            .is_none()
        );
        for limits in [
            HnswRebuildLimits {
                max_records: 0,
                max_snapshot_vector_bytes: 1,
            },
            HnswRebuildLimits {
                max_records: 1_000_001,
                max_snapshot_vector_bytes: 1,
            },
            HnswRebuildLimits {
                max_records: 1,
                max_snapshot_vector_bytes: 0,
            },
            HnswRebuildLimits {
                max_records: 1,
                max_snapshot_vector_bytes: 256 * 1024 * 1024 + 1,
            },
        ] {
            assert!(matches!(
                limits.validate(0, 1),
                Err(Error::InvalidHnswRebuildLimits)
            ));
        }
    }

    #[test]
    fn poisoned_owner_cannot_publish_worker_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let mut db = Database::create(directory.path().join("data.vlite"), 2).unwrap();
        db.put(1, &[1.0, 2.0], b"").unwrap();
        db.start_hnsw_rebuild(
            HnswConfig::default(),
            DistanceMetric::Cosine,
            HnswRebuildLimits::default(),
        )
        .unwrap();
        assert!(
            db.commit_frame_with_sync(2, b"invalid frame for injected failure", |_| Err(
                std::io::Error::other("injected sync failure")
            ))
            .is_err()
        );
        assert!(matches!(db.poll_hnsw_rebuild(), Err(Error::Poisoned)));
        assert!(matches!(
            db.start_hnsw_rebuild(
                HnswConfig::default(),
                DistanceMetric::Cosine,
                HnswRebuildLimits::default()
            ),
            Err(Error::Poisoned)
        ));
        assert!(db.default_state().index.is_none());
        assert!(matches!(db.close(), Err(Error::Poisoned)));
    }
}
