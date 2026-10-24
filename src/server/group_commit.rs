//! Bounded FIFO coordinator for record mutations; never acknowledges before sync.

use super::*;
use std::{
    any::Any,
    sync::{Weak, mpsc},
    thread::JoinHandle,
    time::Duration,
};
use tokio::sync::{OwnedSemaphorePermit, oneshot};

/// Opt-in record-write coalescing. Limits include both queued and active writes.
#[derive(Clone, Copy, Debug)]
pub struct GroupCommitConfig {
    pub max_requests: usize,
    pub max_bytes: usize,
    pub max_delay: Duration,
}

impl Default for GroupCommitConfig {
    fn default() -> Self {
        Self {
            max_requests: MAX_IN_FLIGHT,
            max_bytes: 64 * 1024 * 1024,
            max_delay: Duration::ZERO,
        }
    }
}

impl GroupCommitConfig {
    pub fn validate(&self) -> std::io::Result<()> {
        if !(1..=MAX_IN_FLIGHT).contains(&self.max_requests)
            || !(1024..=256 * 1024 * 1024).contains(&self.max_bytes)
            || self.max_delay > Duration::from_millis(100)
        {
            return Err(std::io::Error::other(
                "group commit requires requests 1..16, bytes 1024..268435456, delay 0..100ms",
            ));
        }
        Ok(())
    }
}

type Output = Box<dyn Any + Send>;
type Work = Box<dyn FnOnce(&mut Database) -> Result<Output, ApiError> + Send>;

macro_rules! counters {
    ($($field:ident),+ $(,)?) => {
        #[derive(Default)]
        struct Counters {
            $($field: AtomicU64,)+
        }

        /// Relaxed, independently sampled operational counters, not a transaction snapshot.
        #[derive(Debug, serde::Serialize)]
        pub(super) struct GroupCommitStats {
            $(pub $field: u64,)+
        }

        impl Counters {
            fn snapshot(&self) -> GroupCommitStats {
                GroupCommitStats {
                    $($field: self.$field.load(Ordering::Relaxed),)+
                }
            }
        }
    };
}

counters! {
    admitted_requests, admitted_bytes, peak_requests, peak_bytes, queued_requests,
    rejected_requests, rejected_bytes, rejected_closed,
    queue_wait_count, queue_wait_ns, queue_wait_max_ns,
    sync_count, sync_requests, sync_failures, sync_ns, sync_max_ns,
    disconnected_replies,
}

struct Admission {
    counters: Arc<Counters>,
    bytes: u64,
    enqueued: Instant,
    queued: bool,
}

impl Admission {
    fn new(counters: Arc<Counters>, bytes: usize) -> Self {
        let requests = counters.admitted_requests.fetch_add(1, Ordering::Relaxed) + 1;
        let charged = counters
            .admitted_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed)
            + bytes as u64;
        counters
            .peak_requests
            .fetch_max(requests, Ordering::Relaxed);
        counters.peak_bytes.fetch_max(charged, Ordering::Relaxed);
        counters.queued_requests.fetch_add(1, Ordering::Relaxed);
        Self {
            counters,
            bytes: bytes as u64,
            enqueued: Instant::now(),
            queued: true,
        }
    }

    fn start(&mut self) {
        self.queued = false;
        self.counters
            .queued_requests
            .fetch_sub(1, Ordering::Relaxed);
        let elapsed = elapsed_ns(self.enqueued);
        self.counters
            .queue_wait_count
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .queue_wait_ns
            .fetch_add(elapsed, Ordering::Relaxed);
        self.counters
            .queue_wait_max_ns
            .fetch_max(elapsed, Ordering::Relaxed);
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        if self.queued {
            self.counters
                .queued_requests
                .fetch_sub(1, Ordering::Relaxed);
        }
        self.counters
            .admitted_requests
            .fetch_sub(1, Ordering::Relaxed);
        self.counters
            .admitted_bytes
            .fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

fn elapsed_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos().min(u64::MAX as u128) as u64
}

struct Job {
    operation: Work,
    reply: oneshot::Sender<Result<Output, ApiError>>,
    _worker: OwnedSemaphorePermit,
    _slot: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
    admission: Admission,
}

pub(super) struct Coordinator {
    sender: Mutex<Option<mpsc::SyncSender<Job>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    slots: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    max_bytes: usize,
    counters: Arc<Counters>,
}

impl Coordinator {
    pub fn start(
        database: Weak<RwLock<Option<Database>>>,
        metrics: Arc<Metrics>,
        config: GroupCommitConfig,
    ) -> std::io::Result<Self> {
        config.validate()?;
        let counters = Arc::new(Counters::default());
        let worker_counters = counters.clone();
        let (sender, receiver) = mpsc::sync_channel::<Job>(config.max_requests);
        let thread = std::thread::Builder::new()
            .name("vlite-group-commit".into())
            .spawn(move || {
                while let Ok(first) = receiver.recv() {
                    let deadline = Instant::now() + config.max_delay;
                    let mut jobs = Vec::with_capacity(config.max_requests);
                    jobs.push(first);
                    while jobs.len() < config.max_requests {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        let next = if remaining.is_zero() {
                            receiver.try_recv().map_err(|error| match error {
                                mpsc::TryRecvError::Empty => mpsc::RecvTimeoutError::Timeout,
                                mpsc::TryRecvError::Disconnected => {
                                    mpsc::RecvTimeoutError::Disconnected
                                }
                            })
                        } else {
                            receiver.recv_timeout(remaining)
                        };
                        match next {
                            Ok(job) => jobs.push(job),
                            Err(_) => break,
                        }
                    }
                    commit(&database, &metrics, &worker_counters, jobs);
                }
            })?;
        Ok(Self {
            sender: Mutex::new(Some(sender)),
            thread: Mutex::new(Some(thread)),
            slots: Arc::new(Semaphore::new(config.max_requests)),
            bytes: Arc::new(Semaphore::new(config.max_bytes)),
            max_bytes: config.max_bytes,
            counters,
        })
    }

