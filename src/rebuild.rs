//! Opt-in background graph construction; publication stays on the database owner.
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

use crate::{DistanceMetric, Error, HnswConfig, Record, Result, hnsw::Index};

/// Bounds snapshot vectors and graph cardinality, not allocator/total process RSS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HnswRebuildLimits {
    pub max_records: usize,
    pub max_snapshot_vector_bytes: usize,
}

impl Default for HnswRebuildLimits {
    fn default() -> Self {
        Self {
            max_records: 100_000,
            max_snapshot_vector_bytes: 128 * 1024 * 1024,
        }
    }
}

impl HnswRebuildLimits {
    pub(crate) fn validate(self, records: usize, dimensions: usize) -> Result<()> {
        if !(1..=1_000_000).contains(&self.max_records)
            || !(1..=256 * 1024 * 1024).contains(&self.max_snapshot_vector_bytes)
        {
            return Err(Error::InvalidHnswRebuildLimits);
        }
        let bytes = records
            .checked_mul(dimensions)
            .and_then(|n| n.checked_mul(4));
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
}

impl Job {
    pub fn start(
        records: &BTreeMap<u64, Record>,
        config: HnswConfig,
        metric: DistanceMetric,
    ) -> Result<Self> {
        // Do not copy opaque payloads or metadata into the worker.
        let snapshot = records
            .iter()
            .map(|(&id, record)| {
                (
                    id,
                    Record {
                        vector: record.vector.clone(),
                        payload: Vec::new(),
                        metadata: BTreeMap::new(),
                    },
                )
            })
            .collect();
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancel);
        let worker = thread::Builder::new()
            .name("vlite-hnsw-rebuild".into())
            .spawn(move || Index::build_cancellable(&snapshot, config, metric, &stop))?;
        Ok(Self {
            worker: Some(worker),
            cancel,
            terminal: None,
            changes: Some(Index::build(&BTreeMap::new(), config, metric)),
        })
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

    fn failed_job() -> Job {
        Job {
            worker: Some(thread::spawn(|| {
                panic!("injected background worker failure")
            })),
            cancel: Arc::new(AtomicBool::new(false)),
            terminal: None,
            changes: Some(Index::build(
                &BTreeMap::new(),
                HnswConfig::default(),
                DistanceMetric::SquaredL2,
            )),
        }
    }

    #[test]
    fn worker_failure_is_reported_without_replacing_ready_index_or_poisoning_storage() {
        let directory = tempfile::tempdir().unwrap();
        let mut db = Database::create(directory.path().join("data.vlite"), 2).unwrap();
        db.put(1, &[1.0, 2.0], b"").unwrap();
        db.build_hnsw(HnswConfig::default()).unwrap();
        db.rebuild = Some(failed_job());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !db.rebuild.as_ref().unwrap().finished() {
            assert!(Instant::now() < deadline);
            thread::yield_now();
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
        db.rebuild = Some(failed_job());
        assert!(matches!(db.close(), Err(Error::HnswRebuildFailed)));
    }

    #[test]
    fn byte_catchup_limit_and_repeated_ids_reuse_existing_index_accounting() {
        let mut job = Job::start(
            &BTreeMap::new(),
            HnswConfig::default(),
            DistanceMetric::SquaredL2,
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
                &BTreeMap::new(),
                HnswConfig::default(),
                DistanceMetric::Cosine,
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
        assert!(db.index.is_none());
        assert!(matches!(db.close(), Err(Error::Poisoned)));
    }
}
