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

struct Job {
    operation: Work,
    reply: oneshot::Sender<Result<Output, ApiError>>,
    _worker: OwnedSemaphorePermit,
    _slot: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
}

pub(super) struct Coordinator {
    sender: Mutex<Option<mpsc::SyncSender<Job>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    slots: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    max_bytes: usize,
}

impl Coordinator {
    pub fn start(
        database: Weak<RwLock<Option<Database>>>,
        metrics: Arc<Metrics>,
        config: GroupCommitConfig,
    ) -> std::io::Result<Self> {
        config.validate()?;
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
                    commit(&database, &metrics, jobs);
                }
            })?;
        Ok(Self {
            sender: Mutex::new(Some(sender)),
            thread: Mutex::new(Some(thread)),
            slots: Arc::new(Semaphore::new(config.max_requests)),
            bytes: Arc::new(Semaphore::new(config.max_bytes)),
            max_bytes: config.max_bytes,
        })
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
            return Err(busy());
        }
        let slot = self.slots.clone().try_acquire_owned().map_err(|_| busy())?;
        let bytes = self
            .bytes
            .clone()
            .try_acquire_many_owned(bytes as u32)
            .map_err(|_| busy())?;
        let (reply, receive) = oneshot::channel();
        let job = Job {
            operation: Box::new(move |db| operation(db).map(|value| Box::new(value) as Output)),
            reply,
            _worker: worker,
            _slot: slot,
            _bytes: bytes,
        };
        self.sender
            .lock()
            .map_err(internal)?
            .as_ref()
            .ok_or_else(unavailable)?
            .try_send(job)
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => busy(),
                mpsc::TrySendError::Disconnected(_) => unavailable(),
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

fn commit(database: &Weak<RwLock<Option<Database>>>, metrics: &Metrics, jobs: Vec<Job>) {
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
        db.durable_group(|db| {
            for job in pending.take().into_iter().flatten() {
                let Job {
                    operation,
                    reply,
                    _worker,
                    _slot,
                    _bytes,
                } = job;
                completed.push((reply, operation(db), _worker, _slot, _bytes));
            }
        })
        .map_err(ApiError::from)
    })();
    if let Some(jobs) = pending {
        for job in jobs {
            let error = match &result {
                Err(error) => error.clone(),
                Ok(()) => internal("durability group skipped queued writes"),
            };
            let _ = job.reply.send(Err(error));
        }
    }
    for (reply, outcome, _worker, _slot, _bytes) in completed {
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
        let _ = reply.send(outcome);
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
        service.close().await.unwrap();
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
        let error = service.run_mutation(1024, |_| Ok(())).await.unwrap_err();
        assert_eq!(error.code, "unavailable");
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