    pub(super) fn stats(&self) -> GroupCommitStats {
        self.counters.snapshot()
    }

    pub async fn submit<T: Send + 'static>(
        &self,
        bytes: usize,
        worker: OwnedSemaphorePermit,
        operation: impl FnOnce(&mut Database) -> Result<T, ApiError> + Send + 'static,
    ) -> Result<T, ApiError> {
        let busy = || {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "busy",
                "group commit queue or memory budget is full",
            )
        };
        if bytes > self.max_bytes {
            self.counters.rejected_bytes.fetch_add(1, Ordering::Relaxed);
            return Err(busy());
        }
        let slot = self.slots.clone().try_acquire_owned().map_err(|_| {
            self.counters
                .rejected_requests
                .fetch_add(1, Ordering::Relaxed);
            busy()
        })?;
        let bytes = self
            .bytes
            .clone()
            .try_acquire_many_owned(bytes as u32)
            .map_err(|_| {
                self.counters.rejected_bytes.fetch_add(1, Ordering::Relaxed);
                busy()
            })?;
        let (reply, receive) = oneshot::channel();
        let job = Job {
            operation: Box::new(move |db| operation(db).map(|value| Box::new(value) as Output)),
            reply,
            _worker: worker,
            _slot: slot,
            admission: Admission::new(self.counters.clone(), bytes.num_permits()),
            _bytes: bytes,
        };
        self.sender
            .lock()
            .map_err(internal)?
            .as_ref()
            .ok_or_else(|| {
                self.counters
                    .rejected_closed
                    .fetch_add(1, Ordering::Relaxed);
                unavailable()
            })?
            .try_send(job)
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => {
                    self.counters
                        .rejected_requests
                        .fetch_add(1, Ordering::Relaxed);
                    busy()
                }
                mpsc::TrySendError::Disconnected(_) => {
                    self.counters
                        .rejected_closed
                        .fetch_add(1, Ordering::Relaxed);
                    unavailable()
                }
            })?;
        let value = receive
            .await
            .map_err(|_| unavailable())??
            .downcast::<T>()
            .map_err(|_| internal("group commit result type mismatch"))?;
        Ok(*value)
    }

    pub fn shutdown(&self) -> std::io::Result<()> {
        self.sender
            .lock()
            .map_err(|error| std::io::Error::other(error.to_string()))?
            .take();
        if let Some(thread) = self
            .thread
            .lock()
            .map_err(|error| std::io::Error::other(error.to_string()))?
            .take()
        {
            thread
                .join()
                .map_err(|_| std::io::Error::other("group commit worker panicked"))?;
        }
        Ok(())
    }
}

fn commit(
    database: &Weak<RwLock<Option<Database>>>,
    metrics: &Metrics,
    counters: &Counters,
    jobs: Vec<Job>,
) {
    metrics.group_commit_groups.fetch_add(1, Ordering::Relaxed);
    metrics
        .group_commit_requests
        .fetch_add(jobs.len() as u64, Ordering::Relaxed);
    let mut completed = Vec::with_capacity(jobs.len());
    let mut pending = Some(jobs);
    let result = (|| {
        let database = database.upgrade().ok_or_else(unavailable)?;
        let mut owner = database.write().map_err(internal)?;
        let db = owner.as_mut().ok_or_else(unavailable)?;
        let initial_sequence = db.sequence;
        let mut sync_started = None;
        let mut sync_requests = 0;
        let result = db.durable_group(|db| {
            for job in pending.take().into_iter().flatten() {
                let Job {
                    operation,
                    reply,
                    _worker,
                    _slot,
                    _bytes,
                    mut admission,
                } = job;
                admission.start();
                let previous = db.sequence;
                let outcome = operation(db);
                if db.sequence != previous {
                    sync_requests += 1;
                }
                completed.push((reply, outcome, _worker, _slot, _bytes, admission));
            }
            if db.sequence != initial_sequence && !db.poisoned {
                sync_started = Some(Instant::now());
            }
        });
        if let Some(started) = sync_started {
            // Measures the shared durability boundary after the final operation,
            // including health checks and sync bookkeeping, never mutation time.
            let elapsed = elapsed_ns(started);
            counters.sync_count.fetch_add(1, Ordering::Relaxed);
            counters
                .sync_requests
                .fetch_add(sync_requests, Ordering::Relaxed);
            counters.sync_ns.fetch_add(elapsed, Ordering::Relaxed);
            counters.sync_max_ns.fetch_max(elapsed, Ordering::Relaxed);
            if result.is_err() {
                counters.sync_failures.fetch_add(1, Ordering::Relaxed);
            }
        }
        result.map_err(ApiError::from)
    })();
    if let Some(jobs) = pending {
        for job in jobs {
            let error = match &result {
                Err(error) => error.clone(),
                Ok(()) => internal("durability group skipped queued writes"),
            };
            if job.reply.send(Err(error)).is_err() {
                counters
                    .disconnected_replies
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    for (reply, outcome, _worker, _slot, _bytes, _admission) in completed {
        // Validation failures retain their own status. A failed shared sync
        // invalidates every would-be success, including disconnected callers.
        let outcome = match (&result, outcome) {
            (Err(error), Ok(_)) => Err(error.clone()),
            (_, outcome) => outcome,
        };
        if outcome
            .as_ref()
            .is_err_and(|error| error.code == "commit_indeterminate")
        {
            metrics.commit_failures.fetch_add(1, Ordering::Relaxed);
        }
        if reply.send(outcome).is_err() {
            counters
                .disconnected_replies
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Conservative retained-input charge, including tree nodes and string capacities.
pub(super) fn record_bytes(
    vector_capacity: usize,
    payload_capacity: usize,
    metadata: &crate::Metadata,
) -> usize {
    vector_capacity
        .saturating_mul(4)
        .saturating_add(payload_capacity)
        .saturating_add(metadata.iter().fold(1024usize, |bytes, (key, value)| {
            bytes
                .saturating_add(256)
                .saturating_add(key.capacity())
                .saturating_add(match value {
                    crate::MetadataValue::String(value) => value.capacity(),
                    _ => 0,
                })
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(path: &std::path::Path, max_requests: usize) -> Service {
        Service::new(Database::create(path, 1).unwrap())
            .with_group_commit(GroupCommitConfig {
                max_requests,
                max_delay: Duration::from_millis(100),
                ..Default::default()
            })
            .unwrap()
    }

    async fn queued(service: &Service, expected: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while service
                .group_commit
                .as_ref()
                .unwrap()
                .slots
                .available_permits()
                != expected
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn fifo_validation_atomicity_and_recovery() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let path = directory.path().join("group.vlite");
        let service = service(&path, 3);
        let first = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .run_mutation(1024, |db| {
                        db.put(1, &[1.0], b"first")?;
                        Ok(db.sequence)
                    })
                    .await
            })
        };
        queued(&service, 2).await;
        let invalid = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .run_mutation(1024, |db| {
                        db.write_batch(&[
                            Mutation::Put {
                                id: 9,
                                vector: vec![9.0],
                                payload: vec![],
                            },
                            Mutation::Put {
                                id: 10,
                                vector: vec![1.0, 2.0],
                                payload: vec![],
                            },
                        ])?;
                        Ok(())
                    })
                    .await
            })
        };
        queued(&service, 1).await;
        let last = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .run_mutation(1024, |db| {
                        assert_eq!(db.get(1)?.unwrap().payload, b"first");
                        db.put_in(
                            0,
                            1,
                            &[2.0],
                            b"last",
                            &crate::Metadata::new(),
                            Expected::Revision(1),
                        )?;
                        Ok(db.sequence)
                    })
                    .await
            })
        };
        assert_eq!(first.await.unwrap().unwrap(), 1);
        assert_eq!(invalid.await.unwrap().unwrap_err().code, "invalid_input");
        assert_eq!(last.await.unwrap().unwrap(), 2);
        assert_eq!(
            service.metrics.group_commit_groups.load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            service
                .metrics
                .group_commit_requests
                .load(Ordering::Relaxed),
            3
        );
        let stats = service.group_commit.as_ref().unwrap().stats();
        assert_eq!(stats.sync_count, 1);
        assert_eq!(stats.sync_requests, 2);
        assert_eq!(stats.sync_failures, 0);
        assert_eq!(stats.queue_wait_count, 3);
        assert!(stats.queue_wait_ns >= stats.queue_wait_max_ns);
        assert!(stats.sync_ns >= stats.sync_max_ns);
        service.close().await.unwrap();
        let stats = service.group_commit.as_ref().unwrap().stats();
        assert_eq!(stats.admitted_requests, 0);
        assert_eq!(stats.admitted_bytes, 0);
        assert_eq!(stats.queued_requests, 0);
        assert_eq!(stats.peak_requests, 3);
        assert_eq!(stats.peak_bytes, 3072);
        let db = Database::open(path).unwrap();
        assert_eq!(db.get(1).unwrap().unwrap().payload, b"last");
        assert!(db.get(9).unwrap().is_none());
        assert!(db.get(10).unwrap().is_none());
    }

    #[tokio::test]
    async fn no_ack_or_read_visibility_before_group_finishes() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let path = directory.path().join("visibility.vlite");
        let service = service(&path, 2);
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .run_mutation(1024, |db| {
                        db.put(1, &[1.0], b"")?;
                        Ok(())
                    })
                    .await
            })
        };
        queued(&service, 1).await;
        let last = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .run_mutation(1024, move |db| {
                        db.put(2, &[2.0], b"")?;
                        started_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Ok(())
                    })
                    .await
            })
        };
        started_rx.await.unwrap();
        assert!(!first.is_finished());
        assert!(!last.is_finished());
        assert!(service.database.try_read().is_err());
        release_tx.send(()).unwrap();
        first.await.unwrap().unwrap();
        last.await.unwrap().unwrap();
        service.close().await.unwrap();
    }

    #[tokio::test]
    async fn rest_preconditions_and_invalid_atomic_batch_are_independent() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;
        let directory = tempfile::tempdir_in(".").unwrap();
        let path = directory.path().join("rest-group.vlite");
        let mut db = Database::create(&path, 1).unwrap();
        db.put(1, &[0.0], b"").unwrap();
        let service = Service::new(db)
            .with_group_commit(GroupCommitConfig {
                max_requests: 3,
                max_delay: Duration::from_millis(100),
                ..Default::default()
            })
            .unwrap();
        let app = router(service.clone());
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/vectors/1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let tag = response.headers()[header::ETAG].clone();
        let mut calls = Vec::new();
        for value in [1, 2] {
            let app = app.clone();
            let tag = tag.clone();
            calls.push(tokio::spawn(async move {
                app.oneshot(
                    Request::builder()
                        .method("PUT")
                        .uri("/v1/vectors/1")
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(header::IF_MATCH, tag)
                        .body(Body::from(json!({"vector":[value]}).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
            }));
            queued(&service, 3 - calls.len()).await;
        }
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/batches")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({"operations":[
                            {"op":"put","id":"9","vector":[9]},
                            {"op":"put","id":"10","vector":[1,2]}
                        ]})
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(calls.remove(0).await.unwrap(), StatusCode::NO_CONTENT);
        assert_eq!(
            calls.remove(0).await.unwrap(),
            StatusCode::PRECONDITION_FAILED
        );
        service.close().await.unwrap();
        let db = Database::open(path).unwrap();
        assert_eq!(db.get(1).unwrap().unwrap().vector, vec![1.0]);
        assert!(db.get(9).unwrap().is_none());
        assert!(db.get(10).unwrap().is_none());
    }

    #[tokio::test]
    async fn detached_read_retains_snapshot_but_releases_owner_for_writer() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let path = directory.path().join("snapshot-writer.vlite");
        let service = service(&path, 1);
        service
            .run_mutation(1024, |db| {
                db.put(1, &[1.0], b"old")?;
                Ok(())
            })
            .await
            .unwrap();
        let (captured_tx, captured_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let read = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .run_read_then(
                        |db| Ok(db.read_snapshot()?),
                        move |snapshot| {
                            captured_tx.send(()).unwrap();
                            release_rx.recv().unwrap();
                            assert_eq!(snapshot.get(1)?.unwrap().payload, b"old");
                            Ok(())
                        },
                    )
                    .await
            })
        };
        captured_rx.await.unwrap();
        service
            .run_mutation(1024, |db| {
                db.put(1, &[2.0], b"new")?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(!read.is_finished());
        release_tx.send(()).unwrap();
        read.await.unwrap().unwrap();
        service.close().await.unwrap();
    }

    #[tokio::test]
    async fn expiry_put_and_metadata_patch_share_a_durable_group() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;
        let directory = tempfile::tempdir_in(".").unwrap();
        let path = directory.path().join("metadata-group.vlite");
        let service =
            Service::new(Database::create_with_format(&path, 1, crate::FormatVersion::V5).unwrap())
                .with_group_commit(GroupCommitConfig {
                    max_requests: 2,
                    max_delay: Duration::from_millis(100),
                    ..Default::default()
                })
                .unwrap();
        let expiry = crate::current_time_ms() + 60_000;
        let first = {
            let service = service.clone();
            tokio::spawn(async move {
                put_core(
                    &service,
                    admin::Role::Admin,
                    Target::Legacy,
                    1,
                    Guard::Expected(Expected::Any),
                    vec![1.0],
                    vec![],
                    crate::Metadata::new(),
                    Some(expiry),
                )
                .await
                .map(|_| ())
            })
        };
        queued(&service, 1).await;
        let response = router(service.clone())
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/v1/vectors/1/metadata")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"metadata":{"tag":{"type":"string","value":"updated"}}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        first.await.unwrap().unwrap();
        assert_eq!(
            service.metrics.group_commit_groups.load(Ordering::Relaxed),
            1
        );
        service.close().await.unwrap();
        let db = Database::open(path).unwrap();
        assert_eq!(
            db.get(1).unwrap().unwrap().metadata["tag"],
            crate::MetadataValue::String("updated".into())
        );
        assert_eq!(db.state(0).expiries[&1], expiry);
    }

    #[tokio::test]
    async fn exact_search_falls_back_when_snapshot_leases_are_exhausted() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;
        let directory = tempfile::tempdir_in(".").unwrap();
        let path = directory.path().join("snapshot-fallback.vlite");
        let mut db = Database::create(path, 1).unwrap();
        db.put(1, &[1.0], b"").unwrap();
        let service = Service::new(db);
        let snapshots: Vec<_> = {
            let owner = service.database.read().unwrap();
            let db = owner.as_ref().unwrap();
            (0..8).map(|_| db.read_snapshot().unwrap()).collect()
        };
        let response = router(service.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/search")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"vector":[1],"k":1,"explain":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["hits"][0]["id"], "1");
        assert_eq!(body["stats"]["records_scanned"], 1);
        drop(snapshots);
        service.close().await.unwrap();
    }

    #[tokio::test]
    async fn exact_rest_snapshot_matches_locked_explain_filters_ttl_and_errors() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;

        async fn search(app: Router, uri: &str, query: &Value) -> (StatusCode, Value) {
            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(query.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let body = http_body_util::BodyExt::collect(response.into_body())
                .await
                .unwrap()
                .to_bytes();
            (status, serde_json::from_slice(&body).unwrap())
        }

        let directory = tempfile::tempdir_in(".").unwrap();
        let path = directory.path().join("exact-differential.vlite");
        let mut db = Database::create_with_format(&path, 2, crate::FormatVersion::V5).unwrap();
        let now = crate::current_time_ms();
        for (id, vector, expiry) in [
            (1, vec![0.0, 0.0], None),
            (2, vec![1.0, 0.0], None),
            (3, vec![0.0, 1.0], Some(now - 1)),
            (4, vec![2.0, 0.0], Some(now + 3_600_000)),
        ] {
            let metadata = BTreeMap::from([
                (
                    "kind".into(),
                    crate::MetadataValue::String(if id == 1 { "zero" } else { "unit" }.into()),
                ),
                ("rank".into(), crate::MetadataValue::Integer(id as i64)),
                (
                    "place".into(),
                    crate::MetadataValue::GeoPoint {
                        latitude: 47.6,
                        longitude: -122.3,
                    },
                ),
            ]);
            db.put_with_expiry_in(0, id, &vector, b"", &metadata, expiry, Expected::Any)
                .unwrap();
        }
        let service = Service::new(db);
        let app = router(service.clone());
        let queries = vec![
            json!({"vector":[0,0],"k":10,"explain":true}),
            json!({"vector":[1,0],"k":10,"metric":"cosine","explain":true}),
            json!({"vector":[1,0],"k":10,"metric":"inner_product","explain":true}),
            json!({"vector":[0,0],"k":0,"explain":true}),
            json!({"vector":[1,0],"k":10,"filter":[{"op":"eq","field":"kind","value":{"type":"string","value":"unit"}}],"explain":true}),
            json!({"vector":[1,0],"k":10,"filter":[{"op":"range","field":"rank","min":{"type":"integer","value":"2"},"max":{"type":"integer","value":"4"}}],"explain":true}),
            json!({"vector":[1,0],"k":10,"filter":[{"op":"eq","field":"missing","value":{"type":"boolean","value":true}}],"explain":true}),
            json!({"vector":[1,0],"k":10,"filter":[{"op":"geo_radius","field":"place","latitude":47.6,"longitude":-122.3,"radius_meters":100}],"explain":true}),
            json!({"vector":[1,0],"k":10,"filter":[{"op":"geo_radius","field":"place","latitude":47.6,"longitude":-122.3,"radius_meters":-1}],"explain":true}),
            json!({"vector":[0,0],"k":10,"metric":"cosine","explain":true}),
            json!({"vector":[1],"k":10,"explain":true}),
            json!({"vector":[1],"k":0,"explain":true}),
            json!({"vector":[1,0],"k":10,"filter":[{"op":"range","field":"rank","min":{"type":"integer","value":"4"},"max":{"type":"integer","value":"2"}}],"explain":true}),
            json!({"vector":[1,0],"k":10,"filter":[{"op":"eq","field":"","value":{"type":"boolean","value":true}}],"explain":true}),
            json!({"vector":[1,0],"k":10,"ef_search":10,"explain":true}),
        ];
        for uri in ["/v1/search", "/v1/collections/default/search"] {
            for query in &queries {
                let fast = search(app.clone(), uri, query).await;
                let captures: Vec<_> = {
                    let owner = service.database.read().unwrap();
                    (0..8)
                        .map(|_| owner.as_ref().unwrap().read_snapshot().unwrap())
                        .collect()
                };
                let locked = search(app.clone(), uri, query).await;
                drop(captures);
                assert_eq!(fast, locked, "snapshot/locked mismatch for {uri}: {query}");
                if fast.0 == StatusCode::OK {
                    assert!(fast.1["plan"].is_null(), "legacy exact plan remains null");
                    assert!(fast.1["stats"].is_object());
                    assert!(
                        fast.1["hits"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .all(|hit| hit["id"] != "3"),
                        "expired record must not appear"
                    );
                }
            }
        }
        let all = search(app, "/v1/search", &queries[0]).await;
        assert_eq!(all.1["hits"].as_array().unwrap().len(), 3);
        assert_eq!(all.1["stats"]["records_scanned"], 4);
        service.close().await.unwrap();
    }

    #[tokio::test]
    async fn disconnected_request_drains_on_shutdown_and_rejects_new_work() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let path = directory.path().join("shutdown.vlite");
        let service = service(&path, 2);
        let request = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .run_mutation(1024, |db| {
                        db.put(1, &[1.0], b"disconnected")?;
                        Ok(())
                    })
                    .await
            })
        };
        queued(&service, 1).await;
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        service.close().await.unwrap();
        assert_eq!(service.workers.available_permits(), MAX_IN_FLIGHT);
        let stats = service.group_commit.as_ref().unwrap().stats();
        assert_eq!(stats.disconnected_replies, 1);
        assert_eq!(stats.admitted_requests, 0);
        assert_eq!(stats.admitted_bytes, 0);
        let error = service.run_mutation(1024, |_| Ok(())).await.unwrap_err();
        assert_eq!(error.code, "unavailable");
        assert_eq!(
            service
                .group_commit
                .as_ref()
                .unwrap()
                .stats()
                .rejected_closed,
            1
        );
        let db = Database::open(path).unwrap();
        assert_eq!(db.get(1).unwrap().unwrap().payload, b"disconnected");
    }

    #[tokio::test]
    async fn byte_and_request_backpressure_are_immediate_and_release_permits() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let path = directory.path().join("bounded.vlite");
        let service = Service::new(Database::create(&path, 1).unwrap())
            .with_group_commit(GroupCommitConfig {
                max_requests: 2,
                max_bytes: 1024,
                max_delay: Duration::from_millis(100),
            })
            .unwrap();
        let error = service.run_mutation(1025, |_| Ok(())).await.unwrap_err();
        assert_eq!(error.code, "busy");
        assert_eq!(service.workers.available_permits(), MAX_IN_FLIGHT);
        let first = {
            let service = service.clone();
            tokio::spawn(async move { service.run_mutation(1024, |_| Ok(())).await })
        };
        queued(&service, 1).await;
        let error = service.run_mutation(1, |_| Ok(())).await.unwrap_err();
        assert_eq!(error.code, "busy");
        first.await.unwrap().unwrap();
        let group = service.group_commit.as_ref().unwrap();
        let slots = group.slots.clone().acquire_many_owned(2).await.unwrap();
        assert_eq!(
            service.run_mutation(1, |_| Ok(())).await.unwrap_err().code,
            "busy"
        );
        drop(slots);
        service.close().await.unwrap();
        assert_eq!(service.workers.available_permits(), MAX_IN_FLIGHT);
        let stats = group.stats();
        assert_eq!(stats.rejected_bytes, 2);
        assert_eq!(stats.rejected_requests, 1);
        assert_eq!(stats.admitted_bytes, 0);
        assert_eq!(stats.admitted_requests, 0);
        assert_eq!(stats.sync_count, 0, "no-op groups do not issue a sync");
    }

    #[tokio::test]
    async fn append_failure_never_acknowledges_neighbor_and_fails_closed() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let path = directory.path().join("failed.vlite");
        let service = service(&path, 2);
        let first = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .run_mutation(1024, |db| {
                        db.put(1, &[1.0], b"")?;
                        Ok(())
                    })
                    .await
            })
        };
        queued(&service, 1).await;
        let disk_path = path.clone();
        let second = service
            .run_mutation(1024, move |db| {
                db.file =
                    crate::LockedFile::disk(std::fs::File::open(disk_path).map_err(internal)?);
                db.put(2, &[2.0], b"")?;
                Ok(())
            })
            .await
            .unwrap_err();
        assert_eq!(second.code, "commit_indeterminate");
        assert_eq!(first.await.unwrap().unwrap_err().code, "poisoned");
        assert_eq!(
            service
                .run_mutation(1024, |_| Ok(()))
                .await
                .unwrap_err()
                .code,
            "poisoned"
        );
        assert!(service.close().await.is_err());
    }

    #[tokio::test]
    async fn shared_sync_failure_replaces_every_success_but_not_validation_errors() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let path = directory.path().join("sync-failed.vlite");
        let service = service(&path, 3);
        service
            .database
            .write()
            .unwrap()
            .as_mut()
            .unwrap()
            .fail_next_group_sync = true;
        let first = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .run_mutation(1024, |db| {
                        db.put(1, &[1.0], b"")?;
                        Ok(())
                    })
                    .await
            })
        };
        queued(&service, 2).await;
        let invalid = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .run_mutation(1024, |db| {
                        db.put(9, &[1.0, 2.0], b"")?;
                        Ok(())
                    })
                    .await
            })
        };
        queued(&service, 1).await;
        let last = service
            .run_mutation(1024, |db| {
                db.put(2, &[1.0], b"")?;
                Ok(())
            })
            .await
            .unwrap_err();
        assert_eq!(
            first.await.unwrap().unwrap_err().code,
            "commit_indeterminate"
        );
        assert_eq!(last.code, "commit_indeterminate");
        assert_eq!(invalid.await.unwrap().unwrap_err().code, "invalid_input");
        assert_eq!(service.metrics.commit_failures.load(Ordering::Relaxed), 2);
        let stats = service.group_commit.as_ref().unwrap().stats();
        assert_eq!(stats.sync_count, 1);
        assert_eq!(stats.sync_requests, 2);
        assert_eq!(stats.sync_failures, 1);
        assert_eq!(
            service
                .run_mutation(1024, |_| Ok(()))
                .await
                .unwrap_err()
                .code,
            "poisoned"
        );
        assert!(service.close().await.is_err());
        let stats = service.group_commit.as_ref().unwrap().stats();
        assert_eq!(stats.admitted_requests, 0);
        assert_eq!(stats.admitted_bytes, 0);
        let db = Database::open(path).unwrap();
        // A failed sync is explicitly uncertain, not a promise to roll back.
        for id in [1, 2] {
            if let Some(record) = db.get(id).unwrap() {
                assert_eq!(record.vector, vec![1.0]);
            }
        }
        assert!(db.get(9).unwrap().is_none());
    }

    #[test]
    fn short_disk_appends_preserve_acknowledged_prefix_and_atomic_batches() {
        use std::io::{Seek, Write};

        struct ShortWriter {
            file: std::fs::File,
            remaining: usize,
            zero: bool,
        }
        impl Write for ShortWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.remaining == 0 {
                    return if self.zero {
                        Ok(0)
                    } else {
                        Err(std::io::Error::other("injected disk-full boundary"))
                    };
                }
                let written = self.file.write(&bytes[..bytes.len().min(1)])?;
                self.remaining -= written;
                Ok(written)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.file.flush()
            }
        }
        impl Seek for ShortWriter {
            fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
                self.file.seek(position)
            }
        }

        let directory = tempfile::tempdir_in(".").unwrap();
        let source = directory.path().join("source.vlite");
        let mut db = Database::create(&source, 1).unwrap();
        db.put(1, &[1.0], b"acknowledged").unwrap();
        let end = std::fs::metadata(&source).unwrap().len();
        db.write_batch(&[
            Mutation::Put {
                id: 2,
                vector: vec![2.0],
                payload: vec![],
            },
            Mutation::Put {
                id: 3,
                vector: vec![3.0],
                payload: vec![],
            },
        ])
        .unwrap();
        db.close().unwrap();
        let bytes = std::fs::read(source).unwrap();
        let frame = &bytes[end as usize..];
        for zero in [false, true] {
            for cut in 0..=frame.len() {
                let path = directory.path().join("partial.vlite");
                std::fs::write(&path, &bytes[..end as usize]).unwrap();
                let mut writer = ShortWriter {
                    file: std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&path)
                        .unwrap(),
                    remaining: cut,
                    zero,
                };
                let synced = std::cell::Cell::new(false);
                let outcome = crate::append_and_sync(&mut writer, end, frame, |writer| {
                    writer.file.sync_all()?;
                    synced.set(true);
                    Ok(())
                });
                assert_eq!(outcome.is_ok(), cut == frame.len());
                assert_eq!(
                    synced.get(),
                    cut == frame.len(),
                    "never sync a failed append"
                );
                if zero && cut < frame.len() {
                    assert_eq!(outcome.unwrap_err().kind(), std::io::ErrorKind::WriteZero);
                }
                drop(writer);
                if cut > 0 && cut < frame.len() {
                    let before_repair = std::fs::read(&path).unwrap();
                    assert!(matches!(
                        Database::open(&path),
                        Err(crate::Error::IncompleteTail { .. })
                    ));
                    assert_eq!(std::fs::read(&path).unwrap(), before_repair);
                    Database::discard_incomplete_tail(&path).unwrap();
                    assert_eq!(std::fs::metadata(&path).unwrap().len(), end);
                }
                let recovered = Database::open(&path).unwrap();
                assert_eq!(recovered.get(1).unwrap().unwrap().payload, b"acknowledged");
                for id in [2, 3] {
                    assert_eq!(
                        recovered.get(id).unwrap().is_some(),
                        cut == frame.len(),
                        "atomic batch cut={cut}, id={id}, zero={zero}"
                    );
                }
                recovered.close().unwrap();
            }
        }
    }

    #[test]
    #[ignore = "subprocess fixture: invoked by grouped_crash_recovery_retains_every_acknowledgement"]
    fn group_commit_crash_child() {
        use std::io::Write;
        let path = std::env::var_os("VLITE_GROUP_CRASH_PATH").unwrap();
        let phase = std::env::var("VLITE_GROUP_CRASH_PHASE").unwrap();
        let format = match std::env::var("VLITE_GROUP_CRASH_FORMAT").unwrap().as_str() {
            "v3" => crate::FormatVersion::V3,
            "v4" => crate::FormatVersion::V4,
            "v5" => crate::FormatVersion::V5,
            other => panic!("unexpected crash fixture format: {other}"),
        };
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let service = Service::new(Database::create_with_format(&path, 1, format).unwrap())
                .with_group_commit(GroupCommitConfig {
                    max_requests: 4,
                    max_delay: Duration::from_millis(100),
                    ..Default::default()
                })
                .unwrap();
            let mut requests = Vec::new();
            for id in 1..=4 {
                let service = service.clone();
                requests.push(tokio::spawn(async move {
                    service
                        .run_mutation(1024, move |db| {
                            db.put(id, &[id as f32], b"acknowledged")?;
                            Ok(id)
                        })
                        .await
                }));
            }
            for request in requests {
                let id = request.await.unwrap().unwrap();
                println!("ACK {id}");
            }
            std::io::stdout().flush().unwrap();
            if phase == "after_ack" {
                std::process::exit(23);
            }
            service
                .run_mutation(1024, |db| {
                    db.write_batch(&[
                        Mutation::Put {
                            id: 5,
                            vector: vec![5.0],
                            payload: vec![],
                        },
                        Mutation::Put {
                            id: 6,
                            vector: vec![6.0],
                            payload: vec![],
                        },
                    ])?;
                    // Exit the actual coordinator after append, before durable_group sync/reply.
                    std::process::exit(23);
                    #[allow(unreachable_code)]
                    Ok(())
                })
                .await
                .unwrap();
        });
        panic!("crash fixture did not exit");
    }

    #[test]
    fn grouped_crash_recovery_retains_every_acknowledgement() {
        for (format, phase) in ["v3", "v4", "v5"]
            .into_iter()
            .flat_map(|format| ["after_ack", "before_sync"].map(|phase| (format, phase)))
        {
            let directory = tempfile::tempdir_in(".").unwrap();
            let path = directory.path().join("crash.vlite");
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "server::group_commit::tests::group_commit_crash_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("VLITE_GROUP_CRASH_PATH", &path)
                .env("VLITE_GROUP_CRASH_PHASE", phase)
                .env("VLITE_GROUP_CRASH_FORMAT", format)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(23), "{output:?}");
            let stdout = String::from_utf8(output.stdout).unwrap();
            let acknowledged: Vec<u64> = stdout
                .lines()
                .filter_map(|line| line.strip_prefix("ACK "))
                .map(|id| id.parse().unwrap())
                .collect();
            assert_eq!(acknowledged, vec![1, 2, 3, 4]);
            let mut db = Database::open(path).unwrap();
            for id in acknowledged {
                let record = db.get(id).unwrap().unwrap();
                assert_eq!(record.payload, b"acknowledged");
                assert_eq!(record.vector, vec![id as f32]);
            }
            let uncertain_batch = db.get(5).unwrap().is_some();
            assert_eq!(db.get(6).unwrap().is_some(), uncertain_batch);
            if phase == "after_ack" {
                assert!(!uncertain_batch);
            }
            db.put(7, &[7.0], b"after recovery").unwrap();
            db.close().unwrap();
        }
    }

    #[tokio::test]
    async fn active_disconnect_overload_and_shutdown_drain_keep_admission_bounded() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let path = directory.path().join("active-drain.vlite");
        let service = Service::new(Database::create(&path, 1).unwrap())
            .with_group_commit(GroupCommitConfig {
                max_requests: 1,
                max_bytes: 1024,
                max_delay: Duration::ZERO,
            })
            .unwrap();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let request = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .run_mutation(1024, move |db| {
                        db.put(1, &[1.0], b"disconnected active writer")?;
                        started_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Ok(())
                    })
                    .await
            })
        };
        started_rx.await.unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        let stats = service.group_commit.as_ref().unwrap().stats();
        assert_eq!(stats.admitted_requests, 1);
        assert_eq!(stats.admitted_bytes, 1024);
        assert_eq!(stats.queued_requests, 0);
        for _ in 0..32 {
            assert_eq!(
                service.run_mutation(1, |_| Ok(())).await.unwrap_err().code,
                "busy"
            );
        }
        let closing = {
            let service = service.clone();
            tokio::spawn(async move { service.close().await })
        };
        tokio::task::yield_now().await;
        assert!(
            !closing.is_finished(),
            "shutdown must wait for the active durability group"
        );
        release_tx.send(()).unwrap();
        closing.await.unwrap().unwrap();
        let stats = service.group_commit.as_ref().unwrap().stats();
        assert_eq!(stats.admitted_requests, 0);
        assert_eq!(stats.admitted_bytes, 0);
        assert_eq!(stats.disconnected_replies, 1);
        assert_eq!(stats.rejected_requests, 32);
        assert_eq!(stats.sync_count, 1);
        assert_eq!(service.workers.available_permits(), MAX_IN_FLIGHT);
        assert_eq!(
            Database::open(path)
                .unwrap()
                .get(1)
                .unwrap()
                .unwrap()
                .payload,
            b"disconnected active writer"
        );
    }

    #[tokio::test]
    async fn disconnected_sync_failure_drains_and_preserves_atomic_validation() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let path = directory.path().join("failed-drain.vlite");
        let mut db = Database::create(&path, 1).unwrap();
        db.put(0, &[0.0], b"previously acknowledged").unwrap();
        let service = Service::new(db)
            .with_group_commit(GroupCommitConfig {
                max_requests: 3,
                max_bytes: 3072,
                max_delay: Duration::from_millis(100),
            })
            .unwrap();
        service
            .database
            .write()
            .unwrap()
            .as_mut()
            .unwrap()
            .fail_next_group_sync = true;
        let disconnected = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .run_mutation(1024, |db| {
                        db.put(1, &[1.0], b"uncertain")?;
                        Ok(())
                    })
                    .await
            })
        };
        queued(&service, 2).await;
        let invalid = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .run_mutation(1024, |db| {
                        db.write_batch(&[
                            Mutation::Put {
                                id: 9,
                                vector: vec![9.0],
                                payload: vec![],
                            },
                            Mutation::Put {
                                id: 10,
                                vector: vec![1.0, 2.0],
                                payload: vec![],
                            },
                        ])?;
                        Ok(())
                    })
                    .await
            })
        };
        queued(&service, 1).await;
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let last = {
            let service = service.clone();
            tokio::spawn(async move {
                service
                    .run_mutation(1024, move |db| {
                        db.put(2, &[2.0], b"uncertain")?;
                        started_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Ok(())
                    })
                    .await
            })
        };
        started_rx.await.unwrap();
        disconnected.abort();
        assert!(disconnected.await.unwrap_err().is_cancelled());
        assert_eq!(
            service.run_mutation(1, |_| Ok(())).await.unwrap_err().code,
            "busy"
        );
        let closing = {
            let service = service.clone();
            tokio::spawn(async move { service.close().await })
        };
        tokio::task::yield_now().await;
        assert!(!closing.is_finished());
        release_tx.send(()).unwrap();
        assert_eq!(invalid.await.unwrap().unwrap_err().code, "invalid_input");
        assert_eq!(
            last.await.unwrap().unwrap_err().code,
            "commit_indeterminate"
        );
        assert!(closing.await.unwrap().is_err());
        let stats = service.group_commit.as_ref().unwrap().stats();
        assert_eq!(stats.sync_count, 1);
        assert_eq!(stats.sync_requests, 2);
        assert_eq!(stats.sync_failures, 1);
        assert_eq!(stats.disconnected_replies, 1);
        assert_eq!(stats.admitted_requests, 0);
        assert_eq!(stats.admitted_bytes, 0);
        assert_eq!(stats.queued_requests, 0);
        assert_eq!(service.workers.available_permits(), MAX_IN_FLIGHT);
        let recovered = Database::open(path).unwrap();
        assert_eq!(
            recovered.get(0).unwrap().unwrap().payload,
            b"previously acknowledged"
        );
        assert!(recovered.get(9).unwrap().is_none());
        assert!(recovered.get(10).unwrap().is_none());
        for (id, vector) in [(1, 1.0), (2, 2.0)] {
            if let Some(record) = recovered.get(id).unwrap() {
                assert_eq!(record.vector, vec![vector]);
            }
        }
    }

    #[test]
    fn invalid_limits_and_late_configuration_fail_closed() {
        for config in [
            GroupCommitConfig {
                max_requests: 0,
                ..Default::default()
            },
            GroupCommitConfig {
                max_requests: 17,
                ..Default::default()
            },
            GroupCommitConfig {
                max_bytes: 0,
                ..Default::default()
            },
            GroupCommitConfig {
                max_bytes: 256 * 1024 * 1024 + 1,
                ..Default::default()
            },
            GroupCommitConfig {
                max_delay: Duration::from_millis(101),
                ..Default::default()
            },
        ] {
            assert!(config.validate().is_err());
        }
        let directory = tempfile::tempdir_in(".").unwrap();
        let service =
            Service::new(Database::create(directory.path().join("late.vlite"), 1).unwrap());
        let clone = service.clone();
        assert!(service.with_group_commit(Default::default()).is_err());
        drop(clone);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "bounded local-disk comparative benchmark"]
    async fn comparative_durable_write_benchmark() {
        const WRITES: u64 = 512;
        for concurrency in [1, 16] {
            for delay in [None, Some(Duration::ZERO), Some(Duration::from_millis(2))] {
                let directory = tempfile::tempdir_in(".").unwrap();
                let path = directory.path().join("bench.vlite");
                let service = Service::new(Database::create(&path, 16).unwrap());
                let service = if let Some(max_delay) = delay {
                    service
                        .with_group_commit(GroupCommitConfig {
                            max_delay,
                            ..Default::default()
                        })
                        .unwrap()
                } else {
                    service
                };
                let started = Instant::now();
                let mut latencies = Vec::with_capacity(WRITES as usize);
                for base in (0..WRITES).step_by(concurrency) {
                    let mut tasks = Vec::new();
                    for id in base..base + concurrency as u64 {
                        let worker = service.clone();
                        tasks.push(tokio::spawn(async move {
                            let started = Instant::now();
                            put_core(
                                &worker,
                                admin::Role::Admin,
                                Target::Legacy,
                                id,
                                Guard::Expected(Expected::Any),
                                vec![id as f32; 16],
                                vec![0; 128],
                                crate::Metadata::new(),
                                None,
                            )
                            .await
                            .unwrap();
                            started.elapsed().as_secs_f64() * 1000.0
                        }));
                    }
                    for task in tasks {
                        latencies.push(task.await.unwrap());
                    }
                }
                let elapsed = started.elapsed();
                latencies.sort_by(f64::total_cmp);
                eprintln!(
                    "delay_ms={:?} writes={WRITES} concurrency={concurrency} elapsed_ms={:.3} writes_per_sec={:.1} groups={} p50_ms={:.3} p95_ms={:.3} p99_ms={:.3}",
                    delay.map(|delay| delay.as_millis()),
                    elapsed.as_secs_f64() * 1000.0,
                    WRITES as f64 / elapsed.as_secs_f64(),
                    service.metrics.group_commit_groups.load(Ordering::Relaxed),
                    latencies[latencies.len() / 2],
                    latencies[latencies.len() * 95 / 100],
                    latencies[latencies.len() * 99 / 100]
                );
                service.close().await.unwrap();
                assert_eq!(Database::open(path).unwrap().len(), WRITES as usize);
            }
        }
    }
}
